mod agents;
mod browser;
mod client;
mod daemon;
mod db;
mod http;
mod install;
mod mcp;
mod model;
mod pane;
mod paths;
mod proto;
mod rich;
mod rpc;
mod server;
mod session;
mod shim;
mod util;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use client::Client;
use proto::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const DEFAULT_HTTP: &str = "127.0.0.1:7878";

#[derive(Parser)]
#[command(
    name = "dpty",
    version,
    about = "Durable PTY sessions: daemon, CLI and MCP server in one binary"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (clients start it automatically; `install` runs it at login)
    Serve {
        #[arg(long, default_value = DEFAULT_HTTP)]
        http: String,
        #[arg(long)]
        no_http: bool,
    },
    /// Stop the daemon. Sessions keep running and the next daemon picks them up again
    Stop {
        /// Also stop every session
        #[arg(long)]
        sessions: bool,
    },
    /// Internal: the helper process that owns one session's terminal
    #[command(hide = true)]
    Shim {
        spec: String,
    },
    /// Daemon status
    Status,
    /// Print the bearer token for the HTTP/WebSocket/MCP listener
    Token,
    /// List sessions, then every running dpty process
    Ls {
        #[arg(long)]
        json: bool,
        #[arg(short, long)]
        label: Vec<String>,
        #[arg(long)]
        status: Option<String>,
    },
    /// Browse, search, and attach to sessions interactively
    #[command(visible_alias = "ui")]
    Browse,
    /// Start a session: dpty new -n work -C ~/proj -- claude "fix the bug"
    New {
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short = 'C', long)]
        cwd: Option<String>,
        #[arg(short, long)]
        label: Vec<String>,
        #[arg(short, long)]
        env: Vec<String>,
        /// Pass the whole environment to the session (default: PATH, HOME, locale and agent API variables)
        #[arg(long)]
        env_all: bool,
        /// Create without attaching
        #[arg(short, long)]
        detach: bool,
        #[arg(long)]
        restart: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// How to resume an agent we have no adapter for, e.g. "grok --resume {id}"
        #[arg(long)]
        resume_cmd: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Attach this terminal to a session (detach with Ctrl-\)
    Attach {
        id: String,
        /// Do not print the session's recent scrollback before the screen
        #[arg(long)]
        no_history: bool,
    },
    Kill {
        id: String,
        #[arg(short, long)]
        signal: Option<String>,
    },
    /// Delete a session and its history
    Rm {
        id: String,
    },
    /// Relaunch a session, resuming the agent conversation when known
    Restart {
        id: String,
    },
    /// Type into a session
    Send {
        id: String,
        text: Vec<String>,
        #[arg(long)]
        no_enter: bool,
        #[arg(short, long)]
        key: Vec<String>,
        /// Pause between the text and Enter, so full-screen agents do not read them as one paste (default: 150 ms, 1500 ms for agents)
        #[arg(long)]
        enter_delay_ms: Option<u64>,
    },
    Logs {
        id: String,
        #[arg(short = 'n', long, default_value_t = 65536)]
        bytes: u64,
        #[arg(long)]
        raw: bool,
    },
    /// Current screen as plain text
    Screen {
        id: String,
    },
    /// Commands typed into a session
    History {
        id: String,
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: i64,
    },
    Events {
        id: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: i64,
    },
    Set {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(short, long)]
        label: Vec<String>,
        #[arg(long)]
        restart: Option<String>,
    },
    /// Record the agent's own session id (for hooks): dpty tag-agent $DPTY_SESSION <id>
    TagAgent {
        id: String,
        agent_session_id: String,
    },
    /// Wait until text shows up in a session
    Wait {
        id: String,
        pattern: String,
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// MCP server over stdio (default) or streamable HTTP + SSE with --http
    Mcp {
        #[arg(long)]
        http: Option<String>,
        #[arg(long)]
        no_auth: bool,
    },
    /// Run the daemon at login and restart it if it exits (launchd on macOS, systemd --user on Linux)
    Install {
        #[arg(long, default_value = DEFAULT_HTTP)]
        http: String,
        #[arg(long)]
        no_http: bool,
    },
    Uninstall,
}

fn kv(items: &[String], what: &str) -> Result<BTreeMap<String, String>> {
    items
        .iter()
        .map(|s| {
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow::anyhow!("{what} must be KEY=VALUE, got '{s}'"))
        })
        .collect()
}

/// Variables an agent sets to describe ITS OWN session (id, parent link, socket). A child agent started with them thinks
/// it is a sub-session: Claude Code, for one, stops saving its transcript. They never belong in a new session.
fn is_session_marker(key: &str) -> bool {
    key == "CLAUDECODE"
        || (key.starts_with("CLAUDE_CODE_")
            && [
                "SESSION",
                "MESSAGING",
                "ENTRYPOINT",
                "EXECPATH",
                "PARENT",
                "BRIDGE",
                "SOCKET",
            ]
            .iter()
            .any(|m| key.contains(m)))
        || (key.starts_with("CODEX_")
            && ["SESSION", "THREAD", "PARENT"]
                .iter()
                .any(|m| key.contains(m)))
}

fn capture_env(all: bool) -> BTreeMap<String, String> {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "SHELL",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "SSH_AUTH_SOCK",
        "TERM_PROGRAM",
    ];
    const PREFIX: &[&str] = &[
        "XDG_",
        "ANTHROPIC_",
        "OPENAI_",
        "CODEX_",
        "CLAUDE_",
        "XAI_",
        "GROK_",
        "GOOGLE_",
        "GEMINI_",
        "NVM_",
        "VOLTA_",
    ];
    std::env::vars()
        .filter(|(k, _)| {
            (all || KEEP.contains(&k.as_str()) || PREFIX.iter().any(|p| k.starts_with(p)))
                && !is_session_marker(k)
        })
        .collect()
}

fn age(ms: i64) -> String {
    let s = (util::now_ms() - ms).max(0) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

async fn call(method: &str, params: Value) -> Result<Value> {
    Client::connect_or_start().await?.call(method, params).await
}

fn term_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    for fd in [1, 0] {
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (120, 32)
}

struct RawMode(libc::termios);
impl RawMode {
    fn enable() -> Option<RawMode> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Some(RawMode(saved))
        }
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

/// Where the detach key (Ctrl-\) starts in typed input. Terminals in an extended keyboard mode (which full-screen agents
/// switch on) send it as an escape sequence instead of the single byte.
fn find_detach(d: &[u8]) -> Option<usize> {
    const FORMS: [&[u8]; 3] = [b"\x1c", b"\x1b[92;5u", b"\x1b[27;5;92~"];
    FORMS
        .iter()
        .filter_map(|f| d.windows(f.len()).position(|w| w == *f))
        .min()
}

/// Follows whether the program has the terminal on its alternate screen, so a detach can switch back.
#[derive(Default)]
struct AltScreen {
    tail: Vec<u8>,
    on: bool,
}

impl AltScreen {
    fn feed(&mut self, d: &[u8]) {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(d);
        let last = |pats: [&[u8]; 3]| {
            pats.iter()
                .filter_map(|p| buf.windows(p.len()).rposition(|w| w == *p))
                .max()
        };
        let on = last([b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"]);
        let off = last([b"\x1b[?1049l", b"\x1b[?1047l", b"\x1b[?47l"]);
        match (on, off) {
            (Some(a), Some(b)) => self.on = a > b,
            (Some(_), None) => self.on = true,
            (None, Some(_)) => self.on = false,
            (None, None) => {}
        }
        self.tail = buf[buf.len().saturating_sub(8)..].to_vec();
    }
}

/// Puts the terminal back the way a shell expects it: the session's program set these modes, and it is still running.
const TERMINAL_RESET: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[<u\x1b[>4;0m\x1b[0m\x1b[?25h";

async fn attach(id: &str, history: bool) -> Result<()> {
    let mut c = Client::connect_or_start().await?;
    let (cols, rows) = term_size();
    let info = c
        .call(
            "attach",
            json!({"id": id, "cols": cols, "rows": rows, "history": history}),
        )
        .await?;
    let name = info["session"]["name"].as_str().unwrap_or(id).to_string();
    let mut reader = c.reader;
    let mut writer = c.writer;
    let raw = RawMode::enable();
    let (ftx, mut frx) = mpsc::channel::<(u8, Vec<u8>)>(64);
    tokio::spawn(async move {
        while let Ok(f) = read_frame(&mut reader).await {
            if ftx.send(f).await.is_err() {
                break;
            }
        }
    });
    let (itx, mut irx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut si = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match si.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if itx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut out = tokio::io::stdout();
    let mut note = String::from("detached");
    let mut alt = AltScreen::default();
    loop {
        tokio::select! {
            f = frx.recv() => match f {
                Some((S_DATA, d)) => { alt.feed(&d); out.write_all(&d).await?; out.flush().await?; }
                Some((S_EXIT, d)) => {
                    let code = if d.len() == 4 { i32::from_be_bytes([d[0], d[1], d[2], d[3]]) } else { -1 };
                    note = format!("process exited with code {code}");
                    break;
                }
                _ => { note = "connection closed".into(); break }
            },
            i = irx.recv() => match i {
                Some(d) => {
                    if let Some(pos) = find_detach(&d) {
                        if pos > 0 { write_frame(&mut writer, C_INPUT, &d[..pos]).await?; }
                        let _ = write_frame(&mut writer, C_DETACH, &[]).await;
                        break;
                    }
                    write_frame(&mut writer, C_INPUT, &d).await?;
                }
                None => break,
            },
            _ = winch.recv() => {
                let (c, r) = term_size();
                let mut p = c.to_be_bytes().to_vec();
                p.extend(r.to_be_bytes());
                write_frame(&mut writer, C_RESIZE, &p).await?;
            }
        }
    }
    if alt.on {
        let _ = out.write_all(b"\x1b[?1049l").await;
    }
    let _ = out.write_all(TERMINAL_RESET).await;
    let _ = out.flush().await;
    drop(raw);
    eprintln!("\r\n[{note}: {name}]");
    std::process::exit(0);
}

fn print_processes(rows: &[Value]) {
    println!("\n{:<8} {:<10} SESSION", "PID", "ROLE");
    if rows.is_empty() {
        println!("(none)");
        return;
    }
    for p in rows {
        let session = p["session"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("-");
        let role = p["role"].as_str().unwrap_or("-");
        let pid = p["pid"]
            .as_u64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        let cmd = p["cmd"].as_str().unwrap_or("");
        // A daemon, or a shim or client this daemon owns, is identified by its role and session.
        // Anything else prints the command, so a stray shim or a zombie is visible.
        if role == "daemon" || (session != "-" && matches!(role, "shim" | "client")) {
            println!("{pid:<8} {role:<10} {session}");
        } else {
            println!("{pid:<8} {role:<10} {session:<18} {cmd}");
        }
    }
}

fn print_table(sessions: &[Value]) {
    println!(
        "{:<9} {:<18} {:<8} {:<8} {:<7} {:<5} {:<30} {:<38} CWD",
        "ID", "NAME", "KIND", "STATUS", "PID", "AGE", "TITLE", "AGENT SESSION"
    );
    for s in sessions {
        println!(
            "{:<9} {:<18} {:<8} {:<8} {:<7} {:<5} {:<30} {:<38} {}",
            s["id"].as_str().unwrap_or(""),
            s["name"].as_str().unwrap_or(""),
            s["kind"].as_str().unwrap_or(""),
            s["status"].as_str().unwrap_or(""),
            s["pid"]
                .as_u64()
                .map(|p| p.to_string())
                .unwrap_or("-".into()),
            age(s["started_ms"].as_i64().unwrap_or(0)),
            s["title"]
                .as_str()
                .map(|t| t.chars().take(30).collect::<String>())
                .unwrap_or("-".into()),
            s["agent_session_id"].as_str().unwrap_or("-"),
            s["cwd"].as_str().unwrap_or(""),
        );
    }
}

fn main() {
    if let Err(e) = start(Cli::parse()) {
        eprintln!("dpty: {e}");
        std::process::exit(1);
    }
}

fn start(cli: Cli) -> Result<()> {
    // Commands that only print: when the reader goes away (`dpty ls | head`), end quietly like any other tool.
    if matches!(
        &cli.cmd,
        Some(
            Cmd::Ls { .. }
                | Cmd::Logs { .. }
                | Cmd::Screen { .. }
                | Cmd::History { .. }
                | Cmd::Events { .. }
                | Cmd::Status
                | Cmd::Token
        )
    ) {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    // Shims use blocking I/O and their own threads, so they do not need a Tokio runtime.
    if let Some(Cmd::Shim { spec }) = &cli.cmd {
        return shim::run(spec);
    }
    let mut builder = if matches!(&cli.cmd, Some(Cmd::Serve { .. })) {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    builder.enable_all().build()?.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd.unwrap_or(Cmd::Browse) {
        Cmd::Serve { http, no_http } => daemon::serve((!no_http).then_some(http)).await?,
        Cmd::Shim { spec } => shim::run(&spec)?,
        Cmd::Stop { sessions } => match Client::connect().await {
            Ok(mut c) => {
                c.call("shutdown", json!({"kill_sessions": sessions}))
                    .await?;
                println!(
                    "{}",
                    if sessions {
                        "stopped, and every session was stopped"
                    } else {
                        "stopped (sessions keep running and are picked up again on next start)"
                    }
                );
            }
            Err(_) => println!("not running"),
        },
        Cmd::Status => match Client::connect().await {
            Ok(mut c) => println!(
                "{}",
                serde_json::to_string_pretty(&c.call("status", json!({})).await?)?
            ),
            Err(_) => {
                println!("not running");
                std::process::exit(3);
            }
        },
        Cmd::Token => println!("{}", paths::token()),
        Cmd::Browse => browser::run().await?,
        Cmd::Ls {
            json: as_json,
            label,
            status,
        } => {
            let r = call(
                "list",
                json!({"labels": kv(&label, "label")?, "status": status}),
            )
            .await?;
            let list = r["sessions"].as_array().cloned().unwrap_or_default();
            if as_json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("no sessions");
            } else {
                print_table(&list);
            }
            if !as_json {
                match call("ps", json!({})).await {
                    Ok(r) => {
                        print_processes(r["processes"].as_array().map(Vec::as_slice).unwrap_or(&[]))
                    }
                    Err(e) => eprintln!("dpty processes: {e}"),
                }
            }
        }
        Cmd::New {
            name,
            cwd,
            label,
            env,
            env_all,
            detach,
            restart,
            kind,
            resume_cmd,
            mut cmd,
        } => {
            let mut envm = capture_env(env_all);
            envm.extend(kv(&env, "env")?);
            let cwd = cwd
                .map(|c| {
                    std::fs::canonicalize(&c)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or(c)
                })
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                });
            let (c, args) = if cmd.is_empty() {
                (None, vec![])
            } else {
                (Some(cmd.remove(0)), cmd)
            };
            let (cols, rows) = term_size();
            let r = call(
                "new",
                json!({"name": name, "cmd": c, "args": args, "cwd": cwd, "env": envm, "labels": kv(&label, "label")?, "restart": restart,
                       "kind": kind, "resume_cmd": resume_cmd, "cols": cols, "rows": rows}),
            )
            .await?;
            let id = r["id"].as_str().unwrap_or("").to_string();
            if detach || !std::io::stdout().is_terminal() {
                println!("{id}");
            } else {
                attach(&id, true).await?;
            }
        }
        Cmd::Attach { id, no_history } => attach(&id, !no_history).await?,
        Cmd::Kill { id, signal } => {
            call("kill", json!({"id": id, "signal": signal})).await?;
        }
        Cmd::Rm { id } => {
            call("rm", json!({"id": id})).await?;
        }
        Cmd::Restart { id } => {
            call("restart", json!({"id": id})).await?;
        }
        Cmd::Send {
            id,
            text,
            no_enter,
            key,
            enter_delay_ms,
        } => {
            let t = text.join(" ");
            call("send", json!({"id": id, "text": t, "enter": !no_enter && !t.is_empty(), "keys": key, "enter_delay_ms": enter_delay_ms})).await?;
        }
        Cmd::Logs { id, bytes, raw } => {
            if raw {
                use base64::Engine;
                let r = call("logs", json!({"id": id, "bytes": bytes, "raw": true})).await?;
                let b = base64::engine::general_purpose::STANDARD
                    .decode(r["b64"].as_str().unwrap_or(""))?;
                tokio::io::stdout().write_all(&b).await?;
            } else {
                print!(
                    "{}",
                    call("logs", json!({"id": id, "bytes": bytes})).await?["text"]
                        .as_str()
                        .unwrap_or("")
                );
            }
        }
        Cmd::Screen { id } => print!(
            "{}",
            call("screen", json!({"id": id})).await?["text"]
                .as_str()
                .unwrap_or("")
        ),
        Cmd::History { id, limit } => {
            for c in call("commands", json!({"id": id, "limit": limit})).await?["commands"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                println!(
                    "{}  {}",
                    age(c["ts"].as_i64().unwrap_or(0)),
                    c["text"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Events { id, limit } => {
            for e in call("events", json!({"id": id, "limit": limit})).await?["events"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                println!(
                    "{:>5} ago  {:<9} {:<14} {}",
                    age(e["ts"].as_i64().unwrap_or(0)),
                    e["session"].as_str().unwrap_or("-"),
                    e["kind"].as_str().unwrap_or(""),
                    e["detail"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Set {
            id,
            name,
            label,
            restart,
        } => {
            call(
                "set",
                json!({"id": id, "name": name, "labels": kv(&label, "label")?, "restart": restart}),
            )
            .await?;
        }
        Cmd::TagAgent {
            id,
            agent_session_id,
        } => {
            call(
                "set_agent",
                json!({"id": id, "agent_session_id": agent_session_id}),
            )
            .await?;
        }
        Cmd::Wait {
            id,
            pattern,
            timeout,
        } => {
            let r = call(
                "wait",
                json!({"id": id, "pattern": pattern, "timeout_ms": timeout * 1000}),
            )
            .await?;
            if r["matched"] != json!(true) {
                eprintln!("dpty: timed out");
                std::process::exit(2);
            }
        }
        Cmd::Mcp { http, no_auth } => match http {
            None => mcp::run_stdio(Arc::new(mcp::Remote)).await?,
            Some(addr) => {
                let l = tokio::net::TcpListener::bind(&addr).await?;
                let token = (!no_auth).then(paths::token);
                eprintln!(
                    "dpty mcp: http://{} (POST /mcp, GET /sse){}",
                    l.local_addr()?,
                    if token.is_some() {
                        ", bearer token required"
                    } else {
                        ""
                    }
                );
                axum::serve(l, http::mcp_router(token)).await?;
            }
        },
        Cmd::Install { http, no_http } => install::install((!no_http).then_some(http.as_str()))?,
        Cmd::Uninstall => install::uninstall()?,
    }
    if false {
        bail!("unreachable");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_key_is_found_in_every_form_a_terminal_sends() {
        assert_eq!(find_detach(b"ab\x1ccd"), Some(2));
        assert_eq!(find_detach(b"x\x1b[92;5u"), Some(1));
        assert_eq!(find_detach(b"\x1b[27;5;92~"), Some(0));
        assert_eq!(find_detach(b"plain \x1b[A"), None);
    }

    #[test]
    fn alternate_screen_is_tracked_across_chunks() {
        let mut a = AltScreen::default();
        a.feed(b"hello\x1b[?10");
        assert!(!a.on);
        a.feed(b"49hworld");
        assert!(a.on);
        a.feed(b"\x1b[?1049h then \x1b[?1049l");
        assert!(!a.on);
    }
}
