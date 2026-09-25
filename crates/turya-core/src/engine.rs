use crate::hooks::{DiagnosticsHook, MemoryHook};
use crate::permissions::PermissionBroker;
use crate::provider::{LlmProvider, ProviderStep};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use turya_protocol::{AgentMode, PermissionDecision, PermissionMode, TuryaEvent};
use turya_tools::ToolRegistry;

/// Max model calls per turn. After each pass that invokes tools, the engine
/// re-invokes the provider with the accumulated transcript so the assistant
/// answers *after* seeing tool results (agentic loop). Pure-text passes end
/// the turn immediately; only runaway tool-calling hits this cap.
const MAX_MODEL_CALLS: usize = 8;
/// Max tool *executions* per turn. One model response can emit many
/// `CallTool` steps, so the model-call cap alone does not bound side
/// effects (`run_bash`, `write_file`). Generous for legit chains
/// (explore → read → test), tight enough to stop a flood.
const MAX_TOOL_CALLS_PER_TURN: u32 = 32;
/// Tool outputs are truncated in history: one `cat` of a huge file must not
/// blow the context window on every follow-up call.
const MAX_TOOL_HISTORY_CHARS: usize = 2000;

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

        // Agentic loop: each pass asks the provider for the next step given
        // everything so far. Tool results re-enter as history, so the model
        // always gets the last word (a summary, an explanation, a follow-up).
        let mut history: Vec<String> = Vec::new();
        let mut turn_error: Option<String> = None;
        let mut passes = 0u32;
        let mut tools_last_pass = 0u32;
        let mut tool_executions = 0u32;
        let mut tool_cap_hit = false;

        for _pass in 0..MAX_MODEL_CALLS {
            passes += 1;
            let (step_tx, mut step_rx) = mpsc::channel(32);
            let provider = self.provider.read().unwrap().clone();
            let prompt_clone = prompt.to_string();
            let history_clone = history.clone();

            let join = tokio::spawn(async move {
                provider
                    .generate_turn(&prompt_clone, &history_clone, step_tx)
                    .await
            });

            let mut assistant_text = String::new();
            let mut tool_calls_this_pass = 0u32;

            while let Some(step) = step_rx.recv().await {
                match step {
                    ProviderStep::Token(chunk) => {
                        assistant_text.push_str(&chunk);
                        let _ = event_tx.send(TuryaEvent::TokenDelta { chunk }).await;
                    }
                    ProviderStep::CallTool(call) => {
                        tool_calls_this_pass += 1;
                        // Tool budget: stop executing, but stay coherent — the
                        // denial completes like any other result so the model
                        // sees it in history instead of hanging.
                        if tool_executions >= MAX_TOOL_CALLS_PER_TURN {
                            tool_cap_hit = true;
                            let res = turya_protocol::ToolResult {
                                call_id: call.call_id.clone(),
                                success: false,
                                output: String::new(),
                                error: Some("tool budget exhausted".to_string()),
                            };
                            let _ = event_tx
                                .send(TuryaEvent::ToolCallInitiated(call.clone()))
                                .await;
                            let _ = event_tx.send(TuryaEvent::ToolCallCompleted(res)).await;
                            history.push(format!(
                                "Tool '{}' result (success=false): tool budget exhausted",
                                call.tool_name
                            ));
                            continue;
                        }
                        tool_executions += 1;
                        let result = self.execute_tool_call(&call, &event_tx, &mut perm_rx).await;
                        let summary = result
                            .error
                            .clone()
                            .filter(|_| !result.success)
                            .unwrap_or_else(|| result.output.clone());
                        history.push(format!(
                            "Tool '{}' result (success={}): {}",
                            call.tool_name,
                            result.success,
                            truncate_history(&summary, MAX_TOOL_HISTORY_CHARS)
                        ));
                    }
                    ProviderStep::Finish => break,
                }
            }

            // Provider failures must surface: previously `let _ =` swallowed
            // them into a silent empty turn; inside a loop that trap repeats.
            match join.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    turn_error = Some(e);
                    break;
                }
                Err(join_err) => {
                    turn_error = Some(format!("provider task failed: {join_err}"));
                    break;
                }
            }

            if !assistant_text.is_empty() {
                history.push(format!("Assistant: {assistant_text}"));
            }
            // A pass with no tool calls is a final answer: the turn is over.
            tools_last_pass = tool_calls_this_pass;
            if tool_calls_this_pass == 0 {
                break;
            }
        }

        if (passes as usize == MAX_MODEL_CALLS && tools_last_pass > 0) || tool_cap_hit {
            // The budget — not a final answer — ended the turn. Say so visibly.
            let _ = event_tx
                .send(TuryaEvent::Error {
                    message: format!(
                        "step budget ({MAX_MODEL_CALLS} model calls / \
                         {MAX_TOOL_CALLS_PER_TURN} tool calls) exhausted; \
                         showing results so far"
                    ),
                })
                .await;
        }
        if let Some(ref e) = turn_error {
            let _ = event_tx
                .send(TuryaEvent::Error { message: e.clone() })
                .await;
        }

        let success = turn_error.is_none();
        let _ = event_tx
            .send(TuryaEvent::TurnCompleted {
                turn_id: turn_id.to_string(),
                success,
            })
            .await;

        // `on_event(TurnCompleted)` memory hook: append audit row, best-effort.
        // Reflection (distilling failures into rules) runs inside the hook
        // implementation, never in the kernel.
        if let Some(ref hook) = self.memory_hook {
            hook.record_turn_completed(&self.session_id, turn_id, prompt, success)
                .await;
        }
    }

    /// Execute one tool call: announce, permission-gate, run, announce the
    /// result. Extracted from `run_turn` so the agentic loop stays readable;
    /// behavior matches the old inline block exactly.
    async fn execute_tool_call(
        &self,
        call: &turya_protocol::ToolCall,
        event_tx: &mpsc::Sender<TuryaEvent>,
        perm_rx: &mut mpsc::Receiver<(String, PermissionDecision)>,
    ) -> turya_protocol::ToolResult {
        let _ = event_tx
            .send(TuryaEvent::ToolCallInitiated(call.clone()))
            .await;
        let tool = match self.tools.get(&call.tool_name) {
            Some(t) => t,
            None => {
                let res = turya_protocol::ToolResult {
                    call_id: call.call_id.clone(),
                    success: false,
                    output: String::new(),
                    error: Some(format!("Unknown tool: {}", call.tool_name)),
                };
                let _ = event_tx
                    .send(TuryaEvent::ToolCallCompleted(res.clone()))
                    .await;
                return res;
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
                self.check_written_file(call, event_tx).await;
            }
            result
        } else {
            let res = turya_protocol::ToolResult {
                call_id: call.call_id.clone(),
                success: false,
                output: String::new(),
                error: Some("Permission denied by user".to_string()),
            };
            let _ = event_tx
                .send(TuryaEvent::ToolCallCompleted(res.clone()))
                .await;
            res
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

/// Truncate by chars (never split a boundary) for history entries.
fn truncate_history(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("…[truncated]");
    out
}
