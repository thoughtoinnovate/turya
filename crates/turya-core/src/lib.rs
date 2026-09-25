pub mod engine;
pub mod hooks;
pub mod permissions;
pub mod plugins;
pub mod provider;
pub mod tasks;

pub use engine::TuryaEngine;
pub use hooks::{DiagnosticsHook, FileDiagnostic, MemoryHook};
pub use permissions::PermissionBroker;
pub use plugins::{
    AuthMethodKind, ModelInfo, ProviderPlugin, ProviderRegistry, ResolvedCreds, UiPlugin,
};
pub use provider::{LlmProvider, MockProvider, ProviderStep};
pub use tasks::{TaskId, TaskKind, TaskRegistry};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{DiagnosticsHook, FileDiagnostic, MemoryHook};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use turya_protocol::Transcript;
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
                    signature: None,
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
                .run_turn("test_turn", "hi", AgentMode::Build, &[], event_tx, perm_rx)
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
                .run_turn("swap", "hi", AgentMode::Build, &[], event_tx, perm_rx)
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
            .run_turn("t1", "hello", AgentMode::Build, &[], event_tx, perm_rx)
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
                .run_turn("t0", "deploy", AgentMode::Build, &[], event_tx, perm_rx)
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
                    signature: None,
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
            .run_turn("t3", "break it", AgentMode::Build, &[], event_tx, perm_rx)
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
                    signature: None,
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
                .run_turn("t2", "write it", AgentMode::Build, &[], event_tx, perm_rx)
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
                    signature: None,
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
                .run_turn(
                    "t4",
                    "write broken",
                    AgentMode::Build,
                    &[],
                    event_tx,
                    perm_rx,
                )
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

    /// Scripted provider for loop tests: serves one canned step-list per
    /// `generate_turn` call and records the transcript it was given, so tests
    /// can assert exactly what the engine fed back. (`MockProvider` replays
    /// one script forever, which cannot model multi-pass turns.)
    struct ScriptProvider {
        scripts: Mutex<Vec<Vec<ProviderStep>>>,
        seen_history: Mutex<Vec<Transcript>>,
        fail_with: Option<String>,
    }

    impl ScriptProvider {
        fn new(scripts: Vec<Vec<ProviderStep>>) -> Self {
            Self {
                scripts: Mutex::new(scripts),
                seen_history: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                scripts: Mutex::new(Vec::new()),
                seen_history: Mutex::new(Vec::new()),
                fail_with: Some(msg.to_string()),
            }
        }
        fn calls(&self) -> usize {
            self.seen_history.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptProvider {
        async fn generate_turn(
            &self,
            transcript: &Transcript,
            tx: mpsc::Sender<ProviderStep>,
        ) -> Result<(), String> {
            self.seen_history.lock().unwrap().push(transcript.clone());
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            let script = self.scripts.lock().unwrap().remove(0);
            for step in script {
                let _ = tx.send(step).await;
            }
            Ok(())
        }
    }

    /// Every text-ish payload in a transcript, flattened. Keeps loop-test
    /// assertions readable ("was the tool output fed back?") without
    /// re-implementing a part matcher in each test.
    fn transcript_text(t: &Transcript) -> String {
        t.turns
            .iter()
            .flat_map(|turn| turn.parts.iter())
            .map(|p| match p {
                turya_protocol::Part::Text { text }
                | turya_protocol::Part::Reasoning { text }
                | turya_protocol::Part::Instruction { text } => text.clone(),
                turya_protocol::Part::ToolResult { output, .. } => output.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_call(id: &str, tool: &str, params: serde_json::Value) -> ProviderStep {
        ProviderStep::CallTool(ToolCall {
            call_id: id.to_string(),
            tool_name: tool.to_string(),
            parameters: params,
            signature: None,
        })
    }

    // run_bash shells out to `bash`, which only exists on Unix.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_transcript_parts_are_appended_once_and_in_order() {
        // Five calls in one pass. The next pass must see exactly ten parts in
        // alternating call/result order — this is the regression guard for
        // the old deep-clone history, where re-appending on every pass made
        // the transcript grow quadratically.
        let provider = Arc::new(ScriptProvider::new(vec![
            vec![
                tool_call("k1", "nope_missing", serde_json::json!({})),
                tool_call("k2", "nope_missing", serde_json::json!({})),
                tool_call("k3", "nope_missing", serde_json::json!({})),
                tool_call("k4", "nope_missing", serde_json::json!({})),
                tool_call("k5", "nope_missing", serde_json::json!({})),
                ProviderStep::Finish,
            ],
            vec![ProviderStep::Token("end".to_string()), ProviderStep::Finish],
        ]));
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider.clone(), tools, PermissionMode::Open);
        let (event_tx, _event_rx) = mpsc::channel(64);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("order", "go", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let seen = provider.seen_history.lock().unwrap();
        let parts: Vec<_> = seen[1].turns.iter().flat_map(|t| t.parts.iter()).collect();
        assert_eq!(parts.len(), 11, "1 user turn + 5 calls + 5 results");
        assert!(matches!(parts[0], turya_protocol::Part::UserText { .. }));
        for i in 0..5 {
            match parts[1 + i * 2] {
                turya_protocol::Part::ToolCall { call_id, .. } => {
                    assert_eq!(call_id.as_str(), format!("k{}", i + 1));
                }
                other => panic!("expected ToolCall k{}, got {other:?}", i + 1),
            }
            match &parts[2 + i * 2] {
                turya_protocol::Part::ToolResult { call_id, .. } => {
                    assert_eq!(call_id.as_str(), format!("k{}", i + 1));
                }
                other => panic!("expected ToolResult k{}, got {other:?}", i + 1),
            }
        }
    }

    #[tokio::test]
    async fn test_oversized_tool_output_is_truncated_and_flagged() {
        // A huge result must be truncated *and* carry the flag, so providers
        // can mark the block instead of the harness splicing a marker string.
        let path = std::env::temp_dir().join("turya-a0-big.txt");
        std::fs::write(&path, "x".repeat(9000)).unwrap();
        let provider = Arc::new(ScriptProvider::new(vec![
            vec![
                tool_call(
                    "big",
                    "view_file",
                    serde_json::json!({"path": path.to_string_lossy()}),
                ),
                ProviderStep::Finish,
            ],
            vec![ProviderStep::Token("ok".to_string()), ProviderStep::Finish],
        ]));
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider.clone(), tools, PermissionMode::Open);
        let (event_tx, _event_rx) = mpsc::channel(64);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("trunc", "read it", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let seen = provider.seen_history.lock().unwrap();
        let result = seen[1]
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .find(|p| {
                matches!(
                    p,
                    turya_protocol::Part::ToolResult { call_id, .. } if call_id == "big"
                )
            })
            .expect("tool result recorded");
        match result {
            turya_protocol::Part::ToolResult {
                output, truncated, ..
            } => {
                assert!(*truncated, "oversized output must be flagged");
                assert!(
                    output.chars().count() < 9000,
                    "output must be truncated: {}",
                    output.chars().count()
                );
                assert!(
                    output.contains("success=true"),
                    "prefix preserved: {output}"
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_loop_feeds_tool_results_back_to_provider() {
        let provider = Arc::new(ScriptProvider::new(vec![
            vec![
                tool_call(
                    "c1",
                    "run_bash",
                    serde_json::json!({"command": "echo hello-loop"}),
                ),
                ProviderStep::Finish,
            ],
            vec![
                ProviderStep::Token("done".to_string()),
                ProviderStep::Finish,
            ],
        ]));
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider.clone(), tools, PermissionMode::Open);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("loop1", "do it", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let mut saw_done = false;
        let mut completed = false;
        let mut budget_hit = false;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TokenDelta { chunk } if chunk == "done" => saw_done = true,
                TuryaEvent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                TuryaEvent::Error { .. } => budget_hit = true,
                _ => {}
            }
        }

        assert!(saw_done, "follow-up text never streamed");
        assert!(completed);
        assert!(!budget_hit, "two-pass turn must not hit the budget");
        assert_eq!(provider.calls(), 2);
        let seen = provider.seen_history.lock().unwrap();
        assert!(
            transcript_text(&seen[1]).contains("hello-loop"),
            "second call transcript missing tool output: {:?}",
            seen[1]
        );
    }

    #[tokio::test]
    async fn test_loop_stops_at_model_call_budget() {
        // A provider that never stops calling tools: 8 scripts, one per
        // pass, plus a text script for the graceful final summary pass.
        let mut scripts: Vec<Vec<ProviderStep>> = (0..8)
            .map(|i| {
                vec![
                    tool_call(&format!("b{i}"), "nope_missing", serde_json::json!({})),
                    ProviderStep::Finish,
                ]
            })
            .collect();
        scripts.push(vec![
            ProviderStep::Token("summary".to_string()),
            ProviderStep::Finish,
        ]);
        let provider = Arc::new(ScriptProvider::new(scripts));
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider.clone(), tools, PermissionMode::Open);
        let (event_tx, mut event_rx) = mpsc::channel(128);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("loop2", "go", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let mut completed = false;
        let mut budget_msg = false;
        let mut saw_summary = false;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                TuryaEvent::Error { message } if message.contains("budget") => budget_msg = true,
                TuryaEvent::TokenDelta { chunk } if chunk == "summary" => saw_summary = true,
                _ => {}
            }
        }

        assert_eq!(
            provider.calls(),
            9,
            "8 tool passes plus exactly one text-only summary pass"
        );
        assert!(saw_summary, "summary pass must stream text");
        assert!(budget_msg, "budget exhaustion must be visible");
        assert!(completed, "turn must still complete");
        // The summary pass saw the exhaustion instruction, not more tools.
        let seen = provider.seen_history.lock().unwrap();
        assert!(
            transcript_text(&seen[8]).contains("Step budget exhausted"),
            "final transcript: {:?}",
            seen[8]
        );
    }

    #[tokio::test]
    async fn test_tool_execution_cap_drops_extra_calls() {
        // 40 tool calls in 2 passes against a 32-execution cap: the first 32
        // refuse as unknown tools (fast, no side effects); the rest must be
        // dropped by the cap with a visible denial. A text script feeds the
        // graceful final summary pass.
        let mut scripts: Vec<Vec<ProviderStep>> = (0..2)
            .map(|p| {
                let mut steps: Vec<ProviderStep> = (0..20)
                    .map(|i| tool_call(&format!("c{p}_{i}"), "nope_missing", serde_json::json!({})))
                    .collect();
                steps.push(ProviderStep::Finish);
                steps
            })
            .collect();
        scripts.push(vec![
            ProviderStep::Token("capped".to_string()),
            ProviderStep::Finish,
        ]);
        let provider = Arc::new(ScriptProvider::new(scripts));
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider.clone(), tools, PermissionMode::Open);
        engine.set_budgets(Some(8), Some(32));
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("loop4", "go", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let mut completed = false;
        let mut cap_denials = 0u32;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                TuryaEvent::ToolCallCompleted(res)
                    if res.error.as_deref() == Some("tool budget exhausted") =>
                {
                    cap_denials += 1;
                }
                _ => {}
            }
        }

        assert_eq!(
            provider.calls(),
            3,
            "2 tool passes plus the summary pass; cap must not loop"
        );
        assert_eq!(cap_denials, 8, "calls 33-40 must be dropped, not executed");
        assert!(completed);
    }

    #[tokio::test]
    async fn test_set_budgets_applies_per_side() {
        // A zero-tool text turn completes in one pass under any budget; this
        // exercises set_budgets (including the zero-clamp) without looping.
        let provider = Arc::new(MockProvider {
            responses: vec![ProviderStep::Token("hi".to_string()), ProviderStep::Finish],
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider, tools, PermissionMode::Open);
        engine.set_budgets(Some(3), None);
        engine.set_budgets(None, Some(0)); // clamps to 1, never 0
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("loop5", "hi", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let mut completed = false;
        while let Some(evt) = event_rx.recv().await {
            if matches!(evt, TuryaEvent::TurnCompleted { .. }) {
                completed = true;
                break;
            }
        }
        assert!(completed);
    }

    #[tokio::test]
    async fn test_provider_error_surfaces_and_ends_turn() {
        let provider = Arc::new(ScriptProvider::failing("boom"));
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(provider, tools, PermissionMode::Open);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        engine
            .run_turn("loop3", "go", AgentMode::Build, &[], event_tx, perm_rx)
            .await;

        let mut saw_boom = false;
        let mut failed_completion = false;
        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::Error { message } if message.contains("boom") => saw_boom = true,
                TuryaEvent::TurnCompleted { success, .. } => {
                    failed_completion = !success;
                    break;
                }
                _ => {}
            }
        }

        assert!(saw_boom, "provider error must surface as an event");
        assert!(failed_completion, "turn must complete with success=false");
    }
}
