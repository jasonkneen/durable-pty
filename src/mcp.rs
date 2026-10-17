//! Minimal MCP server (JSON-RPC 2.0). Transports: stdio, streamable HTTP (POST /mcp) and legacy SSE (GET /sse + POST /messages).
use crate::client::Client;
use crate::rpc;
use crate::session::Manager;
use serde_json::{json, Value};
use std::sync::Arc;

pub mod async_trait_stub {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    pub type Fut<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;
    pub trait Backend: Send + Sync {
        fn call<'a>(&'a self, method: &'a str, params: Value) -> Fut<'a>;
    }
}
pub use async_trait_stub::Backend;

pub struct Local(pub Arc<Manager>);
impl Backend for Local {
    fn call<'a>(&'a self, method: &'a str, params: Value) -> async_trait_stub::Fut<'a> {
        Box::pin(async move { rpc::dispatch(&self.0, method, params).await })
    }
}

pub struct Remote;
impl Backend for Remote {
    fn call<'a>(&'a self, method: &'a str, params: Value) -> async_trait_stub::Fut<'a> {
        Box::pin(async move {
            let mut c = Client::connect_or_start().await.map_err(|e| e.to_string())?;
            c.call(method, params).await.map_err(|e| e.to_string())
        })
    }
}

fn tools() -> Value {
    let id = json!({"type": "string", "description": "Session id or name"});
    json!([
        {"name": "pty_list", "description": "List durable terminal sessions (running, exited, lost). Optional filter by status or labels.",
         "inputSchema": {"type": "object", "properties": {"status": {"type": "string"}, "labels": {"type": "object"}}}},
        {"name": "pty_new", "description": "Start a durable terminal session. Agent CLIs (claude, codex) are resumed automatically after a crash or reboot.",
         "inputSchema": {"type": "object", "properties": {"cmd": {"type": "string"}, "args": {"type": "array", "items": {"type": "string"}}, "cwd": {"type": "string"},
           "name": {"type": "string"}, "env": {"type": "object"}, "labels": {"type": "object"}, "restart": {"type": "string", "enum": ["never", "on-failure", "always"]},
           "cols": {"type": "integer"}, "rows": {"type": "integer"}}}},
        {"name": "pty_send", "description": "Type into a session. `text` is sent literally; set enter to press Return; `keys` takes names such as ctrl-c, esc, up, tab.",
         "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": id, "text": {"type": "string"}, "enter": {"type": "boolean"}, "keys": {"type": "array", "items": {"type": "string"}}}}},
        {"name": "pty_screen", "description": "Read what is currently on a session's screen as plain text.",
         "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": id}}},
        {"name": "pty_logs", "description": "Read recent output of a session as plain text (works for exited and lost sessions too).",
         "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": id, "bytes": {"type": "integer"}}}},
        {"name": "pty_wait", "description": "Wait until text appears on a session's screen or output, or the timeout passes.",
         "inputSchema": {"type": "object", "required": ["id", "pattern"], "properties": {"id": id, "pattern": {"type": "string"}, "timeout_ms": {"type": "integer"}}}},
        {"name": "pty_history", "description": "Commands typed into a session, with timestamps.",
         "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": id, "limit": {"type": "integer"}}}},
        {"name": "pty_restart", "description": "Relaunch a session; resumes the agent conversation when the agent session id is known.",
         "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": id}}},
        {"name": "pty_kill", "description": "Stop a session's process (optional signal such as TERM or KILL).",
         "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": id, "signal": {"type": "string"}}}}
    ])
}

fn tool_to_rpc(name: &str) -> Option<&'static str> {
    Some(match name {
        "pty_list" => "list",
        "pty_new" => "new",
        "pty_send" => "send",
        "pty_screen" => "screen",
        "pty_logs" => "logs",
        "pty_wait" => "wait",
        "pty_history" => "commands",
        "pty_restart" => "restart",
        "pty_kill" => "kill",
        _ => return None,
    })
}

/// Handles one JSON-RPC message. Returns None for notifications (no reply expected).
pub async fn handle(backend: &dyn Backend, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = match id {
        Some(i) if !i.is_null() => i,
        _ => return None,
    };
    let reply = |r: Result<Value, (i64, String)>| match r {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err((c, m)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": c, "message": m}}),
    };
    Some(match method {
        "initialize" => {
            let ver = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("2025-03-26").to_string();
            reply(Ok(json!({
                "protocolVersion": ver, "capabilities": {"tools": {}},
                "serverInfo": {"name": "durable-pty", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Durable terminals that survive crashes. Use pty_new to start, pty_screen/pty_logs to read, pty_send to type."
            })))
        }
        "ping" => reply(Ok(json!({}))),
        "tools/list" => reply(Ok(json!({"tools": tools()}))),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let Some(rpc_method) = tool_to_rpc(name) else {
                return Some(reply(Err((-32602, format!("unknown tool '{name}'")))));
            };
            let out = match backend.call(rpc_method, args).await {
                Ok(v) => {
                    let text = match (&v["text"], &v["screen"]) {
                        (Value::String(t), _) => t.clone(),
                        _ => serde_json::to_string_pretty(&v).unwrap_or_default(),
                    };
                    json!({"content": [{"type": "text", "text": text}], "isError": false})
                }
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            };
            reply(Ok(out))
        }
        _ => reply(Err((-32601, format!("method not found: {method}")))),
    })
}

pub async fn run_stdio(backend: Arc<dyn Backend>) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let e = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}});
            out.write_all(format!("{e}\n").as_bytes()).await?;
            continue;
        };
        let replies: Vec<Value> = if let Value::Array(items) = msg {
            let mut v = vec![];
            for m in items {
                if let Some(r) = handle(&*backend, m).await {
                    v.push(r);
                }
            }
            v
        } else {
            handle(&*backend, msg).await.into_iter().collect()
        };
        for r in replies {
            out.write_all(format!("{r}\n").as_bytes()).await?;
        }
        out.flush().await?;
    }
    Ok(())
}
