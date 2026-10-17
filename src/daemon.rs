use crate::client::Client;
use crate::db::Db;
use crate::session::Manager;
use crate::{http, paths, rpc, server};
use anyhow::{bail, Result};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::net::UnixListener;

pub async fn serve(http_addr: Option<String>) -> Result<()> {
    paths::ensure_home()?;
    if Client::connect().await.is_ok() {
        bail!("durable-pty is already running ({})", paths::sock().display());
    }
    let _ = std::fs::remove_file(paths::sock());
    let db = Arc::new(Db::open(&paths::db())?);
    let mgr = Manager::start(db);
    let listener = UnixListener::bind(paths::sock())?;
    std::fs::set_permissions(paths::sock(), std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(paths::pid_file(), std::process::id().to_string())?;
    mgr.recover();
    tokio::spawn(server::run_unix(mgr.clone(), listener));

    if let Some(addr) = http_addr {
        let l = tokio::net::TcpListener::bind(&addr).await?;
        let bound = l.local_addr()?.to_string();
        let _ = rpc::HTTP_ADDR.set(bound.clone());
        eprintln!("durable-pty: http/ws/mcp on http://{bound} (token in {})", paths::home().join("token").display());
        let app = http::daemon_router(mgr.clone(), paths::token());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(l, app).await {
                eprintln!("durable-pty: http server stopped: {e}");
            }
        });
    }
    eprintln!("durable-pty: serving on {} (pid {})", paths::sock().display(), std::process::id());

    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = hup.recv() => {}
        }
    }
    mgr.shutdown();
    let _ = std::fs::remove_file(paths::sock());
    eprintln!("durable-pty: stopped; sessions are recorded and will be restored on next start");
    Ok(())
}
