mod hook_impl;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use thiserror::Error;
use turya_protocol::{Part, SessionMeta, Turn, TurnId};

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
    /// No session with that id. Never an empty result: a silent success here
    /// reads as "the session existed and was empty".
    #[error("no session with id '{0}'; run `turya sessions --all` to list stored ids")]
    UnknownSession(String),
    /// A database written by another format version. Never migrated
    /// (AGENTS.md Rule 5.4): the old file is backed up and a fresh one
    /// created, and the user is told what happened.
    #[error("session store format {found} is not {current}; see {backup}")]
    FormatMismatch {
        found: String,
        current: String,
        backup: String,
    },
}

/// Current durable format. Bump freely; a mismatch is rejected, never
/// migrated. Stamped on first write so a foreign file is recognisable.
pub const FORMAT_VERSION: &str = "v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRule {
    pub id: i64,
    pub scope: String,
    pub rule: String,
}

/// Episodic + long-term memory backed by SQLite.
///
/// Layout (single source of truth, one writer):
/// - `sessions`: one header row per session (metadata, not conversation)
/// - `turns`: append-only log, one row per turn, contiguous `seq`
/// - `memory_rules`: reflection-extracted rules injected via `pre_turn`
///
/// A turn is written in a single transaction, so a crash can never leave a
/// half-written turn: the log is either missing a turn or has all of it. A
/// *semantically* interrupted turn (tool call with no result) is repaired on
/// read, never truncated away — losing valid work is worse than closing it.
pub struct MemoryStore {
    /// `rusqlite::Connection` is `Send` but not `Sync`; the hook trait needs
    /// both. A mutex gives us that *and* enforces the single-writer rule the
    /// log's contiguous sequence depends on.
    conn: Mutex<Connection>,
}

impl MemoryStore {
    /// Open (or create) a store, recovering from a foreign/legacy file per
    /// Rule 5.3. Returns a note when a file was moved aside.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Option<String>), MemoryError> {
        let path = path.as_ref().to_path_buf();
        let existed_with_data = path.exists() && file_is_nonempty(&path);
        let mut note = None;

        if existed_with_data {
            match probe_format(&path) {
                Ok(ref found) if found == FORMAT_VERSION => {}
                Ok(found) => {
                    // Version tag present but different, or no tag at all on
                    // a pre-A1 database: back up, then start fresh.
                    let backup = backup_path(&path, &found);
                    std::fs::rename(&path, &backup).ok();
                    for suffix in ["-wal", "-shm"] {
                        let side = path.with_extension(format!(
                            "{}{suffix}",
                            path.extension().and_then(|e| e.to_str()).unwrap_or("db")
                        ));
                        let _ = std::fs::remove_file(side);
                    }
                    note = Some(format!(
                        "existing turya.db was format {found}: backed up to {} and started fresh \
                         (forward-only: no migration, AGENTS.md Rule 5.4)",
                        backup.display()
                    ));
                }
                Err(e) => return Err(e),
            }
        }

        let conn = Connection::open(&path)?;
        // A wedged writer must not surface as an instant failure: a single
        // long-lived process is the norm here, but a stale connection from a
        // previous crash can still hold the lock briefly.
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init()?;
        harden_permissions(&path);
        Ok((store, note))
    }

    pub fn open_in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<(), MemoryError> {
        let conn = self.conn.lock().unwrap();
        // WAL is a no-op for :memory: but harmless; best-effort only.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS store_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                parent_id TEXT,
                seq INTEGER NOT NULL DEFAULT 0,
                repaired INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_audit_session ON audit(session_id, id);
            CREATE TABLE IF NOT EXISTS turns (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                turn_id TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (session_id, seq)
            );
            CREATE TABLE IF NOT EXISTS memory_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope TEXT NOT NULL DEFAULT 'project',
                rule TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO store_meta (key, value) VALUES ('format_version', ?1)",
            params![FORMAT_VERSION],
        )?;
        Ok(())
    }

    // ---- session log ----

    /// Create the session row if absent; return the header either way.
    pub fn begin_session(
        &self,
        session_id: &str,
        cwd: &str,
        title: &str,
    ) -> Result<SessionMeta, MemoryError> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO sessions (id, cwd, title) VALUES (?1, ?2, ?3)",
                params![session_id, cwd, title],
            )?;
            if !title.is_empty() {
                conn.execute(
                    "UPDATE sessions SET title = ?2 WHERE id = ?1 AND title = ''",
                    params![session_id, title],
                )?;
            }
        }
        Self::meta_from(&self.conn.lock().unwrap(), session_id)?
            .ok_or_else(|| MemoryError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// Append one turn. Single transaction; refuses to commit a sequence gap
    /// so the log stays contiguous and replayable.
    pub fn append_turn(&self, session_id: &str, turn: &Turn) -> Result<(), MemoryError> {
        let conn = self.conn.lock().unwrap();
        let body = serde_json::to_string(turn)?;
        // Transactional append: the sequence bump and the row land together,
        // so the log can never commit a gap. `unchecked_transaction` takes
        // `&self`; the exclusive write lock is taken by the first INSERT.
        let tx = conn.unchecked_transaction()?;
        let current: i64 = tx
            .query_row(
                "SELECT seq FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| MemoryError::Sqlite(rusqlite::Error::InvalidQuery))?;
        let next = current + 1;
        tx.execute(
            "INSERT INTO turns (session_id, seq, turn_id, body) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, next, turn.id.0, body],
        )?;
        tx.execute(
            "UPDATE sessions SET seq = ?2, updated_at = datetime('now') WHERE id = ?1",
            params![session_id, next],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replay the log, repairing an interrupted tail.
    ///
    /// A `ToolCall` with no matching `ToolResult` would be replayed as a
    /// request the provider rejects (Anthropic requires a `tool_result` for
    /// every `tool_use`). Rather than dropping the turn, we close the call
    /// with a synthetic result and mark the session repaired — the work the
    /// model did before the crash is preserved.
    pub fn load_transcript(&self, session_id: &str) -> Result<Vec<Turn>, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT body FROM turns WHERE session_id = ?1 ORDER BY seq ASC")?;
        let rows = stmt.query_map(params![session_id], |row| row.get::<_, String>(0))?;
        let mut turns: Vec<Turn> = Vec::new();
        for row in rows {
            let body = row?;
            let turn: Turn = serde_json::from_str(&body)?;
            turns.push(turn);
        }
        let mut repaired = false;
        for turn in turns.iter_mut() {
            repaired |= close_interrupted_calls(turn);
        }
        if repaired {
            // Persist the repair so the next load is clean, and so the user
            // can see the session was closed mid-flight.
            conn.execute(
                "UPDATE sessions SET repaired = 1 WHERE id = ?1",
                params![session_id],
            )?;
            for turn in turns.iter() {
                let seq = conn
                    .query_row(
                        "SELECT seq FROM turns WHERE session_id = ?1 AND turn_id = ?2",
                        params![session_id, turn.id.0],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                if let Some(seq) = seq {
                    conn.execute(
                        "UPDATE turns SET body = ?3 WHERE session_id = ?1 AND seq = ?2",
                        params![session_id, seq, serde_json::to_string(turn)?],
                    )?;
                }
            }
        }
        Ok(turns)
    }

    pub fn session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, MemoryError> {
        Self::meta_from(&self.conn.lock().unwrap(), session_id)
    }

    /// Read one header with an already-held lock (callers inside the store
    /// must not re-lock: the mutex is not reentrant).
    fn meta_from(conn: &Connection, session_id: &str) -> Result<Option<SessionMeta>, MemoryError> {
        let row = conn
            .query_row(
                "SELECT id, cwd, title, parent_id, seq, repaired,
                        created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionMeta {
                        id: row.get(0)?,
                        cwd: row.get(1)?,
                        title: row.get(2)?,
                        parent_id: row.get(3)?,
                        seq: row.get::<_, i64>(4)? as u32,
                        repaired: row.get::<_, i64>(5)? != 0,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionMeta>, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let ids: Vec<String> = match cwd {
            Some(dir) => {
                let mut stmt = conn.prepare(
                    "SELECT id FROM sessions WHERE cwd = ?1 ORDER BY updated_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![dir, limit as i64], |r| r.get(0))?;
                rows.collect::<Result<Vec<String>, _>>()?
            }
            None => {
                let mut stmt =
                    conn.prepare("SELECT id FROM sessions ORDER BY updated_at DESC LIMIT ?1")?;
                let rows = stmt.query_map(params![limit as i64], |r| r.get(0))?;
                rows.collect::<Result<Vec<String>, _>>()?
            }
        };
        for id in ids {
            if let Some(meta) = Self::meta_from(&conn, &id)? {
                out.push(meta);
            }
        }
        Ok(out)
    }

    /// Keyword search over stored turn bodies (post-compaction recall).
    /// Plain LIKE: no FTS5 dependency, no index drift, honest behaviour.
    pub fn search(
        &self,
        session_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let needle = format!("%{}%", query.trim());
        let mut stmt = conn.prepare(
            "SELECT turn_id, body FROM turns
             WHERE session_id = ?1 AND body LIKE ?2 ESCAPE '\\'
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, needle, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (turn_id, body) = row?;
            let turn: Turn = serde_json::from_str(&body)?;
            for part in turn.parts {
                let text = match &part {
                    Part::Text { text } | Part::UserText { text } | Part::Instruction { text } => {
                        text.clone()
                    }
                    Part::ToolResult { output, .. } => output.clone(),
                    _ => continue,
                };
                if text.to_lowercase().contains(&query.trim().to_lowercase()) {
                    out.push(format!("{turn_id}: {}", truncate(&text, 400)));
                }
            }
        }
        Ok(out)
    }

    /// Export one session as newline-delimited JSON (interoperability path;
    /// the database stays the source of truth). An unknown id is an error,
    /// not an empty file: silently producing nothing would look like a
    /// successful export of a lost session.
    pub fn export_jsonl(&self, session_id: &str) -> Result<String, MemoryError> {
        let mut out = String::new();
        let meta = self
            .session_meta(session_id)?
            .ok_or_else(|| MemoryError::UnknownSession(session_id.to_string()))?;
        out.push_str(&serde_json::to_string(&serde_json::json!({
            "type": "session_meta",
            "id": meta.id,
            "cwd": meta.cwd,
            "title": meta.title,
            "created_at": meta.created_at,
            "updated_at": meta.updated_at,
            "format_version": FORMAT_VERSION,
        }))?);
        out.push('\n');
        for turn in self.load_transcript(session_id)? {
            out.push_str(&serde_json::to_string(&serde_json::json!({
                "type": "turn",
                "id": turn.id.0,
            }))?);
            out.push('\n');
            for part in turn.parts {
                out.push_str(&serde_json::to_string(&part)?);
                out.push('\n');
            }
        }
        Ok(out)
    }

    // ---- pre-existing surfaces ----

    /// `on_event(TurnCompleted)` hook: persist one audit row.
    pub fn record_event(
        &self,
        session_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<i64, MemoryError> {
        let conn = self.conn.lock().unwrap();
        // The audit trail lives in its own table on purpose: the turn log
        // must contain exactly one shape, or a replay would have to guess
        // whether a row is a turn or an event.
        conn.execute(
            "INSERT INTO audit (session_id, kind, payload) VALUES (?1, ?2, ?3)",
            params![session_id, kind, payload.to_string()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn session_history(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT kind, payload FROM audit
             WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, payload) = row?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
            out.push((kind, payload));
        }
        out.reverse();
        Ok(out)
    }

    /// Reflection subagent hook: store a learned rule.
    pub fn save_rule(&self, scope: &str, rule: &str) -> Result<i64, MemoryError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO memory_rules (scope, rule) VALUES (?1, ?2)",
            params![scope, rule],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// `pre_turn` hook: retrieve candidate rules for prompt injection.
    /// Deterministic keyword overlap (no embeddings required).
    pub fn rules_for(&self, prompt: &str, limit: usize) -> Result<Vec<MemoryRule>, MemoryError> {
        let tokens: Vec<String> = prompt
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 3)
            .map(str::to_string)
            .collect();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, scope, rule FROM memory_rules")?;
        let rows = stmt.query_map([], |row| {
            Ok(MemoryRule {
                id: row.get(0)?,
                scope: row.get(1)?,
                rule: row.get(2)?,
            })
        })?;
        let mut scored: Vec<(usize, MemoryRule)> = Vec::new();
        for row in rows {
            let rule = row?;
            let lower = rule.rule.to_lowercase();
            let score = tokens.iter().filter(|t| lower.contains(*t)).count();
            if score > 0 {
                scored.push((score, rule));
            }
        }
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        Ok(scored.into_iter().take(limit).map(|(_, r)| r).collect())
    }

    pub fn truncate_tool_output(output: &str, max_chars: usize) -> String {
        if output.chars().count() <= max_chars {
            return output.to_string();
        }
        let mut out: String = output.chars().take(max_chars).collect();
        out.push_str("…[truncated]");
        out
    }
}

/// Close every `ToolCall` that never got a result. Returns whether the turn
/// changed. Pure: unit-tested without a database.
fn close_interrupted_calls(turn: &mut Turn) -> bool {
    let answered: Vec<String> = turn
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let open: Vec<String> = turn
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall { call_id, .. } if !answered.contains(call_id) => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    if open.is_empty() {
        return false;
    }
    for call_id in open {
        turn.parts.push(Part::ToolResult {
            call_id,
            output: "[interrupted] this tool call never returned; the session ended \
                     before its result was recorded. Re-run it if you need the value."
                .to_string(),
            truncated: false,
        });
    }
    true
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn file_is_nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

fn backup_path(path: &Path, found: &str) -> std::path::PathBuf {
    let safe = found.replace(['/', ':'], "_");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "turya.db".to_string());
    path.with_file_name(format!("{name}.{safe}.bak"))
}

/// Read the format tag from an existing database, or report why it cannot.
fn probe_format(path: &Path) -> Result<String, MemoryError> {
    let conn = Connection::open(path)?;
    let has_table: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='store_meta'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if has_table.is_none() {
        // Pre-A1 database (session_events/memory_rules, no format tag).
        return Ok("pre-A1".to_string());
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key='format_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value.unwrap_or_else(|| "unversioned".to_string()))
}

/// Session logs hold whatever the user pasted, including secrets. Best-effort
/// 0600; a read-only or exotic filesystem is not a reason to fail a launch.
fn harden_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Convenience: build a single-part turn (used by tests and tools).
pub fn turn_with_id(id: &str, parts: Vec<Part>) -> Turn {
    Turn {
        id: TurnId(id.to_string()),
        parts,
    }
}

#[cfg(test)]
mod tests;
