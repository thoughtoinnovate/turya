use crate::hooks::{DiagnosticsHook, MemoryHook};
use crate::permissions::PermissionBroker;
use crate::provider::{LlmProvider, ProviderStep};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use turya_protocol::{AgentMode, PermissionDecision, PermissionMode, TuryaEvent};
use turya_tools::ToolRegistry;

pub struct TuryaEngine {
    /// Hot-swappable provider slot (Rule 3.3): `/models` switches vendors
    /// mid-session without rebuilding the engine.
    provider: Arc<RwLock<Arc<dyn LlmProvider>>>,
    tools: Arc<ToolRegistry>,
    permissions: Arc<PermissionBroker>,
    /// Injected memory seam (Rule 3.1: trait only, implementation lives
    /// in the `turya-memory` internal plugin crate).
    memory_hook: Option<Arc<dyn MemoryHook>>,
    /// Injected diagnostics seam (implementation lives in `turya-lsp`).
    diagnostics_hook: Option<Arc<dyn DiagnosticsHook>>,
    session_id: String,
}

impl TuryaEngine {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        mode: PermissionMode,
    ) -> Self {
        Self {
            provider: Arc::new(RwLock::new(provider)),
            tools,
            permissions: Arc::new(PermissionBroker::new(mode)),
            memory_hook: None,
            diagnostics_hook: None,
            session_id: "default".to_string(),
        }
    }

    /// Hot-swap the active provider (used by `/models` switching).
    pub fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().unwrap() = provider;
    }

    pub fn with_memory_hook(mut self, hook: Arc<dyn MemoryHook>) -> Self {
        self.memory_hook = Some(hook);
        self
    }

    pub fn with_diagnostics_hook(mut self, hook: Arc<dyn DiagnosticsHook>) -> Self {
        self.diagnostics_hook = Some(hook);
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
        if let Some(ref hook) = self.memory_hook {
            let rules = hook.recall_rules(&self.session_id, prompt, 3).await;
            if !rules.is_empty() {
                let mut chunk = String::from("[memory] recalled rules:\n");
                for rule in &rules {
                    chunk.push_str(&format!("- {}\n", rule));
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
        let provider = self.provider.read().unwrap().clone();
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
                            self.record_tool_error(&call.tool_name, &result).await;
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
        // Reflection (distilling failures into rules) runs inside the hook
        // implementation, never in the kernel.
        if let Some(ref hook) = self.memory_hook {
            hook.record_turn_completed(&self.session_id, turn_id, prompt, true)
                .await;
        }
    }

    async fn record_tool_error(&self, tool_name: &str, result: &turya_protocol::ToolResult) {
        let hook = match self.memory_hook.as_ref() {
            Some(h) => h,
            None => return,
        };
        let error = match result.error.as_deref() {
            // Permission denials are user decisions, not lessons.
            Some(e) if e.contains("Permission denied") => return,
            Some(e) => e.to_string(),
            None if !result.success => "tool reported failure".to_string(),
            None => return,
        };
        hook.record_tool_error(&self.session_id, tool_name, &result.call_id, &error)
            .await;
    }

    /// Diagnose a just-written file and stream any compiler errors back.
    async fn check_written_file(
        &self,
        call: &turya_protocol::ToolCall,
        event_tx: &mpsc::Sender<TuryaEvent>,
    ) {
        let hook = match self.diagnostics_hook.as_ref() {
            Some(h) => h,
            None => return,
        };
        let path_str = match call.parameters.get("path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => return,
        };
        let path = Path::new(path_str);
        let diagnostics = hook.diagnose_written_file(path).await;
        if diagnostics.is_empty() {
            return;
        }
        let protocol_diags: Vec<turya_protocol::DiagnosticItem> = diagnostics
            .iter()
            .map(|d| turya_protocol::DiagnosticItem {
                file: path.to_path_buf(),
                line: d.line,
                message: d.message.clone(),
                severity: d.severity.clone(),
            })
            .collect();
        let _ = event_tx
            .send(TuryaEvent::DiagnosticsReceived {
                diagnostics: protocol_diags,
            })
            .await;
        if let Some(feedback) = hook.format_feedback(path, &diagnostics) {
            let _ = event_tx
                .send(TuryaEvent::TokenDelta { chunk: feedback })
                .await;
        }
    }
}
