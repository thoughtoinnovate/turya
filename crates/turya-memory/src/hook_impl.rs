//! `MemoryHook` implementation over [`MemoryStore`]: the internal plugin
//! side of the session seam (Rule 3.1 — the kernel owns the trait, this
//! crate owns SQLite).
//!
//! Every method is best-effort from the kernel's point of view: a store
//! failure is returned as a message and never takes a turn down with it.

use crate::MemoryStore;
use async_trait::async_trait;
use turya_core::MemoryHook;
use turya_protocol::{SessionMeta, Turn};

/// Errors are flattened to `String` at this boundary: the kernel must not
/// learn a database error type.
fn msg<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[async_trait]
impl MemoryHook for MemoryStore {
    async fn recall_rules(&self, _session_id: &str, prompt: &str, limit: usize) -> Vec<String> {
        // Rule lookup needs a prompt, not a session; `session_id` is accepted
        // so a future scoping rule can use it without another seam.
        match self.rules_for(prompt, limit) {
            Ok(rules) => rules.into_iter().map(|r| r.rule).collect(),
            Err(_) => Vec::new(),
        }
    }

    async fn record_turn_completed(
        &self,
        session_id: &str,
        _turn_id: &str,
        prompt: &str,
        success: bool,
    ) {
        let _ = self.record_event(
            session_id,
            "turn_completed",
            &serde_json::json!({ "prompt": prompt, "success": success }),
        );
    }

    async fn record_tool_error(
        &self,
        session_id: &str,
        tool_name: &str,
        call_id: &str,
        error: &str,
    ) {
        let _ = self.record_event(
            session_id,
            "tool_error",
            &serde_json::json!({ "tool": tool_name, "call_id": call_id, "error": error }),
        );
    }

    async fn begin_session(
        &self,
        session_id: &str,
        cwd: &str,
        title: &str,
    ) -> Result<SessionMeta, String> {
        self.begin_session(session_id, cwd, title).map_err(msg)
    }

    async fn append_turn(&self, session_id: &str, turn: &Turn) -> Result<(), String> {
        self.append_turn(session_id, turn).map_err(msg)
    }

    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Turn>, String> {
        self.load_transcript(session_id).map_err(msg)
    }

    async fn list_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionMeta>, String> {
        self.list_sessions(cwd, limit).map_err(msg)
    }

    async fn search(
        &self,
        session_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        self.search(session_id, query, limit).map_err(msg)
    }

    async fn session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, String> {
        self.session_meta(session_id).map_err(msg)
    }
}
