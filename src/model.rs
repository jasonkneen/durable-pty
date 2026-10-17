use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub name: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    /// claude | codex | grok | shell | process
    pub kind: String,
    pub agent_session_id: Option<String>,
    /// Custom resume command template for agents we have no adapter for; `{id}` is the agent session id.
    pub resume_cmd: Option<String>,
    /// never | on-failure | always
    pub restart: String,
    /// running | exited | lost
    pub status: String,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub created_ms: i64,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub last_active_ms: i64,
    pub restarts: u32,
    /// Where a shell or plain process currently is, so a restart lands in the same directory.
    #[serde(default)]
    pub live_cwd: Option<String>,
}
