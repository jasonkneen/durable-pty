mod agents;
mod client;
mod daemon;
mod db;
mod http;
mod install;
mod mcp;
mod model;
mod paths;
mod proto;
mod rpc;
mod server;
mod session;
mod util;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use client::Client;
use proto::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const DEFAULT_HTTP: &str = "127.0.0.1:7878";

#[derive(Parser)]
#[command(name = "dpty", version, about = "Durable PTY sessions: daemon, CLI and MCP server in one binary")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
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
    /// Stop the daemon. Sessions are recorded and restored on the next start
    Stop,
    /// Daemon status
    Status,
    /// Print the bearer token for the HTTP/WebSocket/MCP listener
    Token,
    /// List sessions
    Ls {
        #[arg(long)]
        json: bool,
        #[arg(short, long)]
        label: Vec<String>,
        #[arg(long)]
        status: Option<String>,
    },
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
    Attach { id: String },
    Kill {
        id: String,
        #[arg(short, long)]
        signal: Option<String>,
    },
    /// Delete a session and its history
    Rm { id: String },
    /// Relaunch a session, resuming the agent conversation when known
    Restart { id: String },
    /// Type into a session
    Send {
        id: String,
        text: Vec<String>,
        #[arg(long)]
        no_enter: bool,
        #[arg(short, long)]
        key: Vec<String>,
    },
    Logs {
        id: String,
        #[arg(short = 'n', long, default_value_t = 65536)]
        bytes: u64,
        #[arg(long)]
        raw: bool,
    },
    /// Current screen as plain text
    Screen { id: String },
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
    TagAgent { id: String, agent_session_id: String },
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
        .map(|s| s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| anyhow::anyhow!("{what} must be KEY=VALUE, got '{s}'")))
        .collect()
}

fn capture_env(all: bool) -> BTreeMap<String, String> {
    const KEEP: &[&str] = &["PATH", "HOME", "SHELL", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "SSH_AUTH_SOCK", "TERM_PROGRAM"];
    const PREFIX: &[&str] = &["XDG_", "ANTHROPIC_", "OPENAI_", "CODEX_", "CLAUDE_", "XAI_", "GROK_", "GOOGLE_", "GEMINI_", "NVM_", "VOLTA_"];
    std::env::vars().filter(|(k, _)| all || KEEP.contains(&k.as_str()) || PREFIX.iter().any(|p| k.starts_with(p))).collect()
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

async fn attach(id: &str) -> Result<()> {
    let mut c = Client::connect_or_start().await?;
    let (cols, rows) = term_size();
    let info = c.call("attach", json!({"id": id, "cols": cols, "rows": rows})).await?;
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
    loop {
        tokio::select! {
            f = frx.recv() => match f {
                Some((S_DATA, d)) => { out.write_all(&d).await?; out.flush().await?; }
                Some((S_EXIT, d)) => {
                    let code = if d.len() == 4 { i32::from_be_bytes([d[0], d[1], d[2], d[3]]) } else { -1 };
                    note = format!("process exited with code {code}");
                    break;
                }
                _ => { note = "connection closed".into(); break }
            },
            i = irx.recv() => match i {
                Some(d) => {
                    if let Some(pos) = d.iter().position(|&b| b == 0x1c) {
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
    drop(raw);
    eprintln!("\r\n[{note}: {name}]");
    std::process::exit(0);
}

fn print_table(sessions: &[Value]) {
    println!("{:<9} {:<18} {:<8} {:<8} {:<7} {:<5} {:<38} CWD", "ID", "NAME", "KIND", "STATUS", "PID", "AGE", "AGENT SESSION");
    for s in sessions {
        println!(
            "{:<9} {:<18} {:<8} {:<8} {:<7} {:<5} {:<38} {}",
            s["id"].as_str().unwrap_or(""),
            s["name"].as_str().unwrap_or(""),
            s["kind"].as_str().unwrap_or(""),
            s["status"].as_str().unwrap_or(""),
            s["pid"].as_u64().map(|p| p.to_string()).unwrap_or("-".into()),
            age(s["started_ms"].as_i64().unwrap_or(0)),
            s["agent_session_id"].as_str().unwrap_or("-"),
            s["cwd"].as_str().unwrap_or(""),
        );
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("dpty: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Serve { http, no_http } => daemon::serve((!no_http).then_some(http)).await?,
        Cmd::Stop => match Client::connect().await {
            Ok(mut c) => {
                c.call("shutdown", json!({})).await?;
                println!("stopped (sessions are restored on next start)");
            }
            Err(_) => println!("not running"),
        },
        Cmd::Status => match Client::connect().await {
            Ok(mut c) => println!("{}", serde_json::to_string_pretty(&c.call("status", json!({})).await?)?),
            Err(_) => {
                println!("not running");
                std::process::exit(3);
            }
        },
        Cmd::Token => println!("{}", paths::token()),
        Cmd::Ls { json: as_json, label, status } => {
            let r = call("list", json!({"labels": kv(&label, "label")?, "status": status})).await?;
            let list = r["sessions"].as_array().cloned().unwrap_or_default();
            if as_json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("no sessions");
            } else {
                print_table(&list);
            }
        }
        Cmd::New { name, cwd, label, env, env_all, detach, restart, kind, resume_cmd, mut cmd } => {
            let mut envm = capture_env(env_all);
            envm.extend(kv(&env, "env")?);
            let cwd = cwd.map(|c| std::fs::canonicalize(&c).map(|p| p.to_string_lossy().into_owned()).unwrap_or(c)).or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
            let (c, args) = if cmd.is_empty() { (None, vec![]) } else { (Some(cmd.remove(0)), cmd) };
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
                attach(&id).await?;
            }
        }
        Cmd::Attach { id } => attach(&id).await?,
        Cmd::Kill { id, signal } => {
            call("kill", json!({"id": id, "signal": signal})).await?;
        }
        Cmd::Rm { id } => {
            call("rm", json!({"id": id})).await?;
        }
        Cmd::Restart { id } => {
            call("restart", json!({"id": id})).await?;
        }
        Cmd::Send { id, text, no_enter, key } => {
            let t = text.join(" ");
            call("send", json!({"id": id, "text": t, "enter": !no_enter && !t.is_empty(), "keys": key})).await?;
        }
        Cmd::Logs { id, bytes, raw } => {
            if raw {
                use base64::Engine;
                let r = call("logs", json!({"id": id, "bytes": bytes, "raw": true})).await?;
                let b = base64::engine::general_purpose::STANDARD.decode(r["b64"].as_str().unwrap_or(""))?;
                tokio::io::stdout().write_all(&b).await?;
            } else {
                print!("{}", call("logs", json!({"id": id, "bytes": bytes})).await?["text"].as_str().unwrap_or(""));
            }
        }
        Cmd::Screen { id } => print!("{}", call("screen", json!({"id": id})).await?["text"].as_str().unwrap_or("")),
        Cmd::History { id, limit } => {
            for c in call("commands", json!({"id": id, "limit": limit})).await?["commands"].as_array().cloned().unwrap_or_default() {
                println!("{}  {}", age(c["ts"].as_i64().unwrap_or(0)), c["text"].as_str().unwrap_or(""));
            }
        }
        Cmd::Events { id, limit } => {
            for e in call("events", json!({"id": id, "limit": limit})).await?["events"].as_array().cloned().unwrap_or_default() {
                println!("{:>5} ago  {:<9} {:<14} {}", age(e["ts"].as_i64().unwrap_or(0)), e["session"].as_str().unwrap_or("-"), e["kind"].as_str().unwrap_or(""), e["detail"].as_str().unwrap_or(""));
            }
        }
        Cmd::Set { id, name, label, restart } => {
            call("set", json!({"id": id, "name": name, "labels": kv(&label, "label")?, "restart": restart})).await?;
        }
        Cmd::TagAgent { id, agent_session_id } => {
            call("set_agent", json!({"id": id, "agent_session_id": agent_session_id})).await?;
        }
        Cmd::Wait { id, pattern, timeout } => {
            let r = call("wait", json!({"id": id, "pattern": pattern, "timeout_ms": timeout * 1000})).await?;
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
                eprintln!("dpty mcp: http://{} (POST /mcp, GET /sse){}", l.local_addr()?, if token.is_some() { ", bearer token required" } else { "" });
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
