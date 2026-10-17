use crate::paths;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub struct Client {
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
    next: u64,
}

impl Client {
    pub async fn connect() -> Result<Client> {
        let s = UnixStream::connect(paths::sock()).await?;
        let (r, w) = s.into_split();
        Ok(Client { reader: BufReader::new(r), writer: w, next: 1 })
    }

    /// Connects, starting the daemon in the background first when it is not running.
    pub async fn connect_or_start() -> Result<Client> {
        if let Ok(c) = Client::connect().await {
            return Ok(c);
        }
        spawn_daemon()?;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Ok(c) = Client::connect().await {
                return Ok(c);
            }
        }
        bail!("could not start the durable-pty daemon (see {})", paths::log_file().display())
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let mut line = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp).await? == 0 {
            bail!("daemon closed the connection");
        }
        let v: Value = serde_json::from_str(&resp)?;
        if v["ok"] == json!(true) {
            Ok(v["result"].clone())
        } else {
            Err(anyhow!(v["error"].as_str().unwrap_or("request failed").to_string()))
        }
    }
}

pub fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    paths::ensure_home()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log_file())?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("serve").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}
