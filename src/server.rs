use crate::proto::*;
use crate::rpc;
use crate::session::{Frame, Manager};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast::error::RecvError, mpsc};

pub async fn run_unix(mgr: Arc<Manager>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((s, _)) => {
                tokio::spawn(handle(mgr.clone(), s));
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
}

async fn handle(mgr: Arc<Manager>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_resp(&mut w, Response::from(Value::Null, Err(format!("bad request: {e}")))).await;
                continue;
            }
        };
        if req.method == "attach" {
            attach(mgr, reader, w, req).await;
            return;
        }
        let resp = Response::from(req.id, rpc::dispatch(&mgr, &req.method, req.params).await);
        if write_resp(&mut w, resp).await.is_err() {
            return;
        }
    }
}

async fn write_resp(w: &mut tokio::net::unix::OwnedWriteHalf, r: Response) -> std::io::Result<()> {
    let mut b = serde_json::to_vec(&r).unwrap_or_default();
    b.push(b'\n');
    w.write_all(&b).await
}

async fn attach(mgr: Arc<Manager>, mut reader: BufReader<tokio::net::unix::OwnedReadHalf>, mut w: tokio::net::unix::OwnedWriteHalf, req: Request) {
    let key = req.params.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let cols = req.params.get("cols").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    let rows = req.params.get("rows").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    let (live, snap, mut rx) = match mgr.attach(&key, cols, rows) {
        Ok(x) => x,
        Err(e) => {
            let _ = write_resp(&mut w, Response::from(req.id, Err(e.to_string()))).await;
            return;
        }
    };
    let info = mgr.describe(mgr.row(&live.id).unwrap_or_else(|_| unreachable_row()));
    if write_resp(&mut w, Response::from(req.id, Ok(json!({"session": info})))).await.is_err() {
        return;
    }
    if write_frame(&mut w, S_DATA, &snap).await.is_err() {
        return;
    }
    let (itx, mut irx) = mpsc::channel::<(u8, Vec<u8>)>(64);
    tokio::spawn(async move {
        while let Ok(f) = read_frame(&mut reader).await {
            if itx.send(f).await.is_err() {
                break;
            }
        }
    });
    loop {
        tokio::select! {
            f = rx.recv() => match f {
                Ok(Frame::Data(d)) => if write_frame(&mut w, S_DATA, &d).await.is_err() { return },
                Ok(Frame::Exit(c)) => { let _ = write_frame(&mut w, S_EXIT, &c.to_be_bytes()).await; return },
                Err(RecvError::Lagged(_)) => {
                    let (snap, nrx) = live.subscribe();
                    rx = nrx;
                    if write_frame(&mut w, S_DATA, &snap).await.is_err() { return }
                }
                Err(RecvError::Closed) => return,
            },
            c = irx.recv() => match c {
                Some((C_INPUT, d)) => { let _ = live.write_input(&d); }
                Some((C_RESIZE, d)) if d.len() == 4 => {
                    let _ = live.resize(u16::from_be_bytes([d[0], d[1]]), u16::from_be_bytes([d[2], d[3]]));
                }
                Some((C_DETACH, _)) | None => return,
                _ => {}
            },
        }
    }
}

fn unreachable_row() -> crate::model::SessionRow {
    unreachable!("attached session has a row")
}
