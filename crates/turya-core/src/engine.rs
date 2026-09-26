use crate::hooks::{DiagnosticsHook, MemoryHook, SkillHook};
use crate::permissions::PermissionBroker;
use crate::provider::{LlmProvider, ProviderStep};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use turya_protocol::{
    AgentMode, ContextBudget, Part, PermissionDecision, PermissionMode, Transcript, TuryaEvent,
};
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
/// Compact once the estimate passes this percentage of the usable window.
/// The margin absorbs a bad estimate and one more turn of output.
const COMPACT_AT_PERCENT: u32 = 85;
/// Give up auto-compacting after this many consecutive failures.
const MAX_COMPACT_FAILURES: u32 = 3;
/// The kernel-owned delegation tool. Not in the `ToolRegistry`: it needs the
/// engine, and a tool holding an `Arc` back to the engine that owns the
/// registry is a reference cycle.
pub const SPAWN_AGENT_TOOL: &str = "spawn_agent";
/// How deep subagents may nest. One level, deliberately: a subagent that can
/// spawn subagents can spawn a fork bomb, and every extra level multiplies
/// cost with no added capability. Depth is a hard cap, not a budget knob.
const MAX_SUBAGENT_DEPTH: u8 = 1;
/// A subagent's own budgets. Smaller than the parent's on purpose: a
/// delegated side quest should not be able to spend the caller's turn.
const SUBAGENT_MODEL_CALLS: usize = 12;
const SUBAGENT_TOOL_CALLS: u32 = 24;
/// Longest child transcript kept for the expanded view.
const MAX_SUBAGENT_TRANSCRIPT_CHARS: usize = 4000;

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

/// Where a turn sits in the subagent tree. Carried by value, not stored on
/// the engine: the engine is shared, the tree is not.
#[derive(Debug, Clone)]
pub struct TurnCtx {
    depth: u8,
    /// Set on a subagent turn; drives the tool catalog and the depth cap.
    subagent: Option<(String, String)>,
}

impl TurnCtx {
    pub fn root() -> Self {
        Self {
            depth: 0,
            subagent: None,
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
    /// Injected skills seam (implementation lives in `turya-skills`).
    skills_hook: Option<Arc<dyn SkillHook>>,
    session_id: String,
    /// Step budgets, hot-swappable per session via `set_budgets`
    /// (driven by `UpdateConfig` from the host). Interior mutability:
    /// the engine is shared as `Arc` across turns.
    budgets: RwLock<TurnBudgets>,
    /// Token budget for the active model, as two plain integers. The host
    /// builds this from catalog data; the kernel never sees `context_window`,
    /// a model id, or anything else catalog-shaped (Rule 3.1).
    context: RwLock<ContextBudget>,
    /// Automatic compaction on the threshold (host-driven setting).
    auto_compact: RwLock<bool>,
    /// Set when a model switch shrank the window below the current estimate:
    /// the next prompt compacts first.
    deferred_compact: RwLock<bool>,
    /// Consecutive compaction failures. Above the cap we stop trying this
    /// session, because a session whose context is irrecoverably over the
    /// limit otherwise retries forever.
    compact_failures: RwLock<u32>,
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
            skills_hook: None,
            session_id: "default".to_string(),
            budgets: RwLock::new(TurnBudgets::default()),
            context: RwLock::new(ContextBudget::generous()),
            auto_compact: RwLock::new(true),
            deferred_compact: RwLock::new(false),
            compact_failures: RwLock::new(0),
        }
    }

    /// The active provider, so the host can adjust optional capabilities
    /// (effort) without owning the concrete type.
    pub fn provider(&self) -> Arc<dyn LlmProvider> {
        self.provider.read().unwrap().clone()
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

    /// Hand the loop the active model's token budget. The host calls this
    /// when the model changes so the next turn (and any compaction check)
    /// uses the new window rather than the old one.
    pub fn set_context_budget(&self, total: u32, reserve: u32) {
        let next = ContextBudget { total, reserve };
        // A switch to a smaller window can put the existing conversation over
        // the limit. Record that and let the next prompt compact first, so a
        // switch never guarantees an overflow on the very next request.
        if next.total > 0 && next.total < self.context.read().unwrap().total {
            *self.deferred_compact.write().unwrap() = true;
        }
        *self.context.write().unwrap() = next;
    }

    /// The stored conversation, as the next turn would see it. Used by
    /// `/context` so the report describes the real thing, not a guess.
    pub async fn stored_transcript(&self) -> Transcript {
        match self.memory_hook {
            Some(ref hook) => {
                let prior = hook
                    .load_transcript(&self.session_id)
                    .await
                    .unwrap_or_default();
                let mut t = Transcript::new(&self.session_id);
                t.turns = prior;
                t
            }
            None => Transcript::new(&self.session_id),
        }
    }

    /// Host-driven setting: automatic compaction on the threshold.
    pub fn set_auto_compact(&self, enabled: bool) {
        *self.auto_compact.write().unwrap() = enabled;
    }

    /// True when the next prompt must compact first (a window shrank under us).
    pub fn needs_deferred_compact(&self) -> bool {
        *self.deferred_compact.read().unwrap()
    }

    pub fn clear_deferred_compact(&self) {
        *self.deferred_compact.write().unwrap() = false;
    }

    pub fn context_budget(&self) -> ContextBudget {
        *self.context.read().unwrap()
    }

    /// Compact before a turn when the threshold or a shrunken window demands
    /// it. Returns the transcript to run with (unchanged on failure), and
    /// records the outcome so a failing compactor stops retrying.
    async fn maybe_compact(
        &self,
        transcript: Transcript,
        event_tx: &mpsc::Sender<TuryaEvent>,
    ) -> Transcript {
        if transcript.turns.is_empty() {
            return transcript;
        }
        if *self.compact_failures.read().unwrap() >= MAX_COMPACT_FAILURES {
            return transcript;
        }
        let forced = *self.deferred_compact.read().unwrap();
        if !forced && !self.should_compact(&transcript) {
            return transcript;
        }
        match self.compact(None, event_tx).await {
            Ok((next, _)) => {
                *self.compact_failures.write().unwrap() = 0;
                *self.deferred_compact.write().unwrap() = false;
                // The compacted view is what this turn runs on. The store
                // still holds everything, so nothing is lost.
                self.persist_compacted(&next).await;
                next
            }
            Err(e) => {
                // Scope the guards: a std RwLock guard held across an await
                // makes the whole future non-Send, and the turn is spawned.
                let failures = {
                    let mut n = self.compact_failures.write().unwrap();
                    *n += 1;
                    *n
                };
                *self.deferred_compact.write().unwrap() = false;
                let _ = event_tx
                    .send(TuryaEvent::Error {
                        message: format!(
                            "compaction skipped ({e}); {failures} consecutive failure(s), \
                             giving up after {MAX_COMPACT_FAILURES}"
                        ),
                    })
                    .await;
                transcript
            }
        }
    }

    /// Record a compaction marker so the stored log explains the jump, and the
    /// pre-compaction turns stay searchable underneath it.
    async fn persist_compacted(&self, next: &Transcript) {
        let Some(hook) = &self.memory_hook else {
            return;
        };
        if let Some(turn) = next.turns.first() {
            let _ = hook.append_turn(&self.session_id, turn).await;
        }
    }

    /// Compact the session transcript. `focus` is the user's optional
    /// instruction ("focus on the auth fix"); `None` is the automatic pass.
    ///
    /// Prunes old tool output first (cheapest win), then asks the model for a
    /// What the model is told it can call.
    ///
    /// Built per turn rather than cached, because the registry can grow at
    /// runtime — an MCP server connecting mid-session must become callable
    /// without a restart.
    fn tool_catalog_instruction(&self, depth: u8) -> String {
        let mut lines = Vec::new();
        for name in self.tools.names() {
            let Some(t) = self.tools.get(&name) else {
                continue;
            };
            let first = t
                .description()
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            lines.push(format!("- {name}: {first}"));
        }
        if lines.is_empty() {
            return String::new();
        }
        // Advertised only where it is callable. A subagent that could spawn
        // another subagent would be a depth cap the model cannot see.
        if depth < MAX_SUBAGENT_DEPTH {
            lines.push(format!(
                "- {SPAWN_AGENT_TOOL}: delegate a self-contained side task to a \
                 subagent with its own context and budget. Arguments: \
                 name: string — a short label; task: string — full instructions, \
                 including what to return."
            ));
        }
        format!(
            "You can call these tools. To use one, emit a tool call with its exact \
             name and JSON arguments.\n{}",
            lines.join("\n")
        )
    }

    /// Run a delegated side task and return only its conclusion.
    ///
    /// The child gets its own transcript and its own budget, and its internal
    /// tool calls are not streamed to the user: the point of delegating is
    /// that the parent reasons over a summary, not over forty tool rows.
    async fn run_subagent(
        &self,
        call: &turya_protocol::ToolCall,
        event_tx: &mpsc::Sender<TuryaEvent>,
        perm_rx: &mut mpsc::Receiver<(String, PermissionDecision)>,
        ctx: &TurnCtx,
        mode: AgentMode,
    ) -> turya_protocol::ToolResult {
        if ctx.depth >= MAX_SUBAGENT_DEPTH {
            return turya_protocol::ToolResult {
                call_id: call.call_id.clone(),
                success: false,
                output: String::new(),
                error: Some(format!(
                    "{SPAWN_AGENT_TOOL} is not available inside a subagent: nesting is \
                     capped at one level."
                )),
            };
        }
        let name = call
            .parameters
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("subagent")
            .to_string();
        let task = match call.parameters.get("task").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => {
                return turya_protocol::ToolResult {
                    call_id: call.call_id.clone(),
                    success: false,
                    output: String::new(),
                    error: Some(
                        "spawn_agent needs a non-empty `task` string describing the work"
                            .to_string(),
                    ),
                };
            }
        };
        let task_id = format!("{}/{}", call.call_id, name);
        let _ = event_tx
            .send(TuryaEvent::SubagentStarted {
                task_id: task_id.clone(),
                name: name.clone(),
                task: task.clone(),
            })
            .await;

        // The child's events go to a private channel drained into a
        // transcript, so the user sees one row instead of the child's innards.
        // Inline, not spawned: the permission channel below is shared state
        // and a nested call keeps exactly one owner of it.
        let (child_tx, mut child_rx) = mpsc::channel::<TuryaEvent>(256);
        let mut transcript = String::new();
        let child_turn_id = format!("{task_id}#1");
        // The child and the drain run concurrently in one task. Draining only
        // after the child returns would deadlock the moment a child emitted
        // more events than the channel holds: it would block on a send that
        // nobody was reading, forever, with no error to show for it.
        //
        // Boxed because the call graph is cyclic: a turn dispatches a tool,
        // which can start another turn. Without a type erasure here the
        // future's own type is infinitely large and the borrow checker is
        // within its rights to refuse.
        // Scoped so the drain future's borrows of `child_rx` and
        // `transcript` end before either is used again below.
        // The block ends the drain future's borrows of `child_rx` and
        // `transcript` before either is read again.
        let parent_events = event_tx;
        {
            let child = Box::pin(self.run_turn_at(
                &child_turn_id,
                &task,
                mode,
                &[],
                child_tx,
                perm_rx,
                TurnCtx {
                    depth: ctx.depth + 1,
                    subagent: Some((task_id.clone(), name.clone())),
                },
            ));
            let drain = async {
                while let Some(ev) = child_rx.recv().await {
                    match ev {
                        TuryaEvent::TokenDelta { chunk } => transcript.push_str(&chunk),
                        // A child's permission request must reach the user.
                        // Discarding it deadlocks the child: it blocks on
                        // `perm_rx.recv()` waiting for an answer to a question
                        // nobody was ever shown, and the parent is blocked
                        // inline waiting for the child. The decision travels
                        // back through the shared `perm_rx`, so forwarding the
                        // request is the whole fix.
                        TuryaEvent::PermissionRequested {
                            request_id,
                            action,
                            risk_level,
                            details,
                        } => {
                            let _ = parent_events
                                .send(TuryaEvent::PermissionRequested {
                                    request_id,
                                    action,
                                    risk_level,
                                    details,
                                })
                                .await;
                        }
                        // Everything else about the child stays private, which
                        // is the point of delegating.
                        _ => {}
                    }
                }
            };
            let (_, ()) = tokio::join!(child, drain);
        }
        drop(child_rx);
        let summary = transcript.trim().to_string();
        let clipped: String = transcript
            .chars()
            .take(MAX_SUBAGENT_TRANSCRIPT_CHARS)
            .collect();
        let _ = event_tx
            .send(TuryaEvent::SubagentFinished {
                task_id: task_id.clone(),
                name: name.clone(),
                summary: summary.clone(),
                transcript: clipped,
            })
            .await;

        // An empty child transcript is a failure the parent must see, not an
        // empty success it will try to interpret.
        if summary.is_empty() {
            return turya_protocol::ToolResult {
                call_id: call.call_id.clone(),
                success: false,
                output: String::new(),
                error: Some(format!(
                    "subagent '{name}' returned nothing; treat the task as unfulfilled"
                )),
            };
        }
        turya_protocol::ToolResult {
            call_id: call.call_id.clone(),
            success: true,
            output: format!("Subagent '{name}' reported:\n{summary}"),
            error: None,
        }
    }

    /// structured summary with tools structurally dropped, then keeps the last
    /// two turns verbatim so exact values survive. Returns the new transcript
    /// and a short note describing what was kept, or `None` when the
    /// conversation is too short to be worth compacting.
    pub async fn compact(
        &self,
        focus: Option<&str>,
        event_tx: &mpsc::Sender<TuryaEvent>,
    ) -> Result<(Transcript, String), String> {
        let mut transcript = match self.memory_hook {
            Some(ref hook) => {
                let prior = hook.load_transcript(&self.session_id).await?;
                let mut t = Transcript::new(&self.session_id);
                t.turns = prior;
                t
            }
            None => return Err("no session store: nothing to compact".to_string()),
        };
        if transcript.turns.len() < 3 {
            return Err("nothing to compact yet (need at least 3 turns)".to_string());
        }
        let before = transcript.turns.len();
        let _ = event_tx
            .send(TuryaEvent::CompactionStarted { turns: before })
            .await;

        let pruned = crate::context::prune_transcript(&mut transcript);
        let tail = crate::context::tail_turns(&transcript);

        // One summarising pass, tools dropped. A failure here is not fatal:
        // the caller keeps the pruned transcript, which is still smaller.
        let summary = self
            .summarize(&transcript, focus, event_tx)
            .await
            .unwrap_or_else(|_| {
                format!("Automatic summary unavailable; {pruned} older tool results were pruned.")
            });

        let marker = format!(
            "{before} turns -> summary + last {} ({} tool results pruned)",
            tail.len(),
            pruned
        );
        let next = crate::context::compacted(&self.session_id, &summary, &tail, &marker);
        let _ = event_tx
            .send(TuryaEvent::CompactionCompleted {
                before_turns: before,
                after_turns: next.turns.len(),
                summary,
            })
            .await;
        Ok((next, marker))
    }

    /// One text-only pass that produces a compaction summary. Tool calls are
    /// announced as denied and never executed, so this cannot act.
    async fn summarize(
        &self,
        transcript: &Transcript,
        focus: Option<&str>,
        event_tx: &mpsc::Sender<TuryaEvent>,
    ) -> Result<String, String> {
        let mut probe = transcript.clone();
        probe.push(Part::Instruction {
            text: crate::context::compaction_prompt(focus),
        });
        let (step_tx, mut step_rx) = mpsc::channel(32);
        let provider = self.provider.read().unwrap().clone();
        let handle = tokio::spawn(async move { provider.generate_turn(&probe, step_tx).await });
        let mut out = String::new();
        while let Some(step) = step_rx.recv().await {
            match step {
                ProviderStep::Token(chunk) => out.push_str(&chunk),
                // Structural: a tool call during compaction is dropped, not run.
                ProviderStep::CallTool(_) => {
                    let _ = event_tx
                        .send(TuryaEvent::Error {
                            message: "compaction summary attempted a tool call; ignored"
                                .to_string(),
                        })
                        .await;
                }
                ProviderStep::Finish => break,
            }
        }
        match handle.await {
            Ok(Ok(())) if !out.trim().is_empty() => Ok(out),
            Ok(Ok(())) => Err("model returned an empty summary".to_string()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("compaction task failed: {e}")),
        }
    }

    /// Would a compaction run right now? The threshold is a fraction of the
    /// usable window, so a bigger model compacts later in absolute terms.
    pub fn should_compact(&self, transcript: &Transcript) -> bool {
        let budget = self.context_budget();
        if !*self.auto_compact.read().unwrap() {
            return false;
        }
        let estimate = crate::context::estimate_transcript_tokens(transcript);
        // Never on the first turns: a young session is not a context problem.
        transcript.turns.len() >= 3 && budget.over(estimate * 100 / COMPACT_AT_PERCENT)
    }

    /// Rough token estimate for the conversation, from the characters we
    /// hold. Provider-reported usage is authoritative when available; this
    /// is the floor, and it deliberately never claims precision.
    pub fn estimate_tokens(text: &str) -> u32 {
        // ~4 chars/token, and only counting real characters: CJK is closer to
        // 1 per char, so this is a floor that errs low, not high.
        let chars = text.chars().count();
        ((chars / 4) as u32).max(1)
    }

    pub fn with_memory_hook(mut self, hook: Arc<dyn MemoryHook>) -> Self {
        self.memory_hook = Some(hook);
        self
    }

    /// Inject the skills seam. Its catalog is injected once per turn, before
    /// the user's prompt, so the model knows what it can load.
    pub fn with_skills_hook(mut self, hook: Arc<dyn SkillHook>) -> Self {
        self.skills_hook = Some(hook);
        self
    }

    /// Load one skill's body, for the `load_skill` tool.
    pub async fn load_skill_body(&self, name: &str) -> Option<String> {
        self.skills_hook.as_ref()?.load_skill(name).await
    }

    /// The skills the session currently offers, for `/skills`.
    pub async fn available_skills(&self) -> Vec<crate::hooks::SkillRef> {
        match self.skills_hook {
            Some(ref hook) => hook.skill_catalog(&self.session_id).await,
            None => Vec::new(),
        }
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
        self.run_turn_at(
            turn_id,
            prompt,
            mode,
            attachments,
            event_tx,
            &mut perm_rx,
            TurnCtx::root(),
        )
        .await
    }

    /// One turn, at a known place in the subagent tree.
    ///
    /// Depth is threaded here rather than held on the engine because the
    /// engine is shared across concurrent turns; a field would be a race.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn_at(
        &self,
        turn_id: &str,
        prompt: &str,
        mode: AgentMode,
        attachments: &[turya_protocol::Attachment],
        event_tx: mpsc::Sender<TuryaEvent>,
        perm_rx: &mut mpsc::Receiver<(String, PermissionDecision)>,
        ctx: TurnCtx,
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
        // Kept for the subagent dispatch below, which happens inside
        // `run_pass` after the original was consumed by `TurnStarted`.
        let subagent_mode = mode;
        // Continue the stored conversation when a memory seam is present, so
        // the model actually remembers earlier turns. Without it the turn
        // starts from nothing, which is correct for a first turn and wrong
        // for every one after it.
        let transcript = match self.memory_hook {
            Some(ref hook) => {
                let prior = hook
                    .load_transcript(&self.session_id)
                    .await
                    .unwrap_or_default();
                let mut t = Transcript::new(&self.session_id);
                t.turns = prior;
                t
            }
            None => Transcript::new(&self.session_id),
        };
        // Compaction happens *before* the new user turn is recorded, so the
        // summary covers the history and not the question being asked.
        let mut transcript = self.maybe_compact(transcript, &event_tx).await;
        transcript.start_turn(turn_id);
        // The user's turn is recorded here, once. Providers serialize the
        // transcript as-is; there is no separate prompt to append.
        // Skills are advertised before the prompt, once, as instructions
        // rather than as conversation: the model reads the catalog and loads a
        // body only if a task actually matches.
        // The tool catalog goes in as an instruction, not as a provider
        // feature: the model has no other way to learn a name, and an MCP
        // server's tools are unknowable by guesswork. Names and one-line
        // summaries only — the schema stays with the tool.
        let catalog = self.tool_catalog_instruction(ctx.depth);
        if !catalog.is_empty() {
            transcript.push(Part::Instruction { text: catalog });
        }
        if let Some(ref hook) = self.skills_hook {
            let catalog = hook.skill_catalog(&self.session_id).await;
            let rendered = crate::skills_catalog(&catalog);
            if !rendered.is_empty() {
                transcript.push(Part::Instruction { text: rendered });
            }
        }
        // A delegated turn is told who asked and what to hand back, so it
        // writes its answer for the parent instead of addressing the user.
        let framed_prompt = match &ctx.subagent {
            Some((task_id, name)) => format!(
                "You are the subagent '{name}' (task {task_id}), working on a delegated \
                 side task. The agent that delegated to you will read only your final \
                 message, so make it a complete answer: state the result, not the \
                 steps you took.\n\nTask:\n{prompt}"
            ),
            None => prompt.to_string(),
        };
        transcript.push(Part::UserText {
            text: framed_prompt,
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
        // The previous tool call's signature, for the repetition guard.
        let mut last_call: Option<String> = None;

        // A subagent is billed against the caller's turn, so it gets its own
        // smaller allowance rather than the session default.
        let budgets = if ctx.depth > 0 {
            TurnBudgets {
                model_calls: SUBAGENT_MODEL_CALLS,
                tool_calls: SUBAGENT_TOOL_CALLS,
            }
        } else {
            budgets
        };

        for _pass in 0..budgets.model_calls {
            passes += 1;
            let gate = ToolGate {
                executions: &mut tool_executions,
                cap: budgets.tool_calls,
                execute: true,
            };
            let outcome = self
                .run_pass(
                    &transcript,
                    &event_tx,
                    perm_rx,
                    gate,
                    &ctx,
                    subagent_mode,
                    &mut last_call,
                )
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
                .run_pass(
                    &transcript,
                    &event_tx,
                    perm_rx,
                    gate,
                    &ctx,
                    subagent_mode,
                    &mut last_call,
                )
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
            // Persist the turn as one atomic log record. Written after the
            // turn so a crash loses at most the in-flight turn, and the
            // store's repair pass closes any tool call left open.
            if let Some(turn) = transcript.turns.last() {
                let _ = hook.append_turn(&self.session_id, turn).await;
            }
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
    #[allow(clippy::too_many_arguments)]
    async fn run_pass(
        &self,
        transcript: &Transcript,
        event_tx: &mpsc::Sender<TuryaEvent>,
        perm_rx: &mut mpsc::Receiver<(String, PermissionDecision)>,
        gate: ToolGate<'_>,
        ctx: &TurnCtx,
        mode: AgentMode,
        last_call: &mut Option<String>,
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
                    let result = self
                        .execute_tool_call(&call, event_tx, perm_rx, ctx, mode, last_call)
                        .await;
                    // On failure the output is usually the whole point. A
                    // shell command that fails says "Exited with code: 101",
                    // which tells the model nothing, while its stderr holds
                    // the actual compiler errors. Sending only the reason
                    // leaves the model blind exactly when it needs to see.
                    let summary = if result.success || result.output.trim().is_empty() {
                        result
                            .error
                            .clone()
                            .filter(|_| !result.success)
                            .unwrap_or_else(|| result.output.clone())
                    } else {
                        match result.error.clone() {
                            Some(reason) if !reason.trim().is_empty() => {
                                format!("{reason}\n{}", result.output)
                            }
                            _ => result.output.clone(),
                        }
                    };
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
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_call(
        &self,
        call: &turya_protocol::ToolCall,
        event_tx: &mpsc::Sender<TuryaEvent>,
        perm_rx: &mut mpsc::Receiver<(String, PermissionDecision)>,
        ctx: &TurnCtx,
        mode: AgentMode,
        last_call: &mut Option<String>,
    ) -> turya_protocol::ToolResult {
        // Repetition guard. A model that calls the same tool with the same
        // arguments twice in a row is stuck: it will do it until the step
        // budget runs out, producing an identical result it has already seen.
        // Observed in the wild - nine identical `view_file` calls, each
        // followed by byte-identical text, which reads as a frozen UI.
        //
        // Only *consecutive* duplicates are refused. Re-reading a file after
        // writing it is legitimate, and a blanket "never call twice" rule
        // would break real work.
        let signature = format!("{}::{}", call.tool_name, call.parameters);
        if last_call.as_deref() == Some(signature.as_str()) {
            // Deliberately *not* cleared: a model that alternates refusal and
            // success would otherwise halve the waste and keep going, which
            // still reads as a frozen screen. Repeats stay refused until the
            // model does something genuinely different.
            let res = turya_protocol::ToolResult {
                call_id: call.call_id.clone(),
                success: false,
                output: String::new(),
                error: Some(format!(
                    "'{}' was just called with these exact arguments and you already have \
                     its result. Repeating it cannot produce anything new: change your \
                     approach, use different arguments, or answer without it.",
                    call.tool_name
                )),
            };
            let _ = event_tx
                .send(TuryaEvent::ToolCallInitiated(call.clone()))
                .await;
            let _ = event_tx
                .send(TuryaEvent::ToolCallCompleted(res.clone()))
                .await;
            return res;
        }
        *last_call = Some(signature);
        let _ = event_tx
            .send(TuryaEvent::ToolCallInitiated(call.clone()))
            .await;
        // `spawn_agent` is answered by the kernel, not the registry: it needs
        // this engine, and a tool holding an `Arc` back to the engine that
        // owns the registry is a cycle. Dispatching here also keeps the
        // capability gate in one place.
        if call.tool_name == SPAWN_AGENT_TOOL {
            return self.run_subagent(call, event_tx, perm_rx, ctx, mode).await;
        }
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
