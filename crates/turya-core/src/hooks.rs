//! Kernel seams for memory and diagnostics (Rule 3.1).
//!
//! These traits are the ONLY way the event loop interacts with memory and
//! language-server capabilities. They carry plain data (strings, paths,
//! diagnostics) — no SQLite, no LSP clients, no secrets, no vendor types.
//! Concrete implementations live in their own plugin crates
//! (`turya-memory`, `turya-lsp`) and are injected by the host.

use async_trait::async_trait;
use std::path::Path;
use turya_protocol::{SessionMeta, Turn};

/// Memory seam: episodic persistence + rule recall + reflection.
///
/// Implemented by `turya_memory::SqliteMemoryHook` (internal plugin).
/// The engine calls these best-effort and never fails a turn over them.
#[async_trait]
pub trait MemoryHook: Send + Sync {
    /// `pre_turn`: recall up to `limit` learned rules relevant to `prompt`.
    async fn recall_rules(&self, session_id: &str, prompt: &str, limit: usize) -> Vec<String>;
    /// `on_event(TurnCompleted)`: persist one audit row.
    async fn record_turn_completed(
        &self,
        session_id: &str,
        turn_id: &str,
        prompt: &str,
        success: bool,
    );
    /// Record a genuine tool failure for the reflection loop.
    async fn record_tool_error(
        &self,
        session_id: &str,
        tool_name: &str,
        call_id: &str,
        error: &str,
    );

    // ---- Session log (episodic persistence, same seam) ----
    //
    // Plain data in, plain data out. The store owns sequencing, transactions,
    // and crash repair; the kernel never sees SQL or a file path (Rule 3.1).

    /// Create the session row if it does not exist. Returns the header, so
    /// the caller learns the committed sequence number.
    async fn begin_session(
        &self,
        session_id: &str,
        cwd: &str,
        title: &str,
    ) -> Result<SessionMeta, String>;

    /// Append one turn as a single atomic log record. The store asserts the
    /// sequence continues the log, so a gap can never be committed.
    async fn append_turn(&self, session_id: &str, turn: &Turn) -> Result<(), String>;

    /// Replay a stored session. The store repairs a crash-interrupted tail
    /// before returning, so a replayed transcript is always a *valid* request.
    async fn load_transcript(&self, session_id: &str) -> Result<Vec<Turn>, String>;

    /// Headers for listing, newest first.
    async fn list_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionMeta>, String>;

    /// Retrieve stored text mentioning `query` (post-compaction recall).
    async fn search(
        &self,
        session_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, String>;

    /// Session header without a full replay.
    async fn session_meta(&self, session_id: &str) -> Result<Option<SessionMeta>, String>;
}

/// One file diagnostic in protocol-neutral shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiagnostic {
    pub line: usize,
    pub message: String,
    /// `"error" | "warning" | "info"`.
    pub severity: String,
}

/// Diagnostics seam: post-write file checks with model-readable feedback.
///
/// Implemented by `turya_lsp::LspDiagnosticsHook` (internal plugin).
#[async_trait]
pub trait DiagnosticsHook: Send + Sync {
    /// Diagnose a just-written file; empty vec means clean.
    async fn diagnose_written_file(&self, path: &Path) -> Vec<FileDiagnostic>;
    /// Render compiler errors as a system-prompt feedback chunk, or `None`
    /// when there is nothing worth interrupting the turn for.
    fn format_feedback(&self, path: &Path, diagnostics: &[FileDiagnostic]) -> Option<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopMemory;
    #[async_trait]
    impl MemoryHook for NoopMemory {
        async fn recall_rules(&self, _s: &str, _p: &str, _l: usize) -> Vec<String> {
            vec![]
        }
        async fn record_turn_completed(&self, _s: &str, _t: &str, _p: &str, _b: bool) {}
        async fn record_tool_error(&self, _s: &str, _t: &str, _c: &str, _e: &str) {}
        async fn begin_session(
            &self,
            session_id: &str,
            _cwd: &str,
            _title: &str,
        ) -> Result<SessionMeta, String> {
            Ok(SessionMeta {
                id: session_id.to_string(),
                cwd: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
                title: String::new(),
                parent_id: None,
                seq: 0,
                repaired: false,
            })
        }
        async fn append_turn(&self, _s: &str, _t: &Turn) -> Result<(), String> {
            Ok(())
        }
        async fn load_transcript(&self, _s: &str) -> Result<Vec<Turn>, String> {
            Ok(Vec::new())
        }
        async fn list_sessions(
            &self,
            _c: Option<&str>,
            _l: usize,
        ) -> Result<Vec<SessionMeta>, String> {
            Ok(Vec::new())
        }
        async fn search(&self, _s: &str, _q: &str, _l: usize) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        async fn session_meta(&self, _s: &str) -> Result<Option<SessionMeta>, String> {
            Ok(None)
        }
    }

    struct NoopDiagnostics;
    #[async_trait]
    impl DiagnosticsHook for NoopDiagnostics {
        async fn diagnose_written_file(&self, _p: &Path) -> Vec<FileDiagnostic> {
            vec![]
        }
        fn format_feedback(&self, _p: &Path, _d: &[FileDiagnostic]) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn hook_traits_are_object_safe_and_send_sync() {
        fn assert_hook<M: MemoryHook + Send + Sync, D: DiagnosticsHook + Send + Sync>(
            _m: M,
            _d: D,
        ) {
        }
        assert_hook(NoopMemory, NoopDiagnostics);
        let mem: Box<dyn MemoryHook> = Box::new(NoopMemory);
        assert!(mem.recall_rules("s", "hi", 3).await.is_empty());
        let diag: Box<dyn DiagnosticsHook> = Box::new(NoopDiagnostics);
        assert!(diag
            .diagnose_written_file(Path::new("x.rs"))
            .await
            .is_empty());
    }
}
