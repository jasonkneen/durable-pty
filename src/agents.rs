//! Adapters for agent CLIs. An agent's own transcript is the durable state; we only need to know how to resume it
//! (the agent session id) and how to find that id when we did not choose it.
use crate::model::SessionRow;
use crate::util::basename;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "tcsh", "nu"];

pub fn detect_kind(cmd: &str) -> String {
    let b = basename(cmd);
    match b.as_str() {
        "claude" | "codex" | "grok" => b,
        s if SHELLS.contains(&s) => "shell".into(),
        _ => "process".into(),
    }
}

pub fn default_restart(kind: &str) -> &'static str {
    match kind {
        "claude" | "codex" | "grok" => "on-failure",
        _ => "never",
    }
}

fn looks_like_id(s: &str) -> bool {
    s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

const CLAUDE_VALUE_FLAGS: &[&str] = &[
    "--model", "--permission-mode", "--add-dir", "--allowedTools", "--allowed-tools", "--disallowedTools", "--disallowed-tools",
    "--mcp-config", "--append-system-prompt", "--system-prompt", "--settings", "--agent", "--max-turns", "--output-format",
    "--input-format", "--fallback-model", "--plugin-dir", "--effort", "--session-id", "--resume", "-r", "--name", "-n",
];
const CODEX_VALUE_FLAGS: &[&str] = &["-m", "--model", "-c", "--config", "-C", "--cd", "-s", "--sandbox", "-a", "--ask-for-approval", "-p", "--profile", "-i", "--image"];

/// Drops the initial prompt (a positional first or last argument) so a resume does not resend it.
fn drop_prompt(args: Vec<String>, value_flags: &[&str]) -> Vec<String> {
    let mut v = args;
    if v.first().is_some_and(|a| !a.starts_with('-')) {
        v.remove(0);
    }
    if v.last().is_some_and(|a| !a.starts_with('-')) {
        let keep = v.len() >= 2 && value_flags.contains(&v[v.len() - 2].as_str());
        if !keep {
            v.pop();
        }
    }
    v
}

/// Called once before first launch. Lets the daemon choose the agent session id when the CLI supports it.
pub fn prepare(row: &mut SessionRow) {
    if row.agent_session_id.is_some() {
        return;
    }
    match row.kind.as_str() {
        "claude" => {
            let a = &row.args;
            let mut it = a.iter().enumerate();
            while let Some((i, x)) = it.next() {
                match x.as_str() {
                    "--session-id" => {
                        row.agent_session_id = a.get(i + 1).cloned();
                        return;
                    }
                    "--resume" | "-r" => {
                        row.agent_session_id = a.get(i + 1).filter(|v| looks_like_id(v)).cloned();
                        return;
                    }
                    "--continue" | "-c" => return,
                    _ => {}
                }
            }
            let id = uuid::Uuid::new_v4().to_string();
            row.args.insert(0, id.clone());
            row.args.insert(0, "--session-id".into());
            row.agent_session_id = Some(id);
        }
        "codex" => {
            if row.args.first().map(String::as_str) == Some("resume") {
                row.agent_session_id = row.args.get(1).filter(|v| looks_like_id(v)).cloned();
            }
        }
        _ => {}
    }
}

/// Rewrites cmd/args so the next launch continues the previous agent conversation. Returns whether it did.
pub fn resume(row: &mut SessionRow) -> bool {
    let Some(id) = row.agent_session_id.clone() else { return false };
    if let Some(tpl) = row.resume_cmd.clone() {
        let line = tpl.replace("{id}", &id);
        let mut parts = shell_split(&line);
        if parts.is_empty() {
            return false;
        }
        row.cmd = parts.remove(0);
        row.args = parts;
        return true;
    }
    match row.kind.as_str() {
        "claude" => {
            let mut rest = vec![];
            let mut i = 0;
            while i < row.args.len() {
                match row.args[i].as_str() {
                    "--session-id" => i += 1,
                    "--resume" | "-r" => {
                        if row.args.get(i + 1).is_some_and(|v| !v.starts_with('-')) {
                            i += 1;
                        }
                    }
                    "--continue" | "-c" => {}
                    other => rest.push(other.to_string()),
                }
                i += 1;
            }
            let mut args = vec!["--resume".to_string(), id];
            args.extend(drop_prompt(rest, CLAUDE_VALUE_FLAGS));
            row.args = args;
            true
        }
        "codex" => {
            if row.args.first().is_some_and(|a| a == "exec") {
                return false;
            }
            let mut rest: Vec<String> = row.args.clone();
            if rest.first().map(String::as_str) == Some("resume") {
                rest.remove(0);
                if rest.first().is_some_and(|v| looks_like_id(v)) {
                    rest.remove(0);
                }
            }
            rest.retain(|a| a != "--last");
            let mut args = vec!["resume".to_string(), id];
            args.extend(drop_prompt(rest, CODEX_VALUE_FLAGS));
            row.args = args;
            true
        }
        _ => false,
    }
}

fn shell_split(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn canon(p: &str) -> String {
    std::fs::canonicalize(p).map(|c| c.to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string())
}

pub fn claude_project_dir(cwd: &str) -> PathBuf {
    let enc: String = canon(cwd).chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    home().join(".claude").join("projects").join(enc)
}

/// Finds the agent's own session id when we did not assign it: the newest transcript written for this cwd since launch,
/// ignoring ids already claimed by other durable-pty sessions.
pub fn scan(row: &SessionRow, claimed: &HashSet<String>) -> Option<String> {
    let since = row.started_ms - 5_000;
    match row.kind.as_str() {
        "claude" => {
            let dir = claude_project_dir(&row.cwd);
            let mut best: Option<(i64, String)> = None;
            for e in std::fs::read_dir(dir).ok()?.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let id = p.file_stem()?.to_string_lossy().into_owned();
                let m = mtime_ms(&p);
                if m >= since && !claimed.contains(&id) && best.as_ref().is_none_or(|(bm, _)| m > *bm) {
                    best = Some((m, id));
                }
            }
            best.map(|b| b.1)
        }
        "codex" => {
            let want = canon(&row.cwd);
            let mut best: Option<(i64, String)> = None;
            let mut stack = vec![(home().join(".codex").join("sessions"), 0)];
            while let Some((dir, depth)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        if depth < 4 {
                            stack.push((p, depth + 1));
                        }
                        continue;
                    }
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                        continue;
                    }
                    let m = mtime_ms(&p);
                    if m < since || best.as_ref().is_some_and(|(bm, _)| m <= *bm) {
                        continue;
                    }
                    let Some((id, cwd)) = codex_meta(&p) else { continue };
                    if canon(&cwd) == want && !claimed.contains(&id) {
                        best = Some((m, id));
                    }
                }
            }
            best.map(|b| b.1)
        }
        _ => None,
    }
}

fn codex_meta(p: &Path) -> Option<(String, String)> {
    use std::io::{BufRead, BufReader};
    let line = BufReader::new(std::fs::File::open(p).ok()?).lines().next()?.ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    let pl = v.get("payload").unwrap_or(&v);
    let cwd = pl.get("cwd")?.as_str()?.to_string();
    let id = pl.get("id").and_then(|i| i.as_str()).map(String::from).or_else(|| {
        let n = p.file_stem()?.to_string_lossy().into_owned();
        (n.len() >= 36).then(|| n[n.len() - 36..].to_string())
    })?;
    Some((id, cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn row(kind: &str, args: &[&str]) -> SessionRow {
        SessionRow {
            id: "i".into(), name: "n".into(), cmd: kind.into(), args: args.iter().map(|s| s.to_string()).collect(), cwd: "/tmp".into(),
            env: BTreeMap::new(), labels: BTreeMap::new(), kind: kind.into(), agent_session_id: None, resume_cmd: None,
            restart: "on-failure".into(), status: "running".into(), exit_code: None, pid: None, cols: 80, rows: 24,
            created_ms: 0, started_ms: 0, ended_ms: None, last_active_ms: 0, restarts: 0, live_cwd: None,
        }
    }

    #[test]
    fn claude_gets_session_id_and_resumes_without_prompt() {
        let mut r = row("claude", &["--dangerously-skip-permissions", "--model", "opus", "fix the bug"]);
        prepare(&mut r);
        let id = r.agent_session_id.clone().unwrap();
        assert_eq!(&r.args[..2], ["--session-id".to_string(), id.clone()]);
        assert!(resume(&mut r));
        assert_eq!(r.args, ["--resume", id.as_str(), "--dangerously-skip-permissions", "--model", "opus"]);
        assert!(resume(&mut r));
        assert_eq!(r.args.iter().filter(|a| *a == "--resume").count(), 1);
    }

    #[test]
    fn claude_resume_flag_is_adopted() {
        let mut r = row("claude", &["--resume", "0b1c2d3e-aaaa-bbbb-cccc-0123456789ab"]);
        prepare(&mut r);
        assert_eq!(r.agent_session_id.as_deref(), Some("0b1c2d3e-aaaa-bbbb-cccc-0123456789ab"));
    }

    #[test]
    fn codex_resume_form() {
        let mut r = row("codex", &["--model", "gpt-x", "do it"]);
        r.agent_session_id = Some("019a0000-1111-2222-3333-444455556666".into());
        assert!(resume(&mut r));
        assert_eq!(r.args, ["resume", "019a0000-1111-2222-3333-444455556666", "--model", "gpt-x"]);
    }

    #[test]
    fn custom_template() {
        let mut r = row("grok", &[]);
        r.agent_session_id = Some("abc12345".into());
        r.resume_cmd = Some("grok --resume {id}".into());
        assert!(resume(&mut r));
        assert_eq!((r.cmd.as_str(), r.args.as_slice()), ("grok", ["--resume".to_string(), "abc12345".to_string()].as_slice()));
    }
}
