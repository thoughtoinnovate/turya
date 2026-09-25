//! Kernel seams for memory and diagnostics (Rule 3.1).
//!
//! These traits are the ONLY way the event loop interacts with memory and
//! language-server capabilities. They carry plain data (strings, paths,
//! diagnostics) — no SQLite, no LSP clients, no secrets, no vendor types.
//! Concrete implementations live in their own plugin crates
//! (`turya-memory`, `turya-lsp`) and are injected by the host.

use async_trait::async_trait;
use std::path::Path;

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
