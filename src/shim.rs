//! The shim: one small process per session that owns the PTY master and the child, so both outlive the daemon.
//!
//! The daemon connects to `<home>/shims/<id>.sock`, says where in the output stream it stopped reading, and gets
//! everything since (the shim keeps the last 8 MB), then live output. If the daemon dies the shim just keeps running;
//! the next daemon reconnects and catches up.
//!
//! Frames reuse the daemon's framing, [type u8][len u32 BE][payload].
use crate::paths;
use crate::proto::{read_frame_blocking, write_frame_blocking};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// daemon -> shim
pub const D_HELLO: u8 = 0; // from_offset u64
pub const D_INPUT: u8 = 1;
pub const D_RESIZE: u8 = 2; // cols u16, rows u16
pub const D_SIGNAL: u8 = 3; // i32
pub const D_BYE: u8 = 4;
// shim -> daemon
pub const S_ACK: u8 = 0; // base u64, total u64, running u8, exit i32, pid u32, cols u16, rows u16, echo u8
pub const S_DATA: u8 = 1; // start offset u64, bytes
pub const S_EXIT: u8 = 2; // i32
pub const S_ECHO: u8 = 3; // u8

const RING_CAP: usize = 8 * 1024 * 1024;
const LINGER_AFTER_EXIT: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Serialize, Deserialize)]
pub struct ShimSpec {
    pub id: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

pub fn shim_dir() -> PathBuf {
    paths::home().join("shims")
}
pub fn sock_path(id: &str) -> PathBuf {
    shim_dir().join(format!("{id}.sock"))
}
pub fn spec_path(id: &str) -> PathBuf {
    shim_dir().join(format!("{id}.spec"))
}
pub fn err_path(id: &str) -> PathBuf {
    shim_dir().join(format!("{id}.err"))
}
fn exit_path(id: &str) -> PathBuf {
    shim_dir().join(format!("{id}.exit"))
}

struct Inner {
    ring: Vec<u8>,
    base: u64,
    total: u64,
    client: Option<UnixStream>,
    exited: Option<i32>,
    exited_at: Option<Instant>,
    echo: bool,
    cols: u16,
    rows: u16,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn echo_on(master: &(dyn MasterPty + Send)) -> bool {
    if let Some(fd) = master.as_raw_fd() {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } == 0 {
            return t.c_lflag & libc::ECHO != 0;
        }
    }
    true
}

/// Writes a frame to the connected daemon, dropping the client if it cannot keep up or is gone.
fn send(inner: &mut Inner, t: u8, payload: &[u8]) {
    if let Some(c) = inner.client.as_mut() {
        if write_frame_blocking(c, t, payload).is_err() {
            if let Some(c) = inner.client.take() {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

pub fn run(spec_file: &str) -> anyhow::Result<()> {
    let spec: ShimSpec = serde_json::from_slice(&std::fs::read(spec_file)?)?;
    let _ = std::fs::remove_file(spec_file); // it holds the environment: do not leave it on disk
    let id = spec.id.clone();
    let result = run_spec(spec);
    if let Err(e) = &result {
        // The daemon is waiting for the socket; tell it why there will not be one.
        let _ = std::fs::write(err_path(&id), e.to_string());
    }
    result
}

fn run_spec(spec: ShimSpec) -> anyhow::Result<()> {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let pty = native_pty_system().openpty(PtySize { rows: spec.rows, cols: spec.cols, pixel_width: 0, pixel_height: 0 })?;
    let mut cmd = CommandBuilder::new(&spec.cmd);
    cmd.args(&spec.args);
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if !spec.env.contains_key("TERM") {
        cmd.env("TERM", "xterm-256color");
    }
    if !spec.env.contains_key("COLORTERM") {
        cmd.env("COLORTERM", "truecolor");
    }
    cmd.env("DPTY_SESSION", &spec.id);
    cmd.cwd(&spec.cwd);
    let mut child = pty.slave.spawn_command(cmd).map_err(|e| anyhow::anyhow!("could not start '{}': {e}", spec.cmd))?;
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pty.master.take_writer()?));
    let pid = child.process_id().unwrap_or(0);
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pty.master));

    std::fs::create_dir_all(shim_dir())?;
    let sock = sock_path(&spec.id);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(exit_path(&spec.id));
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    let inner = Arc::new(Mutex::new(Inner {
        ring: Vec::new(),
        base: 0,
        total: 0,
        client: None,
        exited: None,
        exited_at: None,
        echo: true,
        cols: spec.cols,
        rows: spec.rows,
    }));

    // Output: PTY -> ring buffer -> the connected daemon.
    {
        let inner = inner.clone();
        let master = master.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16384];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let echo = echo_on(&**lock(&master));
                let mut g = lock(&inner);
                let start = g.total;
                g.ring.extend_from_slice(&buf[..n]);
                g.total += n as u64;
                if g.ring.len() > RING_CAP {
                    let cut = g.ring.len() - RING_CAP;
                    g.ring.drain(..cut);
                    g.base += cut as u64;
                }
                if echo != g.echo {
                    g.echo = echo;
                    send(&mut g, S_ECHO, &[echo as u8]);
                }
                let mut payload = Vec::with_capacity(8 + n);
                payload.extend_from_slice(&start.to_be_bytes());
                payload.extend_from_slice(&buf[..n]);
                send(&mut g, S_DATA, &payload);
            }
        });
    }

    // Exit: remember the code (so a daemon that was away can still collect it) and tell the daemon.
    {
        let inner = inner.clone();
        let id = spec.id.clone();
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            // Let the reader drain the last output before announcing the exit.
            std::thread::sleep(Duration::from_millis(150));
            let _ = std::fs::write(exit_path(&id), code.to_string());
            let mut g = lock(&inner);
            g.exited = Some(code);
            g.exited_at = Some(Instant::now());
            send(&mut g, S_EXIT, &code.to_be_bytes());
        });
    }

    // Give up after a week with the process gone and nobody collecting it.
    {
        let inner = inner.clone();
        let id = spec.id.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(600));
            let g = lock(&inner);
            if g.client.is_none() && g.exited_at.is_some_and(|t| t.elapsed() > LINGER_AFTER_EXIT) {
                cleanup(&id);
                std::process::exit(0);
            }
        });
    }

    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let inner = inner.clone();
        let writer = writer.clone();
        let master = master.clone();
        let id = spec.id.clone();
        std::thread::spawn(move || serve(stream, inner, writer, master, pid, &id));
    }
    Ok(())
}

fn cleanup(id: &str) {
    let _ = std::fs::remove_file(sock_path(id));
    let _ = std::fs::remove_file(exit_path(id));
}

fn serve(
    mut stream: UnixStream,
    inner: Arc<Mutex<Inner>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pid: u32,
    id: &str,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok((D_HELLO, hello)) = read_frame_blocking(&mut stream) else { return };
    if hello.len() != 8 {
        return;
    }
    let from = u64::from_be_bytes(hello[..8].try_into().unwrap());
    let _ = stream.set_read_timeout(None);
    let Ok(out) = stream.try_clone() else { return };
    let _ = out.set_write_timeout(Some(Duration::from_secs(10)));

    {
        let mut g = lock(&inner);
        // A newer daemon connection replaces an older one.
        if let Some(old) = g.client.take() {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        g.client = Some(out);
        let mut ack = Vec::new();
        ack.extend_from_slice(&g.base.to_be_bytes());
        ack.extend_from_slice(&g.total.to_be_bytes());
        ack.push(g.exited.is_none() as u8);
        ack.extend_from_slice(&g.exited.unwrap_or(0).to_be_bytes());
        ack.extend_from_slice(&pid.to_be_bytes());
        ack.extend_from_slice(&g.cols.to_be_bytes());
        ack.extend_from_slice(&g.rows.to_be_bytes());
        ack.push(g.echo as u8);
        send(&mut g, S_ACK, &ack);
        // Everything since the daemon last read, in chunks, while holding the lock so live output cannot interleave.
        let start = from.clamp(g.base, g.total);
        let mut off = start;
        while off < g.total {
            let rel = (off - g.base) as usize;
            let end = (rel + 16384).min(g.ring.len());
            let mut payload = Vec::with_capacity(8 + end - rel);
            payload.extend_from_slice(&off.to_be_bytes());
            payload.extend_from_slice(&g.ring[rel..end]);
            send(&mut g, S_DATA, &payload);
            off += (end - rel) as u64;
        }
        if let Some(code) = g.exited {
            send(&mut g, S_EXIT, &code.to_be_bytes());
        }
    }

    loop {
        match read_frame_blocking(&mut stream) {
            Ok((D_INPUT, data)) => {
                let mut w = lock(&writer);
                let _ = w.write_all(&data);
                let _ = w.flush();
            }
            Ok((D_RESIZE, d)) if d.len() == 4 => {
                let (cols, rows) = (u16::from_be_bytes([d[0], d[1]]), u16::from_be_bytes([d[2], d[3]]));
                let _ = lock(&master).resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                let mut g = lock(&inner);
                g.cols = cols;
                g.rows = rows;
            }
            Ok((D_SIGNAL, d)) if d.len() == 4 => {
                let sig = i32::from_be_bytes([d[0], d[1], d[2], d[3]]);
                if pid > 0 {
                    unsafe {
                        if libc::killpg(pid as i32, sig) != 0 {
                            libc::kill(pid as i32, sig);
                        }
                    }
                }
            }
            Ok((D_BYE, _)) => {
                if pid > 0 && lock(&inner).exited.is_none() {
                    unsafe {
                        libc::killpg(pid as i32, libc::SIGKILL);
                    }
                }
                cleanup(id);
                std::process::exit(0);
            }
            Ok(_) => {}
            Err(_) => {
                // The daemon went away: keep the session running and wait for the next one.
                let mut g = lock(&inner);
                g.client = None;
                return;
            }
        }
    }
}
