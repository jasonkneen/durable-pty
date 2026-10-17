use crate::mcp::{self, Backend, Local};
use crate::proto::Request;
use crate::rpc;
use crate::session::{Frame, Manager};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, Request as HttpRequest, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tokio_stream::wrappers::ReceiverStream;

type Sessions = Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>;

#[derive(Clone)]
struct App {
    mgr: Option<Arc<Manager>>,
    backend: Arc<dyn Backend>,
    token: Option<String>,
    sse: Sessions,
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(app): State<App>, headers: HeaderMap, req: HttpRequest<axum::body::Body>, next: Next) -> Response {
    let Some(want) = &app.token else { return next.run(req).await };
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(String::from);
    let query = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=").map(String::from)));
    match bearer.or(query) {
        Some(t) if ct_eq(&t, want) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response(),
    }
}

fn mcp_routes(r: Router<App>) -> Router<App> {
    r.route("/mcp", post(mcp_post).get(|| async { StatusCode::METHOD_NOT_ALLOWED }))
        .route("/sse", get(sse_open))
        .route("/messages", post(sse_message))
}

fn finish(app: App, r: Router<App>) -> Router {
    r.layer(middleware::from_fn_with_state(app.clone(), auth)).with_state(app)
}

pub fn daemon_router(mgr: Arc<Manager>, token: String) -> Router {
    let app = App { mgr: Some(mgr.clone()), backend: Arc::new(Local(mgr)), token: Some(token), sse: Default::default() };
    let r = Router::new().route("/health", get(|| async { Json(json!({"ok": true})) })).route("/rpc", post(rpc_post)).route("/ws", get(ws_upgrade));
    finish(app.clone(), mcp_routes(r))
}

pub fn mcp_router(token: Option<String>) -> Router {
    let app = App { mgr: None, backend: Arc::new(mcp::Remote), token, sse: Default::default() };
    let r = Router::new().route("/health", get(|| async { Json(json!({"ok": true})) }));
    finish(app.clone(), mcp_routes(r))
}

async fn rpc_post(State(app): State<App>, Json(req): Json<Request>) -> Json<Value> {
    let mgr = app.mgr.as_ref().expect("daemon router");
    let r = rpc::dispatch(mgr, &req.method, req.params).await;
    Json(serde_json::to_value(crate::proto::Response::from(req.id, r)).unwrap_or(Value::Null))
}

async fn mcp_post(State(app): State<App>, Json(body): Json<Value>) -> Response {
    if let Value::Array(items) = body {
        let mut out = vec![];
        for m in items {
            if let Some(r) = mcp::handle(&*app.backend, m).await {
                out.push(r);
            }
        }
        return if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { Json(Value::Array(out)).into_response() };
    }
    match mcp::handle(&*app.backend, body).await {
        Some(r) => Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn sse_open(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let (tx, rx) = mpsc::channel::<String>(64);
    app.sse.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), tx);
    let mut endpoint = format!("/messages?sessionId={id}");
    if let Some(t) = q.get("token") {
        endpoint.push_str(&format!("&token={t}"));
    }
    let first = futures::stream::once(async move { Ok::<_, Infallible>(Event::default().event("endpoint").data(endpoint)) });
    let rest = ReceiverStream::new(rx).map(|s| Ok::<_, Infallible>(Event::default().event("message").data(s)));
    Sse::new(first.chain(rest)).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn sse_message(State(app): State<App>, Query(q): Query<HashMap<String, String>>, Json(body): Json<Value>) -> Response {
    let Some(id) = q.get("sessionId") else { return StatusCode::BAD_REQUEST.into_response() };
    let tx = app.sse.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned();
    let Some(tx) = tx else { return StatusCode::NOT_FOUND.into_response() };
    let backend = app.backend.clone();
    let sessions = app.sse.clone();
    let id = id.clone();
    tokio::spawn(async move {
        if let Some(r) = mcp::handle(&*backend, body).await {
            if tx.send(r.to_string()).await.is_err() {
                sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            }
        }
    });
    StatusCode::ACCEPTED.into_response()
}

async fn ws_upgrade(State(app): State<App>, Query(q): Query<HashMap<String, String>>, ws: WebSocketUpgrade) -> Response {
    let mgr = app.mgr.clone().expect("daemon router");
    let key = q.get("id").cloned().unwrap_or_default();
    let cols = q.get("cols").and_then(|v| v.parse().ok()).unwrap_or(0);
    let rows = q.get("rows").and_then(|v| v.parse().ok()).unwrap_or(0);
    ws.on_upgrade(move |socket| ws_session(mgr, socket, key, cols, rows))
}

/// Text frames carry JSON control ({"type":"resize","cols":..,"rows":..}); binary frames are raw terminal bytes both ways.
async fn ws_session(mgr: Arc<Manager>, mut socket: WebSocket, key: String, cols: u16, rows: u16) {
    let (live, snap, mut rx) = match mgr.attach(&key, cols, rows) {
        Ok(x) => x,
        Err(e) => {
            let _ = socket.send(Message::Text(json!({"type": "error", "message": e.to_string()}).to_string().into())).await;
            return;
        }
    };
    let info = mgr.describe(mgr.row(&live.id).unwrap_or_else(|_| panic!("attached session has a row")));
    if socket.send(Message::Text(json!({"type": "attached", "session": info}).to_string().into())).await.is_err()
        || socket.send(Message::Binary(snap.into())).await.is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            f = rx.recv() => match f {
                Ok(Frame::Data(d)) => if socket.send(Message::Binary(d.to_vec().into())).await.is_err() { return },
                Ok(Frame::Exit(c)) => {
                    let _ = socket.send(Message::Text(json!({"type": "exit", "code": c}).to_string().into())).await;
                    return;
                }
                Err(RecvError::Lagged(_)) => {
                    let (snap, nrx) = live.subscribe();
                    rx = nrx;
                    if socket.send(Message::Binary(snap.into())).await.is_err() { return }
                }
                Err(RecvError::Closed) => return,
            },
            m = socket.recv() => match m {
                Some(Ok(Message::Binary(b))) => { let _ = live.write_input(&b); }
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        match v["type"].as_str() {
                            Some("resize") => {
                                let _ = live.resize(v["cols"].as_u64().unwrap_or(0) as u16, v["rows"].as_u64().unwrap_or(0) as u16);
                            }
                            Some("input") => { let _ = live.write_input(v["data"].as_str().unwrap_or("").as_bytes()); }
                            _ => {}
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}
