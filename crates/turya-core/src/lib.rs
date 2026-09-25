pub mod engine;
pub mod hooks;
pub mod permissions;
pub mod plugins;
pub mod provider;

pub use engine::TuryaEngine;
pub use hooks::{DiagnosticsHook, FileDiagnostic, MemoryHook};
pub use permissions::PermissionBroker;
pub use plugins::{
    AuthMethodKind, ModelInfo, ProviderPlugin, ProviderRegistry, ResolvedCreds, UiPlugin,
};
pub use provider::{LlmProvider, MockProvider, ProviderStep};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{DiagnosticsHook, FileDiagnostic, MemoryHook};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use turya_protocol::{AgentMode, PermissionMode, ToolCall, TuryaEvent};

    /// In-memory fake for the memory seam: records hook calls, serves canned rules.
    struct FakeMemory {
        calls: Mutex<Vec<String>>,
        rules: Vec<String>,
    }

    impl FakeMemory {
        fn new(rules: Vec<&str>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                rules: rules.into_iter().map(|r| r.to_string()).collect(),
            }
        }
    }

    #[async_trait]
    impl MemoryHook for FakeMemory {
        async fn recall_rules(&self, _s: &str, _p: &str, limit: usize) -> Vec<String> {
            self.rules.iter().take(limit).cloned().collect()
        }
        async fn record_turn_completed(&self, s: &str, t: &str, _p: &str, _b: bool) {
            self.calls.lock().unwrap().push(format!("turn:{s}:{t}"));
        }
        async fn record_tool_error(&self, s: &str, tool: &str, _c: &str, _e: &str) {
            self.calls.lock().unwrap().push(format!("error:{s}:{tool}"));
        }
    }

    /// Fake diagnostics seam: returns canned diagnostics for any path.
    struct FakeDiagnostics {
        diags: Vec<FileDiagnostic>,
    }

    #[async_trait]
    impl DiagnosticsHook for FakeDiagnostics {
        async fn diagnose_written_file(&self, _p: &Path) -> Vec<FileDiagnostic> {
            self.diags.clone()
        }
        fn format_feedback(&self, _p: &Path, diagnostics: &[FileDiagnostic]) -> Option<String> {
            if diagnostics.iter().any(|d| d.severity == "error") {
                Some("compiler error (fake)".to_string())
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn test_master_loop_with_mock_provider() {
        let mock_provider = Arc::new(MockProvider {
            responses: vec![
                ProviderStep::Token("Hello".to_string()),
                ProviderStep::CallTool(ToolCall {
                    call_id: "1".to_string(),
                    tool_name: "view_file".to_string(),
                    parameters: serde_json::json!({"path": "Cargo.toml"}),
                }),
                ProviderStep::Finish,
            ],
        });

        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        tokio::spawn(async move {
            engine
                .run_turn("test_turn", "hi", AgentMode::Build, event_tx, perm_rx)
                .await;
        });

        let mut received_token = false;
        let mut completed = false;

        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TokenDelta { chunk } => {
                    if chunk == "Hello" {
                        received_token = true;
                    }
                }
                TuryaEvent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(received_token);
        assert!(completed);
    }

    #[tokio::test]
    async fn test_provider_hot_swap_mid_session() {
        let first: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            responses: vec![
                ProviderStep::Token("first".to_string()),
                ProviderStep::Finish,
            ],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(first, tools, PermissionMode::Open);
        // Swap before the turn: the replacement must serve it.
        engine.set_provider(Arc::new(MockProvider {
            responses: vec![
                ProviderStep::Token("second".to_string()),
                ProviderStep::Finish,
            ],
        }));
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        tokio::spawn(async move {
            engine
                .run_turn("swap", "hi", AgentMode::Build, event_tx, perm_rx)
                .await;
        });

        let mut saw = String::new();
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TokenDelta { chunk } => saw.push_str(&chunk),
                TuryaEvent::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        assert_eq!(saw, "second");
    }

    #[tokio::test]
    async fn test_turn_completed_persisted_to_memory() {
        let hook = Arc::new(FakeMemory::new(vec![]));
        let mock_provider = Arc::new(MockProvider {
            responses: vec![ProviderStep::Token("Hi".to_string()), ProviderStep::Finish],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open)
            .with_memory_hook(hook.clone())
            .with_session_id("test-session");
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("t1", "hello", AgentMode::Build, event_tx, perm_rx)
            .await;
        // Drain events so the sender side fully completes.
        while event_rx.recv().await.is_some() {}

        let calls = hook.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c == "turn:test-session:t1"));
    }

    #[tokio::test]
    async fn test_pre_turn_injects_recalled_rules() {
        let hook = Arc::new(FakeMemory::new(vec!["always pin versions"]));
        let mock_provider = Arc::new(MockProvider {
            responses: vec![ProviderStep::Finish],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open)
            .with_memory_hook(hook)
            .with_session_id("recall-session");
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        tokio::spawn(async move {
            engine
                .run_turn("t0", "deploy", AgentMode::Build, event_tx, perm_rx)
                .await;
        });

        let mut saw_rules = false;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TurnCompleted { .. } => break,
                TuryaEvent::TokenDelta { chunk } if chunk.contains("always pin versions") => {
                    saw_rules = true;
                }
                _ => {}
            }
        }
        assert!(saw_rules);
    }

    // run_bash shells out to `bash`, which only exists on Unix.
    // (Windows support needs a cmd.exe fallback in RunBashTool first.)
    #[cfg(unix)]
    #[tokio::test]
    async fn test_failed_tool_yields_reflected_rule() {
        let hook = Arc::new(FakeMemory::new(vec!["old rule"]));
        let mock_provider = Arc::new(MockProvider {
            responses: vec![
                ProviderStep::CallTool(ToolCall {
                    call_id: "f1".to_string(),
                    tool_name: "run_bash".to_string(),
                    parameters: serde_json::json!({"command": "exit 1"}),
                }),
                ProviderStep::Finish,
            ],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open)
            .with_memory_hook(hook.clone())
            .with_session_id("reflect-session");
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("t3", "break it", AgentMode::Build, event_tx, perm_rx)
            .await;
        while event_rx.recv().await.is_some() {}

        // Engine reports the failure through the seam (reflection itself
        // runs inside the real hook implementation, tested in turya-memory).
        let calls = hook.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c == "error:reflect-session:run_bash"));
        assert!(calls.iter().any(|c| c == "turn:reflect-session:t3"));
    }

    #[tokio::test]
    async fn test_write_triggers_lsp_check_without_errors() {
        let dir = std::env::temp_dir().join("turya-engine-lsp-test");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("note.txt");
        let _ = std::fs::remove_file(&target);

        let mock_provider = Arc::new(MockProvider {
            responses: vec![
                ProviderStep::CallTool(ToolCall {
                    call_id: "w1".to_string(),
                    tool_name: "write_file".to_string(),
                    parameters: serde_json::json!({
                        "path": target.to_string_lossy(),
                        "content": "hello",
                    }),
                }),
                ProviderStep::Finish,
            ],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        // Fake seam returning no diagnostics: no feedback must be emitted.
        let hook = Arc::new(FakeDiagnostics { diags: vec![] });
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open)
            .with_diagnostics_hook(hook);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        tokio::spawn(async move {
            engine
                .run_turn("t2", "write it", AgentMode::Build, event_tx, perm_rx)
                .await;
        });

        let mut completed = false;
        let mut error_feedback = false;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                TuryaEvent::TokenDelta { chunk } if chunk.contains("compiler error") => {
                    error_feedback = true;
                }
                _ => {}
            }
        }

        assert!(completed);
        assert!(!error_feedback);
        assert!(target.exists());
        let _ = std::fs::remove_file(&target);
    }

    #[tokio::test]
    async fn test_diagnostics_with_errors_reach_client() {
        let dir = std::env::temp_dir().join("turya-engine-lsp-test");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("broken.rs");
        let _ = std::fs::remove_file(&target);

        let mock_provider = Arc::new(MockProvider {
            responses: vec![
                ProviderStep::CallTool(ToolCall {
                    call_id: "w2".to_string(),
                    tool_name: "write_file".to_string(),
                    parameters: serde_json::json!({
                        "path": target.to_string_lossy(),
                        "content": "fn broken( {",
                    }),
                }),
                ProviderStep::Finish,
            ],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let hook = Arc::new(FakeDiagnostics {
            diags: vec![FileDiagnostic {
                line: 1,
                message: "expected pattern".to_string(),
                severity: "error".to_string(),
            }],
        });
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open)
            .with_diagnostics_hook(hook);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        tokio::spawn(async move {
            engine
                .run_turn("t4", "write broken", AgentMode::Build, event_tx, perm_rx)
                .await;
        });

        let mut saw_diagnostics = false;
        let mut saw_feedback = false;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TurnCompleted { .. } => break,
                TuryaEvent::DiagnosticsReceived { diagnostics } => {
                    saw_diagnostics = !diagnostics.is_empty();
                }
                TuryaEvent::TokenDelta { chunk } if chunk.contains("compiler error") => {
                    saw_feedback = true;
                }
                _ => {}
            }
        }

        assert!(saw_diagnostics);
        assert!(saw_feedback);
        let _ = std::fs::remove_file(&target);
    }
}
