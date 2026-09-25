//! `DiagnosticsHook` adapter: LSP-backed implementation of the kernel seam.
//!
//! Internal plugin (`turya-lsp`). Owns all language-server I/O plus the
//! local fallback; the engine only ever sees plain `FileDiagnostic` values.

use async_trait::async_trait;
use std::path::Path;
use turya_core::{DiagnosticsHook, FileDiagnostic};

use super::{LspBridge, Severity};

pub struct LspDiagnosticsHook {
    bridge: LspBridge,
}

impl LspDiagnosticsHook {
    pub fn new(server_cmd: Vec<String>) -> Self {
        Self {
            bridge: LspBridge::new(server_cmd),
        }
    }

    pub fn rust_analyzer() -> Self {
        Self {
            bridge: LspBridge::rust_analyzer(),
        }
    }
}

fn map_severity(s: &Severity) -> String {
    match s {
        Severity::Error => "error".to_string(),
        Severity::Warning => "warning".to_string(),
        Severity::Info => "info".to_string(),
    }
}

#[async_trait]
impl DiagnosticsHook for LspDiagnosticsHook {
    async fn diagnose_written_file(&self, path: &Path) -> Vec<FileDiagnostic> {
        // Best-effort by design: a missing/broken server yields no
        // diagnostics rather than failing the turn.
        match self.bridge.diagnose_file(path).await {
            Ok(diags) => diags
                .into_iter()
                .map(|d| FileDiagnostic {
                    line: d.line,
                    message: d.message,
                    severity: map_severity(&d.severity),
                })
                .collect(),
            Err(_) => vec![],
        }
    }

    fn format_feedback(&self, path: &Path, diagnostics: &[FileDiagnostic]) -> Option<String> {
        let errors: Vec<&FileDiagnostic> = diagnostics
            .iter()
            .filter(|d| d.severity == "error")
            .collect();
        if errors.is_empty() {
            return None;
        }
        let mut out = format!(
            "Your last edit to {} caused {} compiler error(s). Fix them before finishing the turn:\n",
            path.display(),
            errors.len()
        );
        for e in errors.iter().take(10) {
            out.push_str(&format!("- line {}: {}\n", e.line, e.message));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn adapter_maps_and_degrades_gracefully() {
        // Empty server cmd -> local fallback: existing file is clean.
        let hook = LspDiagnosticsHook::new(vec![]);
        let dir = std::env::temp_dir().join("turya-lsp-adapter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("clean.txt");
        std::fs::write(&target, "hello").unwrap();
        let diags = hook.diagnose_written_file(&target).await;
        assert!(diags.is_empty());
        assert!(hook.format_feedback(&target, &diags).is_none());

        // Missing binary -> empty vec, never an error.
        let hook = LspDiagnosticsHook::new(vec!["turya-nonexistent-lsp-xyz".to_string()]);
        let diags = hook.diagnose_written_file(&target).await;
        assert!(diags.is_empty());

        // Feedback rendering matches the kernel-facing contract.
        let fake = vec![FileDiagnostic {
            line: 3,
            message: "boom".to_string(),
            severity: "error".to_string(),
        }];
        let fb = hook.format_feedback(&target, &fake).unwrap();
        assert!(fb.contains("line 3"));
        let _ = std::fs::remove_file(&target);
    }
}
