use crate::agents;
use crate::db::Db;
use crate::model::SessionRow;
use crate::util::{basename, now_ms, short_id};
use anyhow::{anyhow, bail, Result};
use crate::proto::{read_frame, write_frame};
use crate::shim::{self, ShimSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};

const RING_MAX: usize = 2 * 1024 * 1024;
const RING_KEEP: usize = 1024 * 1024;
const OUTPUT_CAP: i64 = 16 * 1024 * 1024;

#[derive(Clone)]
pub enum Frame {
    Data(Arc<Vec<u8>>),
    Exit(i32),
}

enum OutMsg {
    /// session id, seq, timestamp, bytes, and the shim stream offset just past them (None for daemon-written text)
    Chunk(String, i64, i64, Vec<u8>, Option<u64>),
    Flush(std_mpsc::Sender<()>),
}

struct ExitMsg {
    id: String,
    epoch: u64,
    code: i32,
    /// The helper process vanished without reporting how the program ended: the program is gone, its outcome unknown.
    lost: bool,
}

pub struct Screen {
    pub parser: vt100::Parser,
    pub ring: Vec<u8>,
    pub seq: i64,
    pub dirty: bool,
    /// How far into the shim's output stream the screen has been built (shim-relative bytes).
    pub offset: u64,
}

/// Commands for the connection to a session's shim.
pub enum ShimCmd {
    Input(Vec<u8>),
    Resize(u16, u16),
    Signal(i32),
    Bye,
}

struct ShimLink {
    tx: mpsc::UnboundedSender<ShimCmd>,
}

#[derive(Default)]
struct InputTracker {
    buf: Vec<u8>,
    esc: u8,
}

impl InputTracker {
    /// Collects typed lines. Nothing is recorded while the terminal has echo off (passwords) or a full-screen app is up.
    fn feed(&mut self, data: &[u8], record: bool) -> Vec<String> {
        let mut out = vec![];
        for &b in data {
            match self.esc {
                1 => {
                    self.esc = if b == b'[' || b == b'O' { 2 } else { 0 };
                    continue;
                }
                2 => {
                    if (0x40..=0x7e).contains(&b) {
                        self.esc = 0;
                    }
                    continue;
                }
                _ => {}
            }
            match b {
                0x1b => self.esc = 1,
                b'\r' | b'\n' => {
                    if record {
                        let t = String::from_utf8_lossy(&self.buf).trim().to_string();
                        if !t.is_empty() {
                            out.push(t);
                        }
                    }
                    self.buf.clear();
                }
                0x7f | 0x08 => {
                    self.buf.pop();
                }
                0x03 | 0x15 => self.buf.clear(),
                0x20..=0xff => {
                    if record && self.buf.len() < 4096 {
                        self.buf.push(b)
                    }
                }
                _ => {}
            }
        }
        if !record {
            self.buf.clear();
        }
        out
    }
}

pub struct Live {
    pub id: String,
    pub meta: Mutex<SessionRow>,
    pub tx: broadcast::Sender<Frame>,
    pub screen: Mutex<Screen>,
    link: Mutex<Option<ShimLink>>,
    input: Mutex<InputTracker>,
    epoch: AtomicU64,
    /// Shim stream offset already stored in the database; bytes below it only rebuild the screen after a daemon restart.
    consumed: AtomicU64,
    echo: AtomicBool,
    user_killed: AtomicBool,
    force_restart: AtomicBool,
    removed: AtomicBool,
    /// The current launch is an attempt to resume an agent conversation.
    resuming: AtomicBool,
    out_tx: std_mpsc::Sender<OutMsg>,
    db: Arc<Db>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn snapshot_bytes(s: &Screen) -> Vec<u8> {
    let mut v = b"\x1b[2J\x1b[H".to_vec();
    v.extend(s.parser.screen().state_formatted());
    v
}

impl Live {
    /// Text written by the daemon itself (banners): shown and stored, but not part of the shim's output stream.
    pub fn emit(&self, data: &[u8]) {
        self.apply(data, 0, None);
    }

    /// Output from the shim starting at stream offset `start`. Anything below `consumed` was stored before a daemon
    /// restart: it is only fed to the screen.
    pub fn emit_at(&self, start: u64, data: &[u8]) {
        let end = start + data.len() as u64;
        let skip = self.consumed.load(Ordering::SeqCst).saturating_sub(start).min(data.len() as u64) as usize;
        self.apply(data, skip, Some(end));
    }

    fn apply(&self, data: &[u8], skip: usize, end: Option<u64>) {
        let ts = now_ms();
        {
            let mut s = lock(&self.screen);
            s.parser.process(data);
            s.dirty = true;
            let fresh = &data[skip..];
            if !fresh.is_empty() {
                s.ring.extend_from_slice(fresh);
                if s.ring.len() > RING_MAX {
                    let cut = s.ring.len() - RING_KEEP;
                    s.ring.drain(..cut);
                }
                s.seq += 1;
                if !self.removed.load(Ordering::Relaxed) {
                    let _ = self.out_tx.send(OutMsg::Chunk(self.id.clone(), s.seq, ts, fresh.to_vec(), end));
                }
                let _ = self.tx.send(Frame::Data(Arc::new(fresh.to_vec())));
            }
            if let Some(e) = end {
                s.offset = s.offset.max(e);
                self.consumed.fetch_max(e, Ordering::SeqCst);
            }
        }
        lock(&self.meta).last_active_ms = ts;
    }

    /// Atomically takes the current screen and subscribes, so no output is lost or repeated.
    pub fn subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Frame>) {
        let s = lock(&self.screen);
        let rx = self.tx.subscribe();
        (snapshot_bytes(&s), rx)
    }

    pub fn screen_text(&self) -> String {
        lock(&self.screen).parser.screen().contents()
    }

    pub fn is_running(&self) -> bool {
        lock(&self.link).is_some()
    }

    fn command(&self, cmd: ShimCmd) -> Result<()> {
        let l = lock(&self.link);
        let link = l.as_ref().ok_or_else(|| anyhow!("session is not running"))?;
        link.tx.send(cmd).map_err(|_| anyhow!("session is not running"))
    }

    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        self.command(ShimCmd::Input(data.to_vec()))?;
        let echo = self.echo.load(Ordering::SeqCst);
        let alt = lock(&self.screen).parser.screen().alternate_screen();
        let lines = lock(&self.input).feed(data, echo && !alt);
        for l in lines {
            let _ = self.db.add_command(&self.id, now_ms(), &l);
        }
        lock(&self.meta).last_active_ms = now_ms();
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 {
            bail!("invalid size");
        }
        let _ = self.command(ShimCmd::Resize(cols, rows));
        lock(&self.screen).parser.screen_mut().set_size(rows, cols);
        let mut m = lock(&self.meta);
        m.cols = cols;
        m.rows = rows;
        Ok(())
    }

    fn signal(&self, sig: Option<i32>) -> Result<()> {
        self.command(ShimCmd::Signal(sig.unwrap_or(libc::SIGHUP)))
    }

    /// Tells the shim the daemon is done with the session; it removes its files and exits.
    fn bye(&self) {
        if let Some(l) = lock(&self.link).take() {
            let _ = l.tx.send(ShimCmd::Bye);
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NewReq {
    pub name: Option<String>,
    pub cmd: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub kind: Option<String>,
    pub agent_session_id: Option<String>,
    pub resume_cmd: Option<String>,
    pub restart: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

pub struct Manager {
    pub db: Arc<Db>,
    live: Mutex<HashMap<String, Arc<Live>>>,
    out_tx: std_mpsc::Sender<OutMsg>,
    exit_tx: mpsc::UnboundedSender<ExitMsg>,
    pub shutting_down: AtomicBool,
    restart_log: Mutex<HashMap<String, VecDeque<i64>>>,
    pub started_ms: i64,
}

fn signal_number(name: &str) -> Option<i32> {
    let n = name.trim_start_matches("SIG").to_ascii_uppercase();
    Some(match n.as_str() {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        other => other.parse().ok()?,
    })
}

pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let n = name.to_ascii_lowercase();
    if let Some(c) = n.strip_prefix("ctrl-").or_else(|| n.strip_prefix("c-")) {
        let b = c.bytes().next()?;
        if c.len() == 1 && b.is_ascii_lowercase() {
            return Some(vec![b - b'a' + 1]);
        }
    }
    Some(match n.as_str() {
        "enter" | "return" => b"\r".to_vec(),
        "tab" => b"\t".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "space" => b" ".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        _ => return None,
    })
}

impl Manager {
    pub fn start(db: Arc<Db>) -> Arc<Manager> {
        let (out_tx, out_rx) = std_mpsc::channel::<OutMsg>();
        let (exit_tx, exit_rx) = mpsc::unbounded_channel::<ExitMsg>();
        let wdb = db.clone();
        std::thread::Builder::new()
            .name("dpty-output".into())
            .spawn(move || output_writer(wdb, out_rx))
            .expect("output thread");
        let m = Arc::new(Manager {
            db,
            live: Mutex::new(HashMap::new()),
            out_tx,
            exit_tx,
            shutting_down: AtomicBool::new(false),
            restart_log: Mutex::new(HashMap::new()),
            started_ms: now_ms(),
        });
        tokio::spawn(m.clone().exit_loop(exit_rx));
        tokio::spawn(m.clone().maintenance());
        m
    }

    // ── lookup ───────────────────────────────────────────────────────────────
    pub fn resolve(&self, key: &str) -> Result<String> {
        let rows = self.db.list_sessions()?;
        if let Some(r) = rows.iter().find(|r| r.id == key) {
            return Ok(r.id.clone());
        }
        if let Some(r) = rows.iter().find(|r| r.name == key) {
            return Ok(r.id.clone());
        }
        let m: Vec<_> = rows.iter().filter(|r| r.id.starts_with(key)).collect();
        match m.len() {
            1 => Ok(m[0].id.clone()),
            0 => bail!("no session matches '{key}'"),
            _ => bail!("'{key}' is ambiguous"),
        }
    }

    pub fn live(&self, id: &str) -> Option<Arc<Live>> {
        lock(&self.live).get(id).cloned()
    }

    pub fn running(&self, key: &str) -> Result<Arc<Live>> {
        let id = self.resolve(key)?;
        self.live(&id).filter(|l| l.is_running()).ok_or_else(|| anyhow!("session '{key}' is not running (use restart, logs or screen)"))
    }

    pub fn row(&self, id: &str) -> Result<SessionRow> {
        if let Some(l) = self.live(id) {
            return Ok(lock(&l.meta).clone());
        }
        self.db.get_session(id)?.ok_or_else(|| anyhow!("no such session"))
    }

    pub fn describe(&self, row: SessionRow) -> Value {
        let (running, attached) = match self.live(&row.id) {
            Some(l) => (l.is_running(), l.tx.receiver_count()),
            None => (false, 0),
        };
        let mut v = serde_json::to_value(&row).unwrap_or(Value::Null);
        v["running"] = json!(running);
        v["attached"] = json!(attached);
        v
    }

    pub fn list(&self) -> Result<Vec<Value>> {
        let mut out = vec![];
        for r in self.db.list_sessions()? {
            let r = self.live(&r.id).map(|l| lock(&l.meta).clone()).unwrap_or(r);
            out.push(self.describe(r));
        }
        Ok(out)
    }

    // ── lifecycle ────────────────────────────────────────────────────────────
    pub async fn create(self: &Arc<Self>, req: NewReq) -> Result<SessionRow> {
        let id = short_id();
        let mut env = req.env;
        if !env.contains_key("PATH") {
            env.insert("PATH".into(), std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()));
        }
        let (cmd, args) = match req.cmd {
            Some(c) => (c, req.args),
            None => (env.get("SHELL").cloned().unwrap_or_else(|| "/bin/zsh".into()), if req.args.is_empty() { vec!["-l".into()] } else { req.args }),
        };
        let kind = req.kind.unwrap_or_else(|| agents::detect_kind(&cmd));
        let cwd = req.cwd.unwrap_or_else(|| env.get("HOME").cloned().unwrap_or_else(|| "/".into()));
        if !std::path::Path::new(&cwd).is_dir() {
            bail!("cwd '{cwd}' is not a directory");
        }
        let restart = req.restart.unwrap_or_else(|| agents::default_restart(&kind).to_string());
        if !["never", "on-failure", "always"].contains(&restart.as_str()) {
            bail!("restart must be never, on-failure or always");
        }
        let name = match req.name {
            Some(n) if !n.is_empty() => {
                if self.db.list_sessions()?.iter().any(|r| r.name == n) {
                    bail!("a session named '{n}' already exists");
                }
                n
            }
            _ => format!("{}-{}", if kind == "process" { basename(&cmd) } else { kind.clone() }, &id[..4]),
        };
        let now = now_ms();
        let mut row = SessionRow {
            id: id.clone(), name, cmd, args, cwd, env, labels: req.labels, kind, agent_session_id: req.agent_session_id,
            resume_cmd: req.resume_cmd, restart, status: "running".into(), exit_code: None, pid: None,
            cols: req.cols.unwrap_or(120).max(2), rows: req.rows.unwrap_or(32).max(2), created_ms: now, started_ms: now,
            ended_ms: None, last_active_ms: now, restarts: 0, live_cwd: None,
        };
        agents::prepare(&mut row);
        self.db.upsert_session(&row)?;
        let live = self.new_live(row, false);
        lock(&self.live).insert(id.clone(), live.clone());
        if let Err(e) = self.start_proc(&live).await {
            lock(&self.live).remove(&id);
            let _ = self.db.delete_session(&id);
            return Err(e);
        }
        self.db.add_event(Some(&id), "created", &lock(&live.meta).cmd);
        let row = lock(&live.meta).clone();
        Ok(row)
    }

    fn new_live(&self, row: SessionRow, prime: bool) -> Arc<Live> {
        let (tx, _) = broadcast::channel(1024);
        let mut parser = vt100::Parser::new(row.rows, row.cols, 0);
        let mut ring = vec![];
        let (mut consumed, mut offset) = (0, 0);
        if prime {
            parser.process(&self.db.get_screen(&row.id).unwrap_or_default());
            ring = self.db.tail_output(&row.id, RING_KEEP).unwrap_or_default();
            (consumed, offset) = self.db.offsets(&row.id).unwrap_or((0, 0));
        }
        let seq = self.db.max_seq(&row.id).unwrap_or(0);
        Arc::new(Live {
            id: row.id.clone(),
            meta: Mutex::new(row),
            tx,
            screen: Mutex::new(Screen { parser, ring, seq, dirty: false, offset }),
            link: Mutex::new(None),
            input: Mutex::new(InputTracker::default()),
            epoch: AtomicU64::new(0),
            consumed: AtomicU64::new(consumed),
            echo: AtomicBool::new(true),
            user_killed: AtomicBool::new(false),
            force_restart: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            resuming: AtomicBool::new(false),
            out_tx: self.out_tx.clone(),
            db: self.db.clone(),
        })
    }

    /// Starts the session's shim (a detached process that owns the PTY and the child) and connects to it.
    async fn start_proc(self: &Arc<Self>, live: &Arc<Live>) -> Result<()> {
        let meta = lock(&live.meta).clone();
        // A shell or plain process comes back where it was; an agent must start where its transcript lives.
        let start_dir = meta.live_cwd.as_deref().filter(|d| !matches!(meta.kind.as_str(), "claude" | "codex" | "grok") && std::path::Path::new(d).is_dir()).unwrap_or(&meta.cwd).to_string();
        let spec = ShimSpec { id: meta.id.clone(), cmd: meta.cmd.clone(), args: meta.args.clone(), cwd: start_dir, env: meta.env.clone(), cols: meta.cols, rows: meta.rows };
        std::fs::create_dir_all(shim::shim_dir())?;
        let _ = std::fs::set_permissions(shim::shim_dir(), std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let _ = std::fs::remove_file(shim::sock_path(&meta.id));
        let _ = std::fs::remove_file(shim::err_path(&meta.id));
        let spec_file = shim::spec_path(&meta.id);
        std::fs::write(&spec_file, serde_json::to_vec(&spec)?)?;
        let _ = std::fs::set_permissions(&spec_file, std::os::unix::fs::PermissionsExt::from_mode(0o600));

        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("shim").arg(&spec_file).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let mut up = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            if shim::sock_path(&meta.id).exists() {
                up = true;
                break;
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
        }
        // Reap the shim when it eventually exits; it is deliberately not tied to this daemon's lifetime.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        if !up {
            let why = std::fs::read_to_string(shim::err_path(&meta.id)).unwrap_or_else(|_| "the session helper did not start".into());
            let _ = std::fs::remove_file(&spec_file);
            bail!("{}", why.trim());
        }
        let _ = self.db.reset_offsets(&meta.id);
        live.consumed.store(0, Ordering::SeqCst);
        lock(&live.screen).offset = 0;
        self.attach_shim(live, 0).await
    }

    /// Connects to a running shim, catches up on output since `from_offset`, then follows it live.
    async fn attach_shim(self: &Arc<Self>, live: &Arc<Live>, from_offset: u64) -> Result<()> {
        let stream = UnixStream::connect(shim::sock_path(&live.id)).await?;
        let (mut r, mut w) = stream.into_split();
        write_frame(&mut w, shim::D_HELLO, &from_offset.to_be_bytes()).await?;
        let (t, ack) = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut r)).await??;
        if t != shim::S_ACK || ack.len() < 30 {
            bail!("unexpected reply from the session helper");
        }
        let base = u64::from_be_bytes(ack[0..8].try_into()?);
        let running = ack[16] != 0;
        let pid = u32::from_be_bytes(ack[21..25].try_into()?);
        let cols = u16::from_be_bytes(ack[25..27].try_into()?);
        let rows = u16::from_be_bytes(ack[27..29].try_into()?);
        live.echo.store(ack[29] != 0, Ordering::SeqCst);
        if from_offset < base {
            live.emit(b"\r\n\x1b[2m[durable-pty: some output was lost while the daemon was away]\x1b[0m\r\n");
        }

        let epoch = live.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        live.user_killed.store(false, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::unbounded_channel::<ShimCmd>();
        if running {
            *lock(&live.link) = Some(ShimLink { tx });
        }
        {
            let mut m = lock(&live.meta);
            m.status = "running".into();
            m.pid = if running { Some(pid) } else { None };
            m.exit_code = None;
            m.ended_ms = None;
            if running {
                m.cols = cols;
                m.rows = rows;
            }
            let _ = self.db.upsert_session(&m);
        }

        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let bye = matches!(cmd, ShimCmd::Bye);
                let (t, p) = match cmd {
                    ShimCmd::Input(d) => (shim::D_INPUT, d),
                    ShimCmd::Resize(c, r) => {
                        let mut p = c.to_be_bytes().to_vec();
                        p.extend(r.to_be_bytes());
                        (shim::D_RESIZE, p)
                    }
                    ShimCmd::Signal(s) => (shim::D_SIGNAL, s.to_be_bytes().to_vec()),
                    ShimCmd::Bye => (shim::D_BYE, vec![]),
                };
                if write_frame(&mut w, t, &p).await.is_err() || bye {
                    break;
                }
            }
        });

        let l = live.clone();
        let exit_tx = self.exit_tx.clone();
        let me = self.clone();
        tokio::spawn(async move {
            let mut exited = false;
            while let Ok((t, p)) = read_frame(&mut r).await {
                match t {
                    shim::S_DATA if p.len() >= 8 => l.emit_at(u64::from_be_bytes(p[..8].try_into().unwrap()), &p[8..]),
                    shim::S_ECHO if !p.is_empty() => l.echo.store(p[0] != 0, Ordering::SeqCst),
                    shim::S_EXIT if p.len() == 4 => {
                        exited = true;
                        let _ = exit_tx.send(ExitMsg { id: l.id.clone(), epoch, code: i32::from_be_bytes(p[..4].try_into().unwrap()), lost: false });
                    }
                    _ => {}
                }
            }
            // The connection ended without the process reporting an exit: the helper itself is gone.
            if !exited && !me.shutting_down.load(Ordering::SeqCst) {
                let _ = exit_tx.send(ExitMsg { id: l.id.clone(), epoch, code: -1, lost: true });
            }
        });
        Ok(())
    }

    fn restart_allowed(&self, id: &str) -> bool {
        let now = now_ms();
        let mut log = lock(&self.restart_log);
        let q = log.entry(id.to_string()).or_default();
        while q.front().is_some_and(|t| now - t > 60_000) {
            q.pop_front();
        }
        if q.len() >= 5 {
            return false;
        }
        q.push_back(now);
        true
    }

    async fn exit_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<ExitMsg>) {
        while let Some(m) = rx.recv().await {
            if self.shutting_down.load(Ordering::SeqCst) {
                continue;
            }
            let Some(live) = self.live(&m.id) else { continue };
            if live.epoch.load(Ordering::SeqCst) != m.epoch {
                continue;
            }
            live.bye();
            // A resume that dies straight away means the agent has no such conversation: start a fresh one instead.
            let quick_resume_failure = live.resuming.swap(false, Ordering::SeqCst)
                && m.code != 0
                && now_ms() - lock(&live.meta).started_ms < 20_000;
            if quick_resume_failure && !live.user_killed.load(Ordering::SeqCst) {
                let dropped = agents::forget_resume(&mut lock(&live.meta));
                if dropped {
                    live.emit(b"\r\n\x1b[2m[durable-pty: the previous conversation could not be resumed, starting a new one]\x1b[0m\r\n");
                    self.db.add_event(Some(&m.id), "resume-failed", "starting a new conversation");
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        me.respawn(&live).await;
                    });
                    continue;
                }
            }
            let policy = lock(&live.meta).restart.clone();
            let killed = live.user_killed.load(Ordering::SeqCst);
            let forced = live.force_restart.swap(false, Ordering::SeqCst);
            let failed = !matches!(m.code, 0 | 129 | 130 | 143);
            let want = forced || (!killed && !live.removed.load(Ordering::SeqCst) && (policy == "always" || (policy == "on-failure" && failed)));
            if want && self.restart_allowed(&m.id) {
                live.emit(format!("\r\n\x1b[2m[durable-pty: process exited ({}), restarting…]\x1b[0m\r\n", m.code).as_bytes());
                self.db.add_event(Some(&m.id), "restarting", &format!("exit {}", m.code));
                let me = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    me.respawn(&live).await;
                });
            } else {
                if want {
                    self.db.add_event(Some(&m.id), "restart-loop", "gave up after 5 restarts in 60s");
                }
                self.finalize(&live, m.code, m.lost);
            }
        }
    }

    async fn respawn(self: &Arc<Self>, live: &Arc<Live>) {
        {
            let mut m = lock(&live.meta);
            let resumed = agents::resume(&mut m);
            live.resuming.store(resumed, Ordering::SeqCst);
            if resumed {
                self.db.add_event(Some(&m.id), "resume", &format!("{} {}", m.cmd, m.args.join(" ")));
            }
            m.restarts += 1;
        }
        if let Err(e) = self.start_proc(live).await {
            live.emit(format!("\r\n[durable-pty: restart failed: {e}]\r\n").as_bytes());
            self.db.add_event(Some(&live.id), "restart-failed", &e.to_string());
            self.finalize(live, -1, false);
        }
    }

    fn finalize(&self, live: &Arc<Live>, code: i32, lost: bool) {
        if lost {
            live.emit(b"\r\n\x1b[38;5;244m[the session's helper process disappeared: the program is gone]\x1b[0m\r\n");
        } else {
            live.emit(format!("\r\n\x1b[38;5;244m[process exited with code {code}]\x1b[0m\r\n").as_bytes());
        }
        {
            let mut m = lock(&live.meta);
            m.status = if lost { "lost".into() } else { "exited".into() };
            m.exit_code = if lost { None } else { Some(code) };
            m.ended_ms = Some(now_ms());
            m.pid = None;
            let _ = self.db.upsert_session(&m);
        }
        {
            let s = lock(&live.screen);
            let _ = self.db.set_screen(&live.id, &snapshot_bytes(&s), s.offset);
        }
        let _ = live.tx.send(Frame::Exit(code));
        lock(&self.live).remove(&live.id);
        self.db.add_event(Some(&live.id), if lost { "lost" } else { "exited" }, &if lost { "helper process disappeared".to_string() } else { code.to_string() });
    }

    pub fn kill(&self, key: &str, signal: Option<&str>) -> Result<()> {
        let live = self.running(key)?;
        live.user_killed.store(true, Ordering::SeqCst);
        let sig = match signal {
            Some(s) => Some(signal_number(s).ok_or_else(|| anyhow!("unknown signal '{s}'"))?),
            None => None,
        };
        live.signal(sig)
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        let id = self.resolve(key)?;
        if let Some(live) = self.live(&id) {
            live.removed.store(true, Ordering::SeqCst);
            live.user_killed.store(true, Ordering::SeqCst);
            let _ = live.signal(Some(libc::SIGKILL));
            live.bye();
            lock(&self.live).remove(&id);
        }
        self.db.delete_session(&id)?;
        self.db.add_event(None, "removed", &id);
        Ok(())
    }

    /// Relaunches a session, resuming the agent conversation when we know its id.
    pub async fn restart(self: &Arc<Self>, key: &str) -> Result<()> {
        let id = self.resolve(key)?;
        if let Some(live) = self.live(&id) {
            if live.is_running() {
                live.force_restart.store(true, Ordering::SeqCst);
                return live.signal(None);
            }
        }
        let row = self.db.get_session(&id)?.ok_or_else(|| anyhow!("no such session"))?;
        self.revive(row).await;
        Ok(())
    }

    async fn revive(self: &Arc<Self>, row: SessionRow) {
        let live = self.new_live(row, true);
        lock(&self.live).insert(live.id.clone(), live.clone());
        live.emit(b"\r\n\x1b[2m[durable-pty: session restored]\x1b[0m\r\n");
        self.respawn(&live).await;
    }

    /// Run once at boot. A session whose helper process is still alive is reattached with nothing lost; one whose
    /// process is gone is marked lost and, per its restart policy, relaunched.
    pub async fn recover(self: &Arc<Self>) {
        let Ok(rows) = self.db.list_sessions() else { return };
        for mut row in rows {
            if row.status != "running" {
                continue;
            }
            if UnixStream::connect(shim::sock_path(&row.id)).await.is_ok() {
                let live = self.new_live(row.clone(), true);
                lock(&self.live).insert(live.id.clone(), live.clone());
                let from = lock(&live.screen).offset;
                match self.attach_shim(&live, from).await {
                    Ok(()) => {
                        live.emit(b"\r\n\x1b[2m[durable-pty: daemon restarted, this session kept running]\x1b[0m\r\n");
                        self.db.add_event(Some(&row.id), "reattached", "daemon restarted, process kept running");
                        continue;
                    }
                    Err(e) => {
                        lock(&self.live).remove(&row.id);
                        self.db.add_event(Some(&row.id), "reattach-failed", &e.to_string());
                    }
                }
            }
            row.status = "lost".into();
            row.pid = None;
            row.ended_ms = Some(now_ms());
            let _ = self.db.upsert_session(&row);
            self.db.add_event(Some(&row.id), "lost", "daemon restarted");
            if row.restart != "never" {
                let me = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    me.revive(row).await;
                });
            }
        }
    }

    pub fn set_agent(&self, key: &str, agent_session_id: &str) -> Result<()> {
        let id = self.resolve(key)?;
        if let Some(l) = self.live(&id) {
            let mut m = lock(&l.meta);
            m.agent_session_id = Some(agent_session_id.to_string());
            self.db.upsert_session(&m)?;
        } else if let Some(mut r) = self.db.get_session(&id)? {
            r.agent_session_id = Some(agent_session_id.to_string());
            self.db.upsert_session(&r)?;
        }
        self.db.add_event(Some(&id), "agent-session", agent_session_id);
        Ok(())
    }

    pub fn set_meta(&self, key: &str, name: Option<String>, labels: Option<BTreeMap<String, String>>, restart: Option<String>) -> Result<()> {
        let id = self.resolve(key)?;
        let mut row = self.row(&id)?;
        if let Some(n) = name {
            if self.db.list_sessions()?.iter().any(|r| r.name == n && r.id != id) {
                bail!("a session named '{n}' already exists");
            }
            row.name = n;
        }
        if let Some(l) = labels {
            row.labels.extend(l);
        }
        if let Some(r) = restart {
            if !["never", "on-failure", "always"].contains(&r.as_str()) {
                bail!("restart must be never, on-failure or always");
            }
            row.restart = r;
        }
        if let Some(l) = self.live(&id) {
            let mut m = lock(&l.meta);
            m.name = row.name.clone();
            m.labels = row.labels.clone();
            m.restart = row.restart.clone();
            self.db.upsert_session(&m)?;
        } else {
            self.db.upsert_session(&row)?;
        }
        Ok(())
    }

    pub fn attach(&self, key: &str, cols: u16, rows: u16) -> Result<(Arc<Live>, Vec<u8>, broadcast::Receiver<Frame>)> {
        let live = self.running(key)?;
        if cols > 0 && rows > 0 {
            let _ = live.resize(cols, rows);
        }
        let (snap, rx) = live.subscribe();
        Ok((live, snap, rx))
    }

    pub fn logs_raw(&self, key: &str, max: usize) -> Result<Vec<u8>> {
        let id = self.resolve(key)?;
        if let Some(l) = self.live(&id) {
            let s = lock(&l.screen);
            let start = s.ring.len().saturating_sub(max);
            return Ok(s.ring[start..].to_vec());
        }
        self.db.tail_output(&id, max)
    }

    pub fn screen_of(&self, key: &str) -> Result<String> {
        let id = self.resolve(key)?;
        if let Some(l) = self.live(&id) {
            return Ok(l.screen_text());
        }
        let mut p = vt100::Parser::new(32, 120, 0);
        p.process(&self.db.get_screen(&id)?);
        Ok(p.screen().contents())
    }

    // ── background work ──────────────────────────────────────────────────────
    async fn maintenance(self: Arc<Self>) {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            tick += 1;
            if self.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            if tick % 3 == 0 {
                self.scan_agents().await;
            }
            if tick % 2 == 0 {
                self.save_state();
            }
            if tick % 60 == 0 {
                let db = self.db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    for r in db.list_sessions().unwrap_or_default() {
                        let _ = db.prune_output(&r.id, OUTPUT_CAP);
                    }
                })
                .await;
            }
        }
    }

    pub fn save_state(&self) {
        let lives: Vec<_> = lock(&self.live).values().cloned().collect();
        for l in lives {
            let snap = {
                let mut s = lock(&l.screen);
                if s.dirty {
                    s.dirty = false;
                    Some((snapshot_bytes(&s), s.offset))
                } else {
                    None
                }
            };
            if let Some((b, offset)) = snap {
                let _ = self.db.set_screen(&l.id, &b, offset);
            }
            {
                let mut m = lock(&l.meta);
                if !matches!(m.kind.as_str(), "claude" | "codex" | "grok") {
                    if let Some(pid) = m.pid.filter(|p| *p > 0) {
                        if let Some(d) = crate::util::process_cwd(pid) {
                            m.live_cwd = Some(d);
                        }
                    }
                }
            }
            let m = lock(&l.meta).clone();
            let _ = self.db.upsert_session(&m);
        }
    }

    async fn scan_agents(self: &Arc<Self>) {
        let pending: Vec<SessionRow> = lock(&self.live)
            .values()
            .map(|l| lock(&l.meta).clone())
            .filter(|m| m.agent_session_id.is_none() && (m.kind == "claude" || m.kind == "codex") && m.status == "running")
            .collect();
        if pending.is_empty() {
            return;
        }
        let claimed: HashSet<String> = self.db.list_sessions().unwrap_or_default().into_iter().filter_map(|r| r.agent_session_id).collect();
        for row in pending {
            let c = claimed.clone();
            let id = row.id.clone();
            if let Ok(Some(found)) = tokio::task::spawn_blocking(move || agents::scan(&row, &c)).await {
                let _ = self.set_agent(&id, &found);
            }
        }
    }

    /// Flush everything durable. Without `kill_sessions` the shims keep every session running and the next daemon
    /// reattaches; with it, every session is stopped and recorded as exited.
    pub fn shutdown(&self, kill_sessions: bool) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if kill_sessions {
            let lives: Vec<_> = lock(&self.live).values().cloned().collect();
            for l in lives {
                {
                    let mut m = lock(&l.meta);
                    m.status = "exited".into();
                    m.exit_code = Some(-1);
                    m.ended_ms = Some(now_ms());
                    m.pid = None;
                    let _ = self.db.upsert_session(&m);
                }
                let _ = l.signal(Some(libc::SIGKILL));
                l.bye();
            }
        }
        self.save_state();
        let (tx, rx) = std_mpsc::channel();
        if self.out_tx.send(OutMsg::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(3));
        }
        if kill_sessions {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn output_writer(db: Arc<Db>, rx: std_mpsc::Receiver<OutMsg>) {
    let mut batch: Vec<(String, i64, i64, Vec<u8>, Option<u64>)> = vec![];
    let mut last = Instant::now();
    let flush = |batch: &mut Vec<(String, i64, i64, Vec<u8>, Option<u64>)>| {
        if !batch.is_empty() {
            let _ = db.append_output(batch);
            batch.clear();
        }
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(OutMsg::Chunk(a, b, c, d, e)) => batch.push((a, b, c, d, e)),
            Ok(OutMsg::Flush(ack)) => {
                flush(&mut batch);
                last = Instant::now();
                let _ = ack.send(());
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                flush(&mut batch);
                return;
            }
        }
        if batch.len() >= 512 || last.elapsed() >= Duration::from_millis(250) {
            flush(&mut batch);
            last = Instant::now();
        }
    }
}
