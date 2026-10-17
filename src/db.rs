use crate::model::SessionRow;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, cmd TEXT NOT NULL, args TEXT NOT NULL, cwd TEXT NOT NULL,
  env TEXT NOT NULL, labels TEXT NOT NULL, kind TEXT NOT NULL, agent_session_id TEXT, resume_cmd TEXT,
  restart TEXT NOT NULL, status TEXT NOT NULL, exit_code INTEGER, pid INTEGER, cols INTEGER NOT NULL,
  rows INTEGER NOT NULL, created_ms INTEGER NOT NULL, started_ms INTEGER NOT NULL, ended_ms INTEGER,
  last_active_ms INTEGER NOT NULL, restarts INTEGER NOT NULL DEFAULT 0, last_screen BLOB, live_cwd TEXT
);
CREATE TABLE IF NOT EXISTS output (
  session_id TEXT NOT NULL, seq INTEGER NOT NULL, ts INTEGER NOT NULL, data BLOB NOT NULL,
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS commands (
  id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, ts INTEGER NOT NULL, text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS commands_session ON commands(session_id, id);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, session_id TEXT, kind TEXT NOT NULL, detail TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

const COLS: &str = "id,name,cmd,args,cwd,env,labels,kind,agent_session_id,resume_cmd,restart,status,exit_code,pid,cols,rows,created_ms,started_ms,ended_ms,last_active_ms,restarts,live_cwd";

fn row_to_session(r: &Row) -> rusqlite::Result<SessionRow> {
    let json = |i: usize| -> rusqlite::Result<String> { r.get(i) };
    Ok(SessionRow {
        id: r.get(0)?,
        name: r.get(1)?,
        cmd: r.get(2)?,
        args: serde_json::from_str(&json(3)?).unwrap_or_default(),
        cwd: r.get(4)?,
        env: serde_json::from_str(&json(5)?).unwrap_or_default(),
        labels: serde_json::from_str(&json(6)?).unwrap_or_default(),
        kind: r.get(7)?,
        agent_session_id: r.get(8)?,
        resume_cmd: r.get(9)?,
        restart: r.get(10)?,
        status: r.get(11)?,
        exit_code: r.get(12)?,
        pid: r.get::<_, Option<i64>>(13)?.map(|p| p as u32),
        cols: r.get::<_, i64>(14)? as u16,
        rows: r.get::<_, i64>(15)? as u16,
        created_ms: r.get(16)?,
        started_ms: r.get(17)?,
        ended_ms: r.get(18)?,
        last_active_ms: r.get(19)?,
        restarts: r.get::<_, i64>(20)? as u32,
        live_cwd: r.get(21)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let c = Connection::open(path)?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        c.busy_timeout(Duration::from_secs(5))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        c.execute_batch(SCHEMA)?;
        // Databases created before live_cwd existed.
        let has: bool = c.prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name='live_cwd'")?.exists([])?;
        if !has {
            c.execute_batch("ALTER TABLE sessions ADD COLUMN live_cwd TEXT")?;
        }
        Ok(Db { conn: Mutex::new(c) })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn upsert_session(&self, s: &SessionRow) -> Result<()> {
        self.c().execute(
            &format!("INSERT INTO sessions ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
             ON CONFLICT(id) DO UPDATE SET name=?2,cmd=?3,args=?4,cwd=?5,env=?6,labels=?7,kind=?8,agent_session_id=?9,resume_cmd=?10,
             restart=?11,status=?12,exit_code=?13,pid=?14,cols=?15,rows=?16,started_ms=?18,ended_ms=?19,last_active_ms=?20,restarts=?21,live_cwd=?22"),
            params![
                s.id, s.name, s.cmd, serde_json::to_string(&s.args)?, s.cwd, serde_json::to_string(&s.env)?,
                serde_json::to_string(&s.labels)?, s.kind, s.agent_session_id, s.resume_cmd, s.restart, s.status,
                s.exit_code, s.pid.map(|p| p as i64), s.cols as i64, s.rows as i64, s.created_ms, s.started_ms,
                s.ended_ms, s.last_active_ms, s.restarts as i64, s.live_cwd
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM sessions ORDER BY created_ms"))?;
        let rows = st.query_map([], row_to_session)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        Ok(self.c().query_row(&format!("SELECT {COLS} FROM sessions WHERE id=?1"), [id], row_to_session).optional()?)
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let c = self.c();
        c.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        c.execute("DELETE FROM output WHERE session_id=?1", [id])?;
        c.execute("DELETE FROM commands WHERE session_id=?1", [id])?;
        Ok(())
    }

    pub fn set_screen(&self, id: &str, screen: &[u8]) -> Result<()> {
        self.c().execute("UPDATE sessions SET last_screen=?2 WHERE id=?1", params![id, screen])?;
        Ok(())
    }

    pub fn get_screen(&self, id: &str) -> Result<Vec<u8>> {
        Ok(self
            .c()
            .query_row("SELECT last_screen FROM sessions WHERE id=?1", [id], |r| r.get::<_, Option<Vec<u8>>>(0))
            .optional()?
            .flatten()
            .unwrap_or_default())
    }

    pub fn append_output(&self, batch: &[(String, i64, i64, Vec<u8>)]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT OR REPLACE INTO output (session_id,seq,ts,data) VALUES (?1,?2,?3,?4)")?;
            for (id, seq, ts, data) in batch {
                st.execute(params![id, seq, ts, data])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn max_seq(&self, id: &str) -> Result<i64> {
        Ok(self.c().query_row("SELECT COALESCE(MAX(seq),0) FROM output WHERE session_id=?1", [id], |r| r.get(0))?)
    }

    pub fn tail_output(&self, id: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let c = self.c();
        let mut st = c.prepare("SELECT data FROM output WHERE session_id=?1 ORDER BY seq DESC")?;
        let mut rows = st.query([id])?;
        let mut chunks: Vec<Vec<u8>> = vec![];
        let mut total = 0usize;
        while let Some(r) = rows.next()? {
            let d: Vec<u8> = r.get(0)?;
            total += d.len();
            chunks.push(d);
            if total >= max_bytes {
                break;
            }
        }
        chunks.reverse();
        let mut out: Vec<u8> = chunks.concat();
        if out.len() > max_bytes {
            out.drain(..out.len() - max_bytes);
        }
        Ok(out)
    }

    /// Keeps roughly the newest `cap` bytes of output for the session.
    pub fn prune_output(&self, id: &str, cap: i64) -> Result<()> {
        self.c().execute(
            "DELETE FROM output WHERE session_id=?1 AND seq <= COALESCE((
               SELECT seq FROM (SELECT seq, SUM(LENGTH(data)) OVER (ORDER BY seq DESC) AS s FROM output WHERE session_id=?1)
               WHERE s > ?2 ORDER BY seq DESC LIMIT 1), -1)",
            params![id, cap],
        )?;
        Ok(())
    }

    pub fn add_command(&self, id: &str, ts: i64, text: &str) -> Result<()> {
        self.c().execute("INSERT INTO commands (session_id,ts,text) VALUES (?1,?2,?3)", params![id, ts, text])?;
        Ok(())
    }

    pub fn commands(&self, id: &str, limit: i64) -> Result<Vec<(i64, String)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT ts,text FROM commands WHERE session_id=?1 ORDER BY id DESC LIMIT ?2")?;
        let mut v = st.query_map(params![id, limit], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        v.reverse();
        Ok(v)
    }

    pub fn add_event(&self, session: Option<&str>, kind: &str, detail: &str) {
        let _ = self.c().execute(
            "INSERT INTO events (ts,session_id,kind,detail) VALUES (?1,?2,?3,?4)",
            params![crate::util::now_ms(), session, kind, detail],
        );
    }

    pub fn events(&self, session: Option<&str>, limit: i64) -> Result<Vec<serde_json::Value>> {
        let c = self.c();
        let mut st = c.prepare("SELECT ts,session_id,kind,detail FROM events WHERE (?1 IS NULL OR session_id=?1) ORDER BY id DESC LIMIT ?2")?;
        let mut v = st
            .query_map(params![session, limit], |r| {
                Ok(serde_json::json!({"ts": r.get::<_, i64>(0)?, "session": r.get::<_, Option<String>>(1)?, "kind": r.get::<_, String>(2)?, "detail": r.get::<_, String>(3)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v.reverse();
        Ok(v)
    }
}
