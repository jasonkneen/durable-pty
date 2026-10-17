use crate::session::{key_bytes, Manager, NewReq};
use crate::util::{now_ms, strip_ansi};
use base64::Engine;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub static HTTP_ADDR: OnceLock<String> = OnceLock::new();

fn s<'a>(p: &'a Value, k: &str) -> Result<&'a str, String> {
    p.get(k).and_then(|v| v.as_str()).ok_or_else(|| format!("missing '{k}'"))
}
fn opt_s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}
fn e<T: std::fmt::Display>(x: T) -> String {
    x.to_string()
}

pub async fn dispatch(m: &Arc<Manager>, method: &str, p: Value) -> Result<Value, String> {
    match method {
        "ping" => Ok(json!("pong")),
        "status" => {
            let list = m.list().map_err(e)?;
            let running = list.iter().filter(|v| v["running"] == json!(true)).count();
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id(), "uptime_ms": now_ms() - m.started_ms,
                "sessions": list.len(), "running": running, "home": crate::paths::home(), "http": HTTP_ADDR.get(),
            }))
        }
        "list" => {
            let mut list = m.list().map_err(e)?;
            if let Some(st) = opt_s(&p, "status") {
                list.retain(|v| v["status"] == json!(st));
            }
            if let Some(Value::Object(want)) = p.get("labels") {
                list.retain(|v| want.iter().all(|(k, val)| v["labels"].get(k) == Some(val)));
            }
            Ok(json!({ "sessions": list }))
        }
        "get" => {
            let id = m.resolve(s(&p, "id")?).map_err(e)?;
            Ok(m.describe(m.row(&id).map_err(e)?))
        }
        "new" => {
            let req: NewReq = serde_json::from_value(p).map_err(e)?;
            let row = m.create(req).await.map_err(e)?;
            Ok(m.describe(row))
        }
        "kill" => {
            m.kill(s(&p, "id")?, opt_s(&p, "signal")).map_err(e)?;
            Ok(json!({"ok": true}))
        }
        "rm" => {
            m.remove(s(&p, "id")?).map_err(e)?;
            Ok(json!({"ok": true}))
        }
        "restart" => {
            m.restart(s(&p, "id")?).await.map_err(e)?;
            Ok(json!({"ok": true}))
        }
        "send" => {
            let live = m.running(s(&p, "id")?).map_err(e)?;
            let mut bytes = vec![];
            if let Some(t) = opt_s(&p, "text") {
                bytes.extend_from_slice(t.as_bytes());
            }
            if let Some(b) = opt_s(&p, "b64") {
                bytes.extend(base64::engine::general_purpose::STANDARD.decode(b).map_err(e)?);
            }
            // Text followed by Enter in a single write is read by full-screen TUIs as a paste, and the Enter becomes a
            // newline in the input box instead of submitting (Codex waits for input to go quiet first). Send the Enter on its
            // own after a pause: `enter_delay_ms` (default 150 ms for shells, 1.5 s for agents).
            let enter = p.get("enter").and_then(|v| v.as_bool()).unwrap_or(false);
            let separate_enter = enter && !bytes.is_empty();
            if enter && !separate_enter {
                bytes.push(b'\r');
            }
            if let Some(Value::Array(keys)) = p.get("keys") {
                for k in keys {
                    let name = k.as_str().unwrap_or("");
                    bytes.extend(key_bytes(name).ok_or_else(|| format!("unknown key '{name}'"))?);
                }
            }
            live.write_input(&bytes).map_err(e)?;
            let mut sent = bytes.len();
            if separate_enter {
                // Agent TUIs need a longer quiet moment than a shell does before they treat Enter as a submit.
                let agent = m.row(&live.id).map(|r| matches!(r.kind.as_str(), "claude" | "codex" | "grok")).unwrap_or(false);
                let delay = p.get("enter_delay_ms").and_then(|v| v.as_u64()).unwrap_or(if agent { 1500 } else { 150 }).min(5_000);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                live.write_input(b"\r").map_err(e)?;
                sent += 1;
            }
            Ok(json!({"sent": sent}))
        }
        "resize" => {
            let live = m.running(s(&p, "id")?).map_err(e)?;
            let c = p.get("cols").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let r = p.get("rows").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            live.resize(c, r).map_err(e)?;
            Ok(json!({"ok": true}))
        }
        "logs" => {
            let n = p.get("bytes").and_then(|v| v.as_u64()).unwrap_or(65536) as usize;
            let raw = m.logs_raw(s(&p, "id")?, n).map_err(e)?;
            if p.get("raw").and_then(|v| v.as_bool()).unwrap_or(false) {
                Ok(json!({"b64": base64::engine::general_purpose::STANDARD.encode(raw)}))
            } else {
                Ok(json!({"text": strip_ansi(&raw)}))
            }
        }
        "screen" => Ok(json!({"text": m.screen_of(s(&p, "id")?).map_err(e)?})),
        "commands" => {
            let id = m.resolve(s(&p, "id")?).map_err(e)?;
            let n = p.get("limit").and_then(|v| v.as_i64()).unwrap_or(100);
            let rows = m.db.commands(&id, n).map_err(e)?;
            Ok(json!({"commands": rows.into_iter().map(|(ts, t)| json!({"ts": ts, "text": t})).collect::<Vec<_>>()}))
        }
        "events" => {
            let id = match opt_s(&p, "id") {
                Some(k) => Some(m.resolve(k).map_err(e)?),
                None => None,
            };
            let n = p.get("limit").and_then(|v| v.as_i64()).unwrap_or(100);
            Ok(json!({"events": m.db.events(id.as_deref(), n).map_err(e)?}))
        }
        "set" => {
            let labels = match p.get("labels") {
                Some(v) if v.is_object() => Some(serde_json::from_value(v.clone()).map_err(e)?),
                _ => None,
            };
            m.set_meta(s(&p, "id")?, opt_s(&p, "name").map(String::from), labels, opt_s(&p, "restart").map(String::from)).map_err(e)?;
            Ok(json!({"ok": true}))
        }
        "set_agent" => {
            m.set_agent(s(&p, "id")?, s(&p, "agent_session_id")?).map_err(e)?;
            Ok(json!({"ok": true}))
        }
        "wait" => {
            let key = s(&p, "id")?.to_string();
            let pat = s(&p, "pattern")?.to_string();
            let timeout = p.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(30_000).min(600_000);
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout);
            loop {
                let live = m.running(&key).ok();
                let text = match &live {
                    Some(l) => l.screen_text(),
                    None => m.screen_of(&key).map_err(e)?,
                };
                let tail = m.logs_raw(&key, 16384).map(|b| strip_ansi(&b)).unwrap_or_default();
                if text.contains(&pat) || tail.contains(&pat) {
                    return Ok(json!({"matched": true, "screen": text}));
                }
                if live.is_none() || std::time::Instant::now() >= deadline {
                    return Ok(json!({"matched": false, "running": live.is_some(), "screen": text}));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        "shutdown" => {
            m.shutdown(p.get("kill_sessions").and_then(|v| v.as_bool()).unwrap_or(false));
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            Ok(json!({"ok": true}))
        }
        other => Err(format!("unknown method '{other}'")),
    }
}
