use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    if let Ok(p) = std::env::var("DPTY_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".durable-pty")
}

pub fn ensure_home() -> std::io::Result<PathBuf> {
    let h = home();
    fs::create_dir_all(&h)?;
    fs::set_permissions(&h, fs::Permissions::from_mode(0o700))?;
    Ok(h)
}

pub fn sock() -> PathBuf {
    home().join("dpty.sock")
}
pub fn db() -> PathBuf {
    home().join("dpty.db")
}
pub fn log_file() -> PathBuf {
    home().join("dpty.log")
}
pub fn pid_file() -> PathBuf {
    home().join("dpty.pid")
}
fn token_file() -> PathBuf {
    home().join("token")
}

/// Bearer token for the HTTP/WebSocket/MCP listener. Created on first use, readable only by the owner.
pub fn token() -> String {
    let f = token_file();
    if let Ok(t) = fs::read_to_string(&f) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let _ = ensure_home();
    let t = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let _ = fs::write(&f, &t);
    let _ = fs::set_permissions(&f, fs::Permissions::from_mode(0o600));
    t
}
