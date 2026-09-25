pub mod anthropic;
pub mod engine;
pub mod permissions;
pub mod provider;

pub use anthropic::AnthropicProvider;
pub use engine::TuryaEngine;
pub use permissions::PermissionBroker;
pub use provider::{LlmProvider, MockProvider, ProviderStep};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use turya_protocol::{AgentMode, PermissionMode, ToolCall, TuryaEvent};

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
    async fn test_turn_completed_persisted_to_memory() {
        let store = Arc::new(std::sync::Mutex::new(
            turya_memory::MemoryStore::open_in_memory().unwrap(),
        ));
        let mock_provider = Arc::new(MockProvider {
            responses: vec![ProviderStep::Token("Hi".to_string()), ProviderStep::Finish],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open)
            .with_memory(store.clone())
            .with_session_id("test-session");
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("t1", "hello", AgentMode::Build, event_tx, perm_rx)
            .await;
        // Drain events so the sender side fully completes.
        while event_rx.recv().await.is_some() {}

        let history = store
            .lock()
            .unwrap()
            .session_history("test-session", 10)
            .unwrap();
        assert!(history.iter().any(|(kind, _)| kind == "TurnCompleted"));
    }

    // run_bash shells out to `bash`, which only exists on Unix.
    // (Windows support needs a cmd.exe fallback in RunBashTool first.)
    #[cfg(unix)]
    #[tokio::test]
    async fn test_failed_tool_yields_reflected_rule() {
        let store = Arc::new(std::sync::Mutex::new(
            turya_memory::MemoryStore::open_in_memory().unwrap(),
        ));
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
            .with_memory(store.clone())
            .with_session_id("reflect-session");
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("t3", "break it", AgentMode::Build, event_tx, perm_rx)
            .await;
        while event_rx.recv().await.is_some() {}

        let store = store.lock().unwrap();
        let history = store.session_history("reflect-session", 10).unwrap();
        assert!(history.iter().any(|(kind, _)| kind == "ToolError"));
        let rules = store.rules_for("retry run_bash again", 5).unwrap();
        assert!(rules.iter().any(|r| r.rule.contains("run_bash")));
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
        // Empty server cmd -> local fallback: existing file yields no diagnostics.
        let bridge = Arc::new(turya_lsp::LspBridge::new(vec![]));
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open).with_lsp(bridge);
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
}
