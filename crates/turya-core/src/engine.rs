use crate::hooks::{DiagnosticsHook, MemoryHook};
use crate::permissions::PermissionBroker;
use crate::provider::{LlmProvider, ProviderStep};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use turya_protocol::{AgentMode, Part, PermissionDecision, PermissionMode, Transcript, TuryaEvent};
use turya_tools::ToolRegistry;

/// Default per-turn budgets (see `TuryaEngine::set_budgets`). A turn ends at
/// the first of: a tool-free pass (final answer), the model-call cap, or the
/// tool-execution cap. Sessions are unbounded — every message starts a fresh
/// turn with a fresh budget.
const DEFAULT_MODEL_CALLS: usize = 8;
const DEFAULT_TOOL_CALLS: u32 = 32;
/// Tool outputs are truncated in history: one `cat` of a huge file must not
/// blow the context window on every follow-up call.
const MAX_TOOL_HISTORY_CHARS: usize = 2000;

/// Per-turn step budgets: model generations (cost/latency) and tool
/// executions (side effects). One model response can emit many `CallTool`
/// steps, so the two caps cover different risks.
#[derive(Debug, Clone, Copy)]
pub struct TurnBudgets {
    pub model_calls: usize,
    pub tool_calls: u32,
}

impl Default for TurnBudgets {
    fn default() -> Self {
        Self {
            model_calls: DEFAULT_MODEL_CALLS,
            tool_calls: DEFAULT_TOOL_CALLS,
        }
    }
}

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
    /// Step budgets, hot-swappable per session via `set_budgets`
    /// (driven by `UpdateConfig` from the host). Interior mutability:
    /// the engine is shared as `Arc` across turns.
    budgets: RwLock<TurnBudgets>,
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
            budgets: RwLock::new(TurnBudgets::default()),
        }
    }

    /// Hot-swap the active provider (used by `/models` switching).
    pub fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().unwrap() = provider;
    }

    /// Update the per-turn step budgets (`/steps` in the TUI, carried by
    /// `UpdateConfig`). Each side is independent: `None` leaves it unchanged.
    /// Values clamp to a minimum of 1 — a zero budget would end every turn
    /// empty, which is never what the user asked for.
    pub fn set_budgets(&self, model_calls: Option<usize>, tool_calls: Option<u32>) {
        let mut b = self.budgets.write().unwrap();
        if let Some(m) = model_calls {
            b.model_calls = m.max(1);
        }
        if let Some(t) = tool_calls {
            b.tool_calls = t.max(1);
        }
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
        attachments: &[turya_protocol::Attachment],
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
        let budgets = *self.budgets.read().unwrap();
        let mut transcript = Transcript::new(&self.session_id);
        transcript.start_turn(turn_id);
        // The user's turn is recorded here, once. Providers serialize the
        // transcript as-is; there is no separate prompt to append.
        transcript.push(Part::UserText {
            text: prompt.to_string(),
        });
        for attachment in attachments {
            transcript.push(if attachment.mime.starts_with("image/") {
                Part::Image(attachment.clone())
            } else {
                Part::Attachment(attachment.clone())
            });
        }
        let mut turn_error: Option<String> = None;
        let mut passes = 0usize;
        let mut tools_last_pass = 0u32;
        let mut tool_executions = 0u32;
        let mut tool_cap_hit = false;

        for _pass in 0..budgets.model_calls {
            passes += 1;
            let gate = ToolGate {
                executions: &mut tool_executions,
                cap: budgets.tool_calls,
                execute: true,
            };
            let outcome = self
                .run_pass(&transcript, &event_tx, &mut perm_rx, gate)
                .await;
            // Appended here, not inside `run_pass`: the provider only ever
            // borrows the transcript, so no pass ever deep-copies the
            // history. This is the O(n)-per-call clone that used to make a
            // long turn quadratic.
            transcript.extend(outcome.parts);
            if outcome.tool_cap_hit {
                tool_cap_hit = true;
            }
            // Provider failures must surface: swallowing them here would
            // repeat the old silent-empty-turn trap inside the loop.
            if let Some(e) = outcome.provider_err {
                turn_error = Some(e);
                break;
            }
            // Cap hit: further passes could only produce text (every tool
            // would drop), so skip straight to the summary pass instead of
            // burning generations. A tool-free pass is a final answer.
            if tool_cap_hit || outcome.tool_calls == 0 {
                tools_last_pass = outcome.tool_calls;
                break;
            }
            tools_last_pass = outcome.tool_calls;
        }

        let budget_hit = (passes == budgets.model_calls && tools_last_pass > 0) || tool_cap_hit;
        if budget_hit {
            // Graceful final pass (not a bare stop): one text-only summary.
            // Tool calls here are structurally dropped — announced as denied
            // but never executed — so the ending holds even if the model
            // ignores the no-more-tools instruction.
            transcript.push(Part::Instruction {
                text: "Step budget exhausted. Summarize the work done so far and list \
                       remaining tasks as plain text. Do not call any further tools."
                    .to_string(),
            });
            let gate = ToolGate {
                executions: &mut tool_executions,
                cap: budgets.tool_calls,
                execute: false,
            };
            let outcome = self
                .run_pass(&transcript, &event_tx, &mut perm_rx, gate)
                .await;
            transcript.extend(outcome.parts);
            if let Some(e) = outcome.provider_err {
                turn_error = Some(e);
            }
            // The budget — not a final answer — ended the turn. Say so visibly.
            let _ = event_tx
                .send(TuryaEvent::Error {
                    message: format!(
                        "step budget ({} model calls / {} tool calls) exhausted; \
                         showing results so far",
                        budgets.model_calls, budgets.tool_calls
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

    /// One model invocation: stream its steps, execute (or drop) tool calls,
    /// and return the parts this pass produced. Shared by normal passes
    /// (`gate.execute = true`) and the graceful final summary pass
    /// (`false`: calls are announced as denied but never run, and nothing
    /// is recorded for them — the summary text is what matters).
    ///
    /// Takes the transcript by shared borrow and never mutates it, so no
    /// pass copies the history; the caller appends the returned parts.
    async fn run_pass(
        &self,
        transcript: &Transcript,
        event_tx: &mpsc::Sender<TuryaEvent>,
        perm_rx: &mut mpsc::Receiver<(String, PermissionDecision)>,
        gate: ToolGate<'_>,
    ) -> PassOutcome {
        let (step_tx, mut step_rx) = mpsc::channel(32);
        let provider = self.provider.read().unwrap().clone();
        let transcript = transcript.clone();

        let join = tokio::spawn(async move { provider.generate_turn(&transcript, step_tx).await });

        let mut assistant_text = String::new();
        let mut parts: Vec<Part> = Vec::new();
        let mut outcome = PassOutcome {
            tool_calls: 0,
            tool_cap_hit: false,
            provider_err: None,
            parts: Vec::new(),
        };

        while let Some(step) = step_rx.recv().await {
            match step {
                ProviderStep::Token(chunk) => {
                    assistant_text.push_str(&chunk);
                    let _ = event_tx.send(TuryaEvent::TokenDelta { chunk }).await;
                }
                ProviderStep::CallTool(call) => {
                    outcome.tool_calls += 1;
                    parts.push(Part::ToolCall {
                        call_id: call.call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        arguments: call.parameters.clone(),
                        signature: call.signature.clone(),
                    });
                    // Tool budget (or the final summary pass): stop executing,
                    // but stay coherent — the denial completes like any other
                    // result so the model sees it instead of hanging.
                    if !gate.execute || *gate.executions >= gate.cap {
                        outcome.tool_cap_hit = true;
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
                        if gate.execute {
                            parts.push(Part::ToolResult {
                                call_id: call.call_id.clone(),
                                output: format!(
                                    "Tool '{}' (success=false): tool budget exhausted",
                                    call.tool_name
                                ),
                                truncated: false,
                            });
                        }
                        continue;
                    }
                    *gate.executions += 1;
                    let result = self.execute_tool_call(&call, event_tx, perm_rx).await;
                    let summary = result
                        .error
                        .clone()
                        .filter(|_| !result.success)
                        .unwrap_or_else(|| result.output.clone());
                    let recorded = truncate_history(&summary, MAX_TOOL_HISTORY_CHARS);
                    parts.push(Part::ToolResult {
                        call_id: call.call_id.clone(),
                        output: format!(
                            "Tool '{}' result (success={}): {recorded}",
                            call.tool_name, result.success
                        ),
                        truncated: recorded.chars().count() < summary.chars().count(),
                    });
                }
                ProviderStep::Finish => break,
            }
        }

        match join.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => outcome.provider_err = Some(e),
            Err(join_err) => {
                outcome.provider_err = Some(format!("provider task failed: {join_err}"));
            }
        }

        if !assistant_text.is_empty() {
            parts.push(Part::Text {
                text: assistant_text,
            });
        }
        outcome.parts = parts;
        outcome
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

/// What one model pass did: how many tools it asked for, whether the tool
/// cap was hit, and whether the provider itself failed.
struct PassOutcome {
    tool_calls: u32,
    tool_cap_hit: bool,
    provider_err: Option<String>,
    /// Parts this pass produced; the caller appends them to the transcript.
    parts: Vec<Part>,
}

/// Mutable per-pass tool state: execution counter, cap, and whether this
/// pass may execute at all (the final summary pass may not — its calls
/// are structurally dropped). Bundles what would otherwise be a
/// too-many-arguments tail on `run_pass`.
struct ToolGate<'a> {
    executions: &'a mut u32,
    cap: u32,
    execute: bool,
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
