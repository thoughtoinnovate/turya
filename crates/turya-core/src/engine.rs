use crate::permissions::PermissionBroker;
use crate::provider::{LlmProvider, ProviderStep};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use turya_protocol::{AgentMode, PermissionDecision, PermissionMode, TuryaEvent};
use turya_tools::ToolRegistry;

pub struct TuryaEngine {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    permissions: Arc<PermissionBroker>,
    /// Step 8 hook: episodic persistence (`TurnCompleted`) + `pre_turn` rule injection.
    memory: Option<Arc<Mutex<turya_memory::MemoryStore>>>,
    /// Step 10 hook: post-write LSP diagnostics fed back into the turn.
    lsp: Option<Arc<turya_lsp::LspBridge>>,
    session_id: String,
}

impl TuryaEngine {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        mode: PermissionMode,
    ) -> Self {
        Self {
            provider,
            tools,
            permissions: Arc::new(PermissionBroker::new(mode)),
            memory: None,
            lsp: None,
            session_id: "default".to_string(),
        }
    }

    pub fn with_memory(mut self, store: Arc<Mutex<turya_memory::MemoryStore>>) -> Self {
        self.memory = Some(store);
        self
    }

    pub fn with_lsp(mut self, bridge: Arc<turya_lsp::LspBridge>) -> Self {
        self.lsp = Some(bridge);
        self
    }

    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = session_id.to_string();
        self
    }

    pub async fn run_turn(
        &self,
        turn_id: &str,
        prompt: &str,
        mode: AgentMode,
        event_tx: mpsc::Sender<TuryaEvent>,
        mut perm_rx: mpsc::Receiver<(String, PermissionDecision)>,
    ) {
        // `pre_turn` memory hook: surface learned rules before generation.
        if let Some(ref memory) = self.memory {
            let rules = memory
                .lock()
                .ok()
                .and_then(|store| store.rules_for(prompt, 3).ok())
                .unwrap_or_default();
            if !rules.is_empty() {
                let mut chunk = String::from("[memory] recalled rules:\n");
                for rule in &rules {
                    chunk.push_str(&format!("- {}\n", rule.rule));
                }
                let _ = event_tx.send(TuryaEvent::TokenDelta { chunk }).await;
            }
        }

        let _ = event_tx
            .send(TuryaEvent::TurnStarted {
                turn_id: turn_id.to_string(),
                mode,
            })
            .await;

        let (step_tx, mut step_rx) = mpsc::channel(32);
        let provider = self.provider.clone();
        let prompt_clone = prompt.to_string();

        tokio::spawn(async move {
            let _ = provider.generate_turn(&prompt_clone, &[], step_tx).await;
        });

        while let Some(step) = step_rx.recv().await {
            match step {
                ProviderStep::Token(chunk) => {
                    let _ = event_tx.send(TuryaEvent::TokenDelta { chunk }).await;
                }
                ProviderStep::CallTool(call) => {
                    let _ = event_tx
                        .send(TuryaEvent::ToolCallInitiated(call.clone()))
                        .await;
                    let tool = match self.tools.get(&call.tool_name) {
                        Some(t) => t,
                        None => {
                            let _ = event_tx
                                .send(TuryaEvent::ToolCallCompleted(turya_protocol::ToolResult {
                                    call_id: call.call_id,
                                    success: false,
                                    output: String::new(),
                                    error: Some(format!("Unknown tool: {}", call.tool_name)),
                                }))
                                .await;
                            continue;
                        }
                    };

                    let risk = tool.risk_level(&call.parameters);
                    let authorized = match self.permissions.check(&call.tool_name, risk) {
                        Some(decision) => decision != PermissionDecision::Deny,
                        None => {
                            // Issue challenge
                            let req_id = format!("req_{}", call.call_id);
                            let _ = event_tx
                                .send(TuryaEvent::PermissionRequested {
                                    request_id: req_id.clone(),
                                    action: call.tool_name.clone(),
                                    risk_level: risk,
                                    details: call.parameters.to_string(),
                                })
                                .await;

                            // Wait for UI to resolve
                            let mut approved = false;
                            while let Some((id, dec)) = perm_rx.recv().await {
                                if id == req_id {
                                    self.permissions.record_decision(&call.tool_name, dec);
                                    approved = dec != PermissionDecision::Deny;
                                    break;
                                }
                            }
                            approved
                        }
                    };

                    if authorized {
                        let result = tool.execute(&call.call_id, call.parameters.clone()).await;
                        let _ = event_tx
                            .send(TuryaEvent::ToolCallCompleted(result.clone()))
                            .await;
                        // Record genuine tool failures for the reflection loop
                        // (permission denials are user decisions, not lessons).
                        if !result.success {
                            self.record_tool_error(&call.tool_name, &result);
                        }
                        // Step 10 hook: freshly written files get a live diagnostic check.
                        if call.tool_name == "write_file" {
                            self.check_written_file(&call, &event_tx).await;
                        }
                    } else {
                        let _ = event_tx
                            .send(TuryaEvent::ToolCallCompleted(turya_protocol::ToolResult {
                                call_id: call.call_id,
                                success: false,
                                output: String::new(),
                                error: Some("Permission denied by user".to_string()),
                            }))
                            .await;
                    }
                }
                ProviderStep::Finish => break,
            }
        }

        let _ = event_tx
            .send(TuryaEvent::TurnCompleted {
                turn_id: turn_id.to_string(),
                success: true,
            })
            .await;

        // `on_event(TurnCompleted)` memory hook: append audit row, best-effort.
        if let Some(ref memory) = self.memory {
            if let Ok(store) = memory.lock() {
                let _ = store.record_event(
                    &self.session_id,
                    "TurnCompleted",
                    &serde_json::json!({"turn_id": turn_id, "prompt": prompt, "success": true}),
                );
                // Reflection loop: distill this session's tool failures into rules.
                let _ = store.reflect_session(&self.session_id);
            }
        }
    }

    fn record_tool_error(&self, tool_name: &str, result: &turya_protocol::ToolResult) {
        let memory = match self.memory.as_ref() {
            Some(m) => m,
            None => return,
        };
        if result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("Permission denied"))
        {
            return;
        }
        if let Ok(store) = memory.lock() {
            let _ = store.record_event(
                &self.session_id,
                "ToolError",
                &serde_json::json!({
                    "tool": tool_name,
                    "call_id": result.call_id,
                    "error": result.error.clone().unwrap_or_else(|| "tool reported failure".to_string()),
                }),
            );
        }
    }

    /// Diagnose a just-written file and stream any compiler errors back.
    async fn check_written_file(
        &self,
        call: &turya_protocol::ToolCall,
        event_tx: &mpsc::Sender<TuryaEvent>,
    ) {
        let bridge = match self.lsp.as_ref() {
            Some(b) => b,
            None => return,
        };
        let path_str = match call.parameters.get("path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => return,
        };
        let path = Path::new(path_str);
        let diagnostics = match bridge.diagnose_file(path).await {
            Ok(d) => d,
            Err(_) => return,
        };
        if diagnostics.is_empty() {
            return;
        }
        let protocol_diags: Vec<turya_protocol::DiagnosticItem> =
            diagnostics.iter().map(|d| d.to_protocol()).collect();
        let _ = event_tx
            .send(TuryaEvent::DiagnosticsReceived {
                diagnostics: protocol_diags,
            })
            .await;
        if let Some(feedback) = turya_lsp::LspBridge::format_feedback(path, &diagnostics) {
            let _ = event_tx
                .send(TuryaEvent::TokenDelta { chunk: feedback })
                .await;
        }
    }
}
