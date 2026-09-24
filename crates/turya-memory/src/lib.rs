use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRule {
    pub id: i64,
    pub scope: String,
    pub rule: String,
}

/// Episodic + long-term memory backed by SQLite.
///
/// Layout mirrors `.plans/memory_and_self_improvement.md`:
/// - `session_events`: append-only audit trail (`turya resume` source)
/// - `memory_rules`: reflection-extracted rules injected via `pre_turn`
pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Open (or create) a store. Enables WAL mode to avoid I/O stalls on HDDs.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<(), MemoryError> {
        // WAL is a no-op for :memory: but harmless; best-effort only.
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_session_events_session
                ON session_events(session_id, id);
            CREATE TABLE IF NOT EXISTS memory_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope TEXT NOT NULL DEFAULT 'project',
                rule TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        Ok(())
    }

    /// `on_event(TurnCompleted)` hook: persist one audit row.
    pub fn record_event(
        &self,
        session_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO session_events (session_id, kind, payload) VALUES (?1, ?2, ?3)",
            params![session_id, kind, payload.to_string()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn session_history(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, payload FROM session_events
             WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
            let kind: String = row.get(0)?;
            let payload: String = row.get(1)?;
            Ok((kind, payload))
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
        self.conn.execute(
            "INSERT INTO memory_rules (scope, rule) VALUES (?1, ?2)",
            params![scope, rule],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// `pre_turn` hook: retrieve candidate rules for prompt injection.
    /// Deterministic keyword overlap (no embeddings required).
    pub fn rules_for(&self, prompt: &str, limit: usize) -> Result<Vec<MemoryRule>, MemoryError> {
        let tokens: Vec<String> = prompt
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 3)
            .map(|t| t.to_ascii_lowercase())
            .collect();
        let mut stmt = self
            .conn
            .prepare("SELECT id, scope, rule FROM memory_rules ORDER BY id DESC LIMIT 200")?;
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
            let hay = rule.rule.to_ascii_lowercase();
            let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
            // Always keep global-scope rules; keep project rules on any overlap
            // (or when the prompt carries no usable tokens).
            if rule.scope == "global" || score > 0 || tokens.is_empty() {
                scored.push((score, rule));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
        Ok(scored.into_iter().take(limit).map(|(_, r)| r).collect())
    }

    /// Compaction helper: keep head/tail of giant tool outputs.
    pub fn truncate_tool_output(output: &str, max_chars: usize) -> String {
        if output.len() <= max_chars {
            return output.to_string();
        }
        let head = max_chars * 2 / 3;
        let tail = max_chars - head;
        format!(
            "{}\n...[truncated {} chars]...\n{}",
            &output[..head],
            output.len() - max_chars,
            &output[output.len() - tail..]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_and_replays_session_events() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.record_event("s1", "TurnCompleted", &json!({"ok": true})).unwrap();
        store.record_event("s1", "TokenDelta", &json!({"chunk": "hi"})).unwrap();
        let history = store.session_history("s1", 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "TurnCompleted");
    }

    #[test]
    fn retrieves_relevant_rules() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save_rule("project", "Axum handlers require Send + Sync on custom errors")
            .unwrap();
        store.save_rule("global", "Always use double quotes in bash").unwrap();
        let rules = store.rules_for("add a new axum route handler", 5).unwrap();
        assert!(rules.iter().any(|r| r.rule.contains("Axum")));
        assert!(rules.iter().any(|r| r.scope == "global"));
    }

    #[test]
    fn truncates_giant_outputs() {
        let big = "x".repeat(10_000);
        let out = MemoryStore::truncate_tool_output(&big, 1000);
        assert!(out.len() < big.len());
        assert!(out.contains("truncated"));
    }
}
