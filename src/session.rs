use crate::agents;
use crate::db::Db;
use crate::model::SessionRow;
use crate::proto::{read_frame, write_frame};
use crate::shim::{self, ShimSpec};
use crate::util::{basename, now_ms, short_id};
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc as std_mpsc};
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

/// Collects the window title a program sets (OSC 0/2) so the session can show it.
#[derive(Default)]
pub struct TitleCb {
    title: Option<String>,
}

impl vt100::Callbacks for TitleCb {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }
}

/// Drops control characters and leading status glyphs (spinners, `✳`) so the title only changes when the text does.
pub fn clean_title(raw: &str) -> Option<String> {
    let t: String = raw.chars().filter(|c| !c.is_control()).collect();
    let t = t
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim_end();
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(200).collect())
}

pub struct Screen {
    pub parser: vt100::Parser<TitleCb>,
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
                0x20..=0xff if record && self.buf.len() < 4096 => self.buf.push(b),
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
    /// Process that owns the PTY. Zero until the daemon is connected to it.
    shim_pid: AtomicU32,
    out_tx: std_mpsc::Sender<OutMsg>,
    db: Arc<Db>,
    seats: Mutex<Seats>,
}

/// The clients attached to a session and the terminal size each has, oldest first.
#[derive(Default)]
struct Seats {
    next: u64,
    /// The session's size before the first of the current clients attached.
    before: Option<(u16, u16)>,
    taken: Vec<(u64, u16, u16)>,
    /// Attached client processes, including a client that did not set a terminal size.
    clients: Vec<(u64, u32)>,
}

/// What an attaching client gets: the session, its screen so far, the stream of what follows, and its seat.
pub type Attached = (Arc<Live>, Vec<u8>, broadcast::Receiver<Frame>, Seat);

/// One attached client's place in a session. A session has a single size, so it takes the size of whichever client
/// attached or resized last. When that client goes, the size goes back to the client before it, or to what it was
/// before anyone attached: looking at a session from a small panel must not leave it small for the terminal that was
/// already showing it.
pub struct Seat {
    live: Arc<Live>,
    id: u64,
}

impl Seat {
    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut seats = lock(&self.live.seats);
            seats.taken.retain(|s| s.0 != self.id);
            seats.taken.push((self.id, cols, rows));
        }
        let _ = self.live.resize(cols, rows);
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let back_to = {
            let mut seats = lock(&self.live.seats);
            seats.clients.retain(|s| s.0 != self.id);
            let was_last = seats.taken.last().is_some_and(|s| s.0 == self.id);
            seats.taken.retain(|s| s.0 != self.id);
            let size = match seats.taken.last() {
                Some(&(_, cols, rows)) => Some((cols, rows)),
                None => seats.before.take(),
            };
            // Only the client that set the current size hands it back.
            size.filter(|_| was_last)
        };
        if let Some((cols, rows)) = back_to {
            let current = {
                let m = lock(&self.live.meta);
                (m.cols, m.rows)
            };
            if current != (cols, rows) && self.live.is_running() {
                let _ = self.live.resize(cols, rows);
            }
        }
    }
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
        let skip = self
            .consumed
            .load(Ordering::SeqCst)
            .saturating_sub(start)
            .min(data.len() as u64) as usize;
        self.apply(data, skip, Some(end));
    }

    fn apply(&self, data: &[u8], skip: usize, end: Option<u64>) {
        let ts = now_ms();
        let title;
        {
            let mut s = lock(&self.screen);
            s.parser.process(data);
            title = s.parser.callbacks_mut().title.take();
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
                    let _ = self.out_tx.send(OutMsg::Chunk(
                        self.id.clone(),
                        s.seq,
                        ts,
                        fresh.to_vec(),
                        end,
                    ));
                }
                let _ = self.tx.send(Frame::Data(Arc::new(fresh.to_vec())));
            }
            if let Some(e) = end {
                s.offset = s.offset.max(e);
                self.consumed.fetch_max(e, Ordering::SeqCst);
            }
        }
        let changed = {
            let mut m = lock(&self.meta);
            m.last_active_ms = ts;
            match title.as_deref().and_then(clean_title) {
                Some(t) if m.title.as_deref() != Some(t.as_str()) => {
                    m.title = Some(t.clone());
                    Some(t)
                }
                _ => None,
            }
        };
        if let Some(t) = changed {
            self.db.add_event(Some(&self.id), "title", &t);
        }
    }

    /// Atomically takes the current screen and subscribes, so no output is lost or repeated.
    pub fn subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Frame>) {
        let s = lock(&self.screen);
        let rx = self.tx.subscribe();
        (snapshot_bytes(&s), rx)
    }

    /// Like `subscribe`, with the recent scrollback as plain text ahead of the screen, so a terminal attaching to a shell
    /// can scroll back. Plain text only: replaying raw output would replay its terminal queries too. Full-screen programs
    /// have no scrollback worth showing.
    pub fn subscribe_with_history(&self) -> (Vec<u8>, broadcast::Receiver<Frame>) {
        let s = lock(&self.screen);
        let rx = self.tx.subscribe();
        let mut v = vec![];
        if !s.parser.screen().alternate_screen() {
            let tail = &s.ring[s.ring.len().saturating_sub(256 * 1024)..];
            let text = crate::util::strip_ansi(tail);
            // Start at a line boundary: the tail may begin mid-line.
            let text = text.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
            v.extend(text.replace('\n', "\r\n").into_bytes());
            v.extend(b"\r\n");
        }
        v.extend(snapshot_bytes(&s));
        (v, rx)
    }

    /// Recent output exactly as it was written, from the start of a line. Empty while a full-screen program has the
    /// alternate screen: its past output is not what is on screen.
    pub fn raw_history(&self) -> Vec<u8> {
        let s = lock(&self.screen);
        if s.parser.screen().alternate_screen() {
            return vec![];
        }
        let tail = &s.ring[s.ring.len().saturating_sub(256 * 1024)..];
        match tail.iter().position(|&b| b == b'\n') {
            Some(at) => tail[at + 1..].to_vec(),
            None => vec![],
        }
    }

    pub fn screen_text(&self) -> String {
        lock(&self.screen).parser.screen().contents()
    }

    pub fn is_running(&self) -> bool {
        lock(&self.link).is_some()
    }

    fn command(&self, cmd: ShimCmd) -> Result<()> {
        let l = lock(&self.link);
        let link = l
            .as_ref()
            .ok_or_else(|| anyhow!("session is not running"))?;
        link.tx
            .send(cmd)
            .map_err(|_| anyhow!("session is not running"))
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

    /// Takes a place in the session for a client whose terminal is `cols` x `rows` (0 when it has no size of its own).
    /// `client` is that process, when the connection names one.
    pub fn seat(self: &Arc<Self>, cols: u16, rows: u16, client: Option<u32>) -> Seat {
        let id = {
            let mut seats = lock(&self.seats);
            if seats.taken.is_empty() {
                let m = lock(&self.meta);
                seats.before = Some((m.cols, m.rows)).filter(|s| s.0 > 0 && s.1 > 0);
            }
            seats.next += 1;
            let id = seats.next;
            // A client with no size of its own never decides the session's size.
            if cols > 0 && rows > 0 {
                seats.taken.push((id, cols, rows));
            }
            if let Some(pid) = client.filter(|p| *p > 0) {
                seats.clients.push((id, pid));
            }
            id
        };
        if cols > 0 && rows > 0 {
            let _ = self.resize(cols, rows);
        }
        Seat {
            live: self.clone(),
            id,
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 {
            bail!("invalid size");
        }
        let _ = self.command(ShimCmd::Resize(cols, rows));
        {
            let mut s = lock(&self.screen);
            let (old_rows, _) = s.parser.screen().size();
            if rows < old_rows && !s.parser.screen().alternate_screen() {
                // Shrinking cuts rows off the bottom, where a shell's prompt is. Rebuild from recent output instead, so
                // the newest lines stay in view as they would in a real terminal. (A full-screen program redraws itself.)
                let mut parser =
                    vt100::Parser::new_with_callbacks(rows, cols, 0, TitleCb::default());
                parser.process(&s.ring);
                parser.callbacks_mut().title = None;
                s.parser = parser;
            } else {
                s.parser.screen_mut().set_size(rows, cols);
            }
            s.dirty = true;
        }
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
        self.shim_pid.store(0, Ordering::Relaxed);
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
        // Output stored by a version without the search index is indexed once, in the background.
        let idb = m.db.clone();
        let _ = std::thread::Builder::new()
            .name("dpty-index".into())
            .spawn(move || index_stored_output(&idb));
        tokio::spawn(m.clone().exit_loop(exit_rx));
        tokio::spawn(m.clone().maintenance());
        m
    }

    // ── lookup ───────────────────────────────────────────────────────────────
    /// The running session a process lives inside, if any: the session whose program is the process or one of its
    /// ancestors.
    pub fn session_of_pid(&self, pid: u32) -> Option<String> {
        let chain = crate::util::ancestry(pid);
        let lives: Vec<_> = lock(&self.live).values().cloned().collect();
        lives
            .into_iter()
            .find(|l| {
                lock(&l.meta)
                    .pid
                    .is_some_and(|p| p > 0 && chain.contains(&p))
            })
            .map(|l| l.id.clone())
    }

    pub fn resolve(&self, key: &str) -> Result<String> {
        if key.is_empty() {
            bail!("a session id or name is required");
        }
        let mut ids = self.db.find_ids(key)?;
        match ids.len() {
            1 => Ok(ids.remove(0)),
            0 => bail!("no session matches '{key}'"),
            _ => bail!("'{key}' is ambiguous"),
        }
    }

    pub fn live(&self, id: &str) -> Option<Arc<Live>> {
        lock(&self.live).get(id).cloned()
    }

    pub fn running(&self, key: &str) -> Result<Arc<Live>> {
        let id = self.resolve(key)?;
        self.live(&id)
            .filter(|l| l.is_running())
            .ok_or_else(|| anyhow!("session '{key}' is not running (use restart, logs or screen)"))
    }

    pub fn row(&self, id: &str) -> Result<SessionRow> {
        if let Some(l) = self.live(id) {
            return Ok(lock(&l.meta).clone());
        }
        self.db
            .get_session(id)?
            .ok_or_else(|| anyhow!("no such session"))
    }

    pub fn describe(&self, row: SessionRow) -> Value {
        let (running, attached, shim_pid, client_pids) = match self.live(&row.id) {
            Some(l) => {
                let clients: Vec<u32> =
                    lock(&l.seats).clients.iter().map(|(_, pid)| *pid).collect();
                (
                    l.is_running(),
                    l.tx.receiver_count(),
                    l.shim_pid.load(Ordering::Relaxed),
                    clients,
                )
            }
            None => (false, 0, 0, Vec::new()),
        };
        let mut v = serde_json::to_value(&row).unwrap_or(Value::Null);
        // The environment holds credentials (API keys): callers, including agents over MCP, only get the variable names.
        v["env_keys"] = json!(row.env.keys().collect::<Vec<_>>());
        if let Some(o) = v.as_object_mut() {
            o.remove("env");
        }
        v["running"] = json!(running);
        v["attached"] = json!(attached);
        v["shim_pid"] = if shim_pid > 0 {
            json!(shim_pid)
        } else {
            Value::Null
        };
        v["client_pids"] = json!(client_pids);
        v
    }

    /// Every running `dpty` process, classified. `skip` is the process making this request.
    pub fn processes(&self, skip: Option<u32>) -> Vec<Value> {
        let lives: Vec<_> = lock(&self.live).values().cloned().collect();
        let mut shims = HashMap::new();
        let mut clients = HashMap::new();
        for live in lives {
            let name = lock(&live.meta).name.clone();
            let shim = live.shim_pid.load(Ordering::Relaxed);
            if shim > 0 {
                shims.insert(shim, name.clone());
            }
            for (_, pid) in &lock(&live.seats).clients {
                clients.insert(*pid, name.clone());
            }
        }
        crate::util::classify_dpty(
            std::process::id(),
            &shims,
            &clients,
            &crate::util::dpty_processes(),
            skip,
        )
        .into_iter()
        .map(|p| {
            json!({
                "pid": p.pid,
                "role": p.role,
                "session": if p.session.is_empty() { Value::Null } else { json!(p.session) },
                "cmd": p.cmd,
            })
        })
        .collect()
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
            env.insert(
                "PATH".into(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
            );
        }
        let (cmd, args) = match req.cmd {
            Some(c) => (c, req.args),
            None => (
                env.get("SHELL")
                    .cloned()
                    .unwrap_or_else(|| "/bin/zsh".into()),
                if req.args.is_empty() {
                    vec!["-l".into()]
                } else {
                    req.args
                },
            ),
        };
        let kind = req.kind.unwrap_or_else(|| agents::detect_kind(&cmd));
        let cwd = req
            .cwd
            .unwrap_or_else(|| env.get("HOME").cloned().unwrap_or_else(|| "/".into()));
        if !std::path::Path::new(&cwd).is_dir() {
            bail!("cwd '{cwd}' is not a directory");
        }
        let restart = req
            .restart
            .unwrap_or_else(|| agents::default_restart(&kind).to_string());
        if !["never", "on-failure", "always"].contains(&restart.as_str()) {
            bail!("restart must be never, on-failure or always");
        }
        let name = match req.name {
            Some(n) if !n.is_empty() => {
                if self.db.name_taken(&n, "")? {
                    bail!("a session named '{n}' already exists");
                }
                n
            }
            _ => format!(
                "{}-{}",
                if kind == "process" {
                    basename(&cmd)
                } else {
                    kind.clone()
                },
                &id[..4]
            ),
        };
        let now = now_ms();
        let mut row = SessionRow {
            id: id.clone(),
            name,
            cmd,
            args,
            cwd,
            env,
            labels: req.labels,
            kind,
            agent_session_id: req.agent_session_id,
            resume_cmd: req.resume_cmd,
            restart,
            status: "running".into(),
            exit_code: None,
            pid: None,
            cols: req.cols.unwrap_or(120).max(2),
            rows: req.rows.unwrap_or(32).max(2),
            created_ms: now,
            started_ms: now,
            ended_ms: None,
            last_active_ms: now,
            restarts: 0,
            live_cwd: None,
            title: None,
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
        self.db
            .add_event(Some(&id), "created", &lock(&live.meta).cmd);
        let row = lock(&live.meta).clone();
        Ok(row)
    }

    fn new_live(&self, row: SessionRow, prime: bool) -> Arc<Live> {
        let (tx, _) = broadcast::channel(1024);
        let mut parser =
            vt100::Parser::new_with_callbacks(row.rows, row.cols, 0, TitleCb::default());
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
            screen: Mutex::new(Screen {
                parser,
                ring,
                seq,
                dirty: false,
                offset,
            }),
            link: Mutex::new(None),
            input: Mutex::new(InputTracker::default()),
            epoch: AtomicU64::new(0),
            consumed: AtomicU64::new(consumed),
            echo: AtomicBool::new(true),
            user_killed: AtomicBool::new(false),
            force_restart: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            resuming: AtomicBool::new(false),
            shim_pid: AtomicU32::new(0),
            out_tx: self.out_tx.clone(),
            db: self.db.clone(),
            seats: Mutex::new(Seats::default()),
        })
    }

    /// Starts the session's shim (a detached process that owns the PTY and the child) and connects to it.
    async fn start_proc(self: &Arc<Self>, live: &Arc<Live>) -> Result<()> {
        let meta = lock(&live.meta).clone();
        // A shell or plain process comes back where it was; an agent must start where its transcript lives.
        let start_dir = meta
            .live_cwd
            .as_deref()
            .filter(|d| {
                !matches!(meta.kind.as_str(), "claude" | "codex" | "grok")
                    && std::path::Path::new(d).is_dir()
            })
            .unwrap_or(&meta.cwd)
            .to_string();
        let spec = ShimSpec {
            id: meta.id.clone(),
            cmd: meta.cmd.clone(),
            args: meta.args.clone(),
            cwd: start_dir,
            env: meta.env.clone(),
            cols: meta.cols,
            rows: meta.rows,
        };
        std::fs::create_dir_all(shim::shim_dir())?;
        let _ = std::fs::set_permissions(
            shim::shim_dir(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        );
        let _ = std::fs::remove_file(shim::sock_path(&meta.id));
        let _ = std::fs::remove_file(shim::err_path(&meta.id));
        let spec_file = shim::spec_path(&meta.id);
        std::fs::write(&spec_file, serde_json::to_vec(&spec)?)?;
        let _ = std::fs::set_permissions(
            &spec_file,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );

        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("shim")
            .arg(&spec_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
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
            let why = std::fs::read_to_string(shim::err_path(&meta.id))
                .unwrap_or_else(|_| "the session helper did not start".into());
            let _ = std::fs::remove_file(&spec_file);
            bail!("{}", why.trim());
        }
        let _ = self.db.reset_offsets(&meta.id);
        // Each launch has its own start time: the quick-resume-failure check and the agent transcript scan measure from it.
        lock(&live.meta).started_ms = now_ms();
        live.consumed.store(0, Ordering::SeqCst);
        lock(&live.screen).offset = 0;
        self.attach_shim(live, 0).await
    }

    /// Connects to a running shim, catches up on output since `from_offset`, then follows it live.
    async fn attach_shim(self: &Arc<Self>, live: &Arc<Live>, from_offset: u64) -> Result<()> {
        let stream = UnixStream::connect(shim::sock_path(&live.id)).await?;
        if let Some(pid) = stream
            .peer_cred()
            .ok()
            .and_then(|c| c.pid())
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p > 0)
        {
            live.shim_pid.store(pid, Ordering::Relaxed);
        }
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
                    shim::S_DATA if p.len() >= 8 => {
                        l.emit_at(u64::from_be_bytes(p[..8].try_into().unwrap()), &p[8..])
                    }
                    shim::S_ECHO if !p.is_empty() => l.echo.store(p[0] != 0, Ordering::SeqCst),
                    shim::S_EXIT if p.len() == 4 => {
                        exited = true;
                        let _ = exit_tx.send(ExitMsg {
                            id: l.id.clone(),
                            epoch,
                            code: i32::from_be_bytes(p[..4].try_into().unwrap()),
                            lost: false,
                        });
                    }
                    _ => {}
                }
            }
            // The connection ended without the process reporting an exit. That is not proof the helper died: it drops a
            // daemon that stalls. Reconnect and carry on from where the screen stands; only a helper that cannot be
            // reached is gone.
            if !exited
                && !me.shutting_down.load(Ordering::SeqCst)
                && l.epoch.load(Ordering::SeqCst) == epoch
                && !l.removed.load(Ordering::SeqCst)
            {
                *lock(&l.link) = None;
                if !me.clone().reconnect(l.clone()).await {
                    let _ = exit_tx.send(ExitMsg {
                        id: l.id.clone(),
                        epoch,
                        code: -1,
                        lost: true,
                    });
                }
            }
        });
        Ok(())
    }

    /// Tries to get back to a session's helper after its connection dropped. Boxed because attaching spawns the task
    /// that calls this.
    fn reconnect(
        self: Arc<Self>,
        live: Arc<Live>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
        Box::pin(async move {
            let killed = live.user_killed.load(Ordering::SeqCst);
            for attempt in 0..4u64 {
                tokio::time::sleep(Duration::from_millis(100 + attempt * 150)).await;
                if self.shutting_down.load(Ordering::SeqCst) || live.removed.load(Ordering::SeqCst)
                {
                    return true;
                }
                // A helper that is gone refuses the connection at once: no point waiting on it.
                if UnixStream::connect(shim::sock_path(&live.id))
                    .await
                    .is_err()
                {
                    return false;
                }
                let from = lock(&live.screen).offset;
                if self.attach_shim(&live, from).await.is_ok() {
                    live.user_killed.store(killed, Ordering::SeqCst);
                    self.db.add_event(
                        Some(&live.id),
                        "reconnected",
                        "connection to the session helper was re-established",
                    );
                    return true;
                }
            }
            false
        })
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
            let Some(live) = self.live(&m.id) else {
                continue;
            };
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
                    self.db
                        .add_event(Some(&m.id), "resume-failed", "starting a new conversation");
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        // Launch as prepared: resuming here would turn the new conversation id into another failed resume.
                        me.respawn(&live, false).await;
                    });
                    continue;
                }
            }
            let policy = lock(&live.meta).restart.clone();
            let killed = live.user_killed.load(Ordering::SeqCst);
            let forced = live.force_restart.swap(false, Ordering::SeqCst);
            let failed = !matches!(m.code, 0 | 129 | 130 | 143);
            let want = forced
                || (!killed
                    && !live.removed.load(Ordering::SeqCst)
                    && (policy == "always" || (policy == "on-failure" && failed)));
            if want && self.restart_allowed(&m.id) {
                live.emit(
                    format!(
                        "\r\n\x1b[2m[durable-pty: process exited ({}), restarting…]\x1b[0m\r\n",
                        m.code
                    )
                    .as_bytes(),
                );
                self.db
                    .add_event(Some(&m.id), "restarting", &format!("exit {}", m.code));
                let me = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    me.respawn(&live, true).await;
                });
            } else {
                if want {
                    self.db.add_event(
                        Some(&m.id),
                        "restart-loop",
                        "gave up after 5 restarts in 60s",
                    );
                }
                self.finalize(&live, m.code, m.lost);
            }
        }
    }

    async fn respawn(self: &Arc<Self>, live: &Arc<Live>, resume: bool) {
        {
            let mut m = lock(&live.meta);
            let resumed = resume && agents::resume(&mut m);
            live.resuming.store(resumed, Ordering::SeqCst);
            if resumed {
                self.db.add_event(
                    Some(&m.id),
                    "resume",
                    &format!("{} {}", m.cmd, m.args.join(" ")),
                );
            }
            m.restarts += 1;
        }
        if let Err(e) = self.start_proc(live).await {
            live.emit(format!("\r\n[durable-pty: restart failed: {e}]\r\n").as_bytes());
            self.db
                .add_event(Some(&live.id), "restart-failed", &e.to_string());
            self.finalize(live, -1, false);
        }
    }

    fn finalize(&self, live: &Arc<Live>, code: i32, lost: bool) {
        if lost {
            live.emit(b"\r\n\x1b[38;5;244m[the session's helper process disappeared: the program is gone]\x1b[0m\r\n");
        } else {
            live.emit(
                format!("\r\n\x1b[38;5;244m[process exited with code {code}]\x1b[0m\r\n")
                    .as_bytes(),
            );
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
        self.db.add_event(
            Some(&live.id),
            if lost { "lost" } else { "exited" },
            &if lost {
                "helper process disappeared".to_string()
            } else {
                code.to_string()
            },
        );
    }

    pub fn kill(self: &Arc<Self>, key: &str, signal: Option<&str>) -> Result<()> {
        let live = self.running(key)?;
        live.user_killed.store(true, Ordering::SeqCst);
        match signal {
            Some(s) => live.signal(Some(
                signal_number(s).ok_or_else(|| anyhow!("unknown signal '{s}'"))?,
            )),
            None => self.stop_process(&live),
        }
    }

    /// Asks the program to go (HUP), then insists (TERM, KILL) if it is still there, so a stop or restart always lands.
    fn stop_process(self: &Arc<Self>, live: &Arc<Live>) -> Result<()> {
        live.signal(None)?;
        let epoch = live.epoch.load(Ordering::SeqCst);
        let live = live.clone();
        tokio::spawn(async move {
            for sig in [libc::SIGTERM, libc::SIGKILL] {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if live.epoch.load(Ordering::SeqCst) != epoch || !live.is_running() {
                    return;
                }
                let _ = live.signal(Some(sig));
            }
        });
        Ok(())
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
        if let Some(live) = self.live(&id)
            && live.is_running()
        {
            live.force_restart.store(true, Ordering::SeqCst);
            return self.stop_process(&live);
        }
        let row = self
            .db
            .get_session(&id)?
            .ok_or_else(|| anyhow!("no such session"))?;
        self.revive(row).await;
        Ok(())
    }

    async fn revive(self: &Arc<Self>, row: SessionRow) {
        let live = self.new_live(row, true);
        lock(&self.live).insert(live.id.clone(), live.clone());
        live.emit(b"\r\n\x1b[2m[durable-pty: session restored]\x1b[0m\r\n");
        self.respawn(&live, true).await;
    }

    /// Run once at boot. A session whose helper process is still alive is reattached with nothing lost; one whose
    /// process is gone is marked lost and, per its restart policy, relaunched.
    pub async fn recover(self: &Arc<Self>) {
        let Ok(rows) = self.db.list_sessions() else {
            return;
        };
        for mut row in rows {
            // A session recorded as lost whose helper still answers was only out of reach: take it back.
            let was_lost = row.status == "lost";
            if row.status != "running" && !was_lost {
                continue;
            }
            let reachable = UnixStream::connect(shim::sock_path(&row.id)).await.is_ok();
            if was_lost && !reachable {
                continue;
            }
            if reachable {
                let live = self.new_live(row.clone(), true);
                lock(&self.live).insert(live.id.clone(), live.clone());
                let from = lock(&live.screen).offset;
                match self.attach_shim(&live, from).await {
                    Ok(()) => {
                        // No banner here: the program may be a full-screen TUI, and text written into its screen would
                        // sit inside its layout until it redraws. The event log records the reattach.
                        self.db.add_event(
                            Some(&row.id),
                            "reattached",
                            "daemon restarted, process kept running",
                        );
                        continue;
                    }
                    Err(e) => {
                        lock(&self.live).remove(&row.id);
                        self.db
                            .add_event(Some(&row.id), "reattach-failed", &e.to_string());
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
        self.db
            .add_event(Some(&id), "agent-session", agent_session_id);
        Ok(())
    }

    pub fn set_meta(
        &self,
        key: &str,
        name: Option<String>,
        labels: Option<BTreeMap<String, String>>,
        restart: Option<String>,
    ) -> Result<()> {
        let id = self.resolve(key)?;
        let mut row = self.row(&id)?;
        if let Some(n) = name {
            if n.is_empty() {
                bail!("a name cannot be empty");
            }
            if self.db.name_taken(&n, &id)? {
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

    /// The returned seat holds the client's size in the session; dropping it (the client going away) gives the size
    /// back.
    pub fn attach(
        &self,
        key: &str,
        cols: u16,
        rows: u16,
        history: bool,
        client: Option<u32>,
    ) -> Result<Attached> {
        let live = self.running(key)?;
        let seat = live.seat(cols, rows, client);
        let shell = matches!(lock(&live.meta).kind.as_str(), "shell" | "process");
        let (snap, rx) = if history && shell {
            live.subscribe_with_history()
        } else {
            live.subscribe()
        };
        Ok((live, snap, rx, seat))
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

    /// The screen as terminal bytes plus its size, without attaching or resizing: (bytes, cols, rows, running).
    pub fn snapshot_of(&self, key: &str) -> Result<(Vec<u8>, u16, u16, bool)> {
        let id = self.resolve(key)?;
        if let Some(l) = self.live(&id) {
            let s = lock(&l.screen);
            let (rows, cols) = s.parser.screen().size();
            return Ok((snapshot_bytes(&s), cols, rows, l.is_running()));
        }
        let row = self
            .db
            .get_session(&id)?
            .ok_or_else(|| anyhow!("no such session"))?;
        Ok((self.db.get_screen(&id)?, row.cols, row.rows, false))
    }

    /// Searches everything kept about every session: output, typed commands and, for claude and codex sessions, the
    /// conversation transcript. Blocking (database and file reads).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(vec![]);
        }
        let mut out: Vec<Value> = self
            .db
            .search(query, limit as i64)?
            .into_iter()
            .map(|(id, source, ts, text)| json!({"id": id, "source": source, "ts": ts, "text": crate::util::excerpt(&text, query, 160)}))
            .collect();
        for r in self.db.list_sessions()? {
            if out.len() >= limit * 3 {
                break;
            }
            if let Some(file) = agents::transcript_path(&r) {
                for text in crate::util::grep_file(&file, query, 3) {
                    out.push(json!({"id": r.id, "source": "transcript", "ts": r.last_active_ms, "text": text}));
                }
            }
        }
        Ok(out)
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
            if tick.is_multiple_of(3) {
                self.scan_agents().await;
            }
            if tick.is_multiple_of(2) {
                self.save_state();
            }
            if tick.is_multiple_of(60) {
                self.autoclean();
            }
        }
    }

    /// Removes sessions nobody will come back to. Off unless asked for, because it deletes history. A running plain
    /// shell goes when no client is attached, it has no child processes (nothing running in it), and it has been silent
    /// for `DPTY_IDLE_SHELL_SECS`. An exited shell/process session goes `DPTY_EXITED_KEEP_SECS` after it ended. Agent
    /// sessions are never touched (they can be resumed), nor is anything labelled `keep`. Unset or 0 disables each.
    pub fn autoclean(&self) {
        let idle_ms = crate::util::env_secs("DPTY_IDLE_SHELL_SECS", 0) as i64 * 1000;
        let exited_ms = crate::util::env_secs("DPTY_EXITED_KEEP_SECS", 0) as i64 * 1000;
        let now = now_ms();
        let mut doomed: Vec<(String, &'static str)> = vec![];
        if idle_ms > 0 {
            let lives: Vec<_> = lock(&self.live).values().cloned().collect();
            for l in lives {
                let m = lock(&l.meta).clone();
                let idle = m.kind == "shell"
                    && !m.labels.contains_key("keep")
                    && l.is_running()
                    && l.tx.receiver_count() == 0
                    && now - m.last_active_ms > idle_ms
                    && m.pid
                        .is_some_and(|p| p > 0 && !crate::util::has_children(p));
                if idle {
                    doomed.push((m.id, "idle empty shell"));
                }
            }
        }
        if exited_ms > 0 {
            for r in self.db.list_sessions().unwrap_or_default() {
                let stale = matches!(r.kind.as_str(), "shell" | "process")
                    && r.status != "running"
                    && !r.labels.contains_key("keep")
                    && self.live(&r.id).is_none_or(|l| !l.is_running())
                    && now - r.ended_ms.unwrap_or(r.last_active_ms) > exited_ms;
                if stale {
                    doomed.push((r.id, "exited"));
                }
            }
        }
        for (id, why) in doomed {
            if self.remove(&id).is_ok() {
                self.db
                    .add_event(None, "autoclean", &format!("{id}: {why}"));
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
                if !matches!(m.kind.as_str(), "claude" | "codex" | "grok")
                    && let Some(pid) = m.pid.filter(|p| *p > 0)
                    && let Some(d) = crate::util::process_cwd(pid)
                {
                    m.live_cwd = Some(d);
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
            .filter(|m| {
                m.agent_session_id.is_none()
                    && (m.kind == "claude" || m.kind == "codex")
                    && m.status == "running"
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        let claimed: HashSet<String> = self
            .db
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| r.agent_session_id)
            .collect();
        for row in pending {
            let c = claimed.clone();
            let id = row.id.clone();
            if let Ok(Some(found)) =
                tokio::task::spawn_blocking(move || agents::scan(&row, &c)).await
            {
                let _ = self.set_agent(&id, &found);
            }
        }
    }

    /// Flush everything durable. Without `kill_sessions` the shims keep every session running and the next daemon
    /// reattaches; with it, every session is stopped and recorded as exited.
    /// `spare` is a session left running even with `kill_sessions`: the one the request came from.
    pub fn shutdown(&self, kill_sessions: bool, spare: Option<&str>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if kill_sessions {
            let lives: Vec<_> = lock(&self.live)
                .values()
                .filter(|l| spare != Some(l.id.as_str()))
                .cloned()
                .collect();
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

/// Builds the search index for output that was stored before the index existed. Works a page at a time so the
/// database is never held for long, and records completion so it runs once.
fn index_stored_output(db: &Db) {
    const DONE: &str = "lines_backfilled";
    if db.meta(DONE).is_some() {
        return;
    }
    for id in db.unindexed_sessions().unwrap_or_default() {
        let mut plain = crate::util::PlainLines::default();
        let mut after = 0;
        loop {
            let Ok(page) = db.output_page(&id, after, 256) else {
                return;
            };
            let Some(last) = page.last() else { break };
            after = last.0;
            let mut lines: Vec<(String, i64, String)> = vec![];
            for (_, ts, data) in &page {
                lines.extend(
                    plain
                        .feed(data)
                        .into_iter()
                        .map(|text| (id.clone(), *ts, text)),
                );
            }
            if !lines.is_empty() && db.append_lines(&lines).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let _ = db.set_meta(DONE, "1");
}

fn output_writer(db: Arc<Db>, rx: std_mpsc::Receiver<OutMsg>) {
    let mut batch: Vec<crate::db::OutChunk> = vec![];
    let mut lines: Vec<(String, i64, String)> = vec![];
    // Per session: the plain-text extractor feeding the search index.
    let mut plain: HashMap<String, crate::util::PlainLines> = HashMap::new();
    // Sessions that wrote since the last prune: only those can be over the cap.
    let mut wrote: HashSet<String> = HashSet::new();
    let mut last = Instant::now();
    let mut pruned = Instant::now();
    let flush = |batch: &mut Vec<crate::db::OutChunk>, lines: &mut Vec<(String, i64, String)>| {
        if !batch.is_empty() {
            let _ = db.append_output(batch);
            batch.clear();
        }
        if !lines.is_empty() {
            let _ = db.append_lines(lines);
            lines.clear();
        }
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(OutMsg::Chunk(id, seq, ts, data, end)) => {
                for text in plain.entry(id.clone()).or_default().feed(&data) {
                    lines.push((id.clone(), ts, text));
                }
                wrote.insert(id.clone());
                batch.push((id, seq, ts, data, end));
            }
            Ok(OutMsg::Flush(ack)) => {
                flush(&mut batch, &mut lines);
                last = Instant::now();
                let _ = ack.send(());
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                flush(&mut batch, &mut lines);
                return;
            }
        }
        if batch.len() >= 512 || last.elapsed() >= Duration::from_millis(250) {
            flush(&mut batch, &mut lines);
            last = Instant::now();
        }
        if pruned.elapsed() >= Duration::from_secs(60) {
            for id in wrote.drain() {
                let _ = db.prune_output(&id, OUTPUT_CAP);
            }
            plain.retain(|id, _| db.get_session(id).ok().flatten().is_some());
            pruned = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_osc() {
        let mut p = vt100::Parser::new_with_callbacks(24, 80, 0, TitleCb::default());
        p.process(b"hi\x1b]0;\xe2\x9c\xb3 Fix the bug\x07there");
        assert_eq!(
            p.callbacks_mut()
                .title
                .take()
                .as_deref()
                .and_then(clean_title)
                .as_deref(),
            Some("Fix the bug")
        );
        p.process(b"\x1b]2;\xe2\xa0\x82 Fix the bug\x1b\\");
        assert_eq!(
            p.callbacks_mut()
                .title
                .take()
                .as_deref()
                .and_then(clean_title)
                .as_deref(),
            Some("Fix the bug")
        );
        p.process(b"no title here");
        assert!(p.callbacks_mut().title.take().is_none());
    }

    #[test]
    fn clean_title_rules() {
        assert_eq!(clean_title("  "), None);
        assert_eq!(clean_title("✳"), None);
        assert_eq!(clean_title("zsh ~/proj ").as_deref(), Some("zsh ~/proj"));
    }
}
