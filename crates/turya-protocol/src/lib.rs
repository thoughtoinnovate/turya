use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    Plan,
    Build,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Open,
    ReviewForMe,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Moderate,
    High,
    Critical,
}

/// Everything `/settings` can change, in one shape because it is written as
/// one file. `None` on a field leaves it alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiSettings {
    pub user_bg: Option<String>,
    pub assistant_bg: Option<String>,
    pub tool_bg: Option<String>,
    /// `auto` | `on` | `off`.
    pub mouse: Option<String>,
    pub no_color: Option<bool>,
}

/// What `/mcp` shows: a server, its command, and the tools it added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub command: String,
    pub tools: Vec<String>,
    /// Why this server is absent or degraded. Present means the user has
    /// something actionable; absent means it connected cleanly.
    pub error: Option<String>,
}

/// A tool the model may call, in vendor-neutral shape.
///
/// The kernel owns the registry, so the kernel decides what is callable and
/// hands this to the provider; the provider only knows how to render it into
/// its own wire format. That split is why a runtime-discovered tool (an MCP
/// server's) reaches the model as a real declared function instead of a
/// sentence of prose in the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    /// What the tool is for, including when to reach for it. This is the only
    /// place behavioural guidance can live, so it is written for the model.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// A skill advertised to the model (tier 1 of progressive disclosure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRef {
    pub name: String,
    pub description: String,
    /// Absolute path to the `SKILL.md`; the model reads it with its normal
    /// file tool, so activation needs no special machinery.
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub parameters: serde_json::Value,
    /// Opaque provider reasoning signature that must be replayed verbatim
    /// with the call (Gemini's `thoughtSignature`). Providers that do not use
    /// one send `None`; a provider that needs it and gets `None` rejects the
    /// request, so this is carried on the call rather than recomputed.
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticItem {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
    pub severity: String,
}

/// A file handed to the model with a prompt (forward-only: required vec,
/// never `Option` — "no attachments" is the empty vec).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub path: PathBuf,
    pub mime: String,
}

/// Opaque turn identifier. The engine mints these; the registry and the
/// transcript index by them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub String);

/// One typed unit of a turn. The engine accumulates these; providers and
/// the TUI each render them. One exact format: no defaults, no `Unknown`
/// catch-alls — a binary that does not know a shape fails to parse it
/// (Rule 5.3), it never silently drops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Part {
    Text {
        text: String,
    },
    /// The user's own message for this turn. Distinct from `Instruction`
    /// (harness-authored) because the two serialize to the same role but
    /// mean different things in a resumed transcript.
    UserText {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
        /// Replayed verbatim; see `ToolCall::signature`.
        signature: Option<String>,
    },
    ToolResult {
        call_id: String,
        output: String,
        truncated: bool,
    },
    /// Harness-authored user message: the budget-exhaustion wrap-up request,
    /// later the compaction marker and skill injection. Modelled as a part
    /// (not a string splice into history) so it survives compaction and
    /// re-serializes with a real role.
    Instruction {
        text: String,
    },
    Attachment(Attachment),
    Image(Attachment),
}

/// One model turn: everything said, called, and returned, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub parts: Vec<Part>,
}

/// A session's conversation: ordered turns. Built once per `run_turn`,
/// never rewritten — compaction appends a marker part, it does not edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    pub session_id: String,
    pub turns: Vec<Turn>,
}

impl Transcript {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            turns: Vec::new(),
        }
    }

    pub fn start_turn(&mut self, turn_id: &str) {
        self.turns.push(Turn {
            id: TurnId(turn_id.to_string()),
            parts: Vec::new(),
        });
    }

    /// Append to the latest turn (starts one if none exists).
    pub fn push(&mut self, part: Part) {
        if self.turns.is_empty() {
            self.start_turn("t0");
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.parts.push(part);
        }
    }

    /// Append a batch of parts to the latest turn.
    pub fn extend(&mut self, parts: Vec<Part>) {
        for part in parts {
            self.push(part);
        }
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn part_count(&self) -> usize {
        self.turns.iter().map(|t| t.parts.len()).sum()
    }

    /// Every text payload in the conversation, in order. Used for context
    /// estimation and rendering, so it counts the *user's* turns and harness
    /// instructions too — leaving them out under-reports the window.
    pub fn texts(&self) -> Vec<&str> {
        self.turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .filter_map(|p| match p {
                Part::Text { text }
                | Part::Reasoning { text }
                | Part::UserText { text }
                | Part::Instruction { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Two integers the loop compares. The engine never sees the catalog's
/// `context_window` — the host builds this datum from catalog data and
/// hands it in at turn start (Rule 3.1: core holds numbers, not knowledge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    pub total: u32,
    pub reserve: u32,
}

impl ContextBudget {
    /// Generous default while no catalog datum is wired in (A4 fills it).
    pub fn generous() -> Self {
        Self {
            total: 200_000,
            reserve: 20_000,
        }
    }

    pub fn usable(&self) -> u32 {
        self.total.saturating_sub(self.reserve)
    }

    pub fn over(&self, estimate: u32) -> bool {
        estimate > self.usable()
    }
}

/// Who authored a message. Providers map this to their own role names
/// (`assistant`/`user`, or Gemini's `model`/`user`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// A provider-neutral message block. This is the seam between the
/// transcript and the wire: role assembly happens once, here, and each
/// provider only serializes. Adding a block type is additive to the
/// protocol (Rule 5.2 still forbids shims, not new variants).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum MessagePart {
    Text {
        text: String,
    },
    /// Model reasoning, surfaced but not re-sent as instructions.
    Reasoning {
        text: String,
    },
    ToolUse {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
        /// Opaque reasoning signature to replay with the call.
        signature: Option<String>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        truncated: bool,
    },
    File {
        path: PathBuf,
        mime: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<MessagePart>,
}

impl Transcript {
    /// Project the transcript into wire-ready messages.
    ///
    /// Role rules, chosen to match both vendors' requirements: a `ToolCall`
    /// is assistant content, but its `ToolResult` is a **user** message (the
    /// Anthropic `tool_result` block convention, which Gemini's
    /// `functionResponse` mirrors) and never merges into the assistant turn.
    /// Consecutive same-role *text* merges, so a streamed answer stays one
    /// block instead of one message per delta.
    ///
    /// There is deliberately **no `prompt` argument**: the user's turn lives
    /// in the transcript as `Part::UserText`. Re-appending a separate prompt
    /// on every call duplicated the user turn and made the request start with
    /// an assistant tool-call, which the APIs reject.
    pub fn to_messages(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for turn in &self.turns {
            for part in &turn.parts {
                let (role, piece) = match part {
                    Part::Text { text } => {
                        (Role::Assistant, MessagePart::Text { text: text.clone() })
                    }
                    Part::UserText { text } | Part::Instruction { text } => {
                        (Role::User, MessagePart::Text { text: text.clone() })
                    }
                    Part::Reasoning { text } => (
                        Role::Assistant,
                        MessagePart::Reasoning { text: text.clone() },
                    ),
                    Part::ToolCall {
                        call_id,
                        tool_name,
                        arguments,
                        signature,
                    } => (
                        Role::Assistant,
                        MessagePart::ToolUse {
                            call_id: call_id.clone(),
                            name: tool_name.clone(),
                            arguments: arguments.clone(),
                            signature: signature.clone(),
                        },
                    ),
                    Part::ToolResult {
                        call_id,
                        output,
                        truncated,
                    } => (
                        Role::User,
                        MessagePart::ToolResult {
                            call_id: call_id.clone(),
                            content: output.clone(),
                            is_error: false,
                            truncated: *truncated,
                        },
                    ),
                    Part::Attachment(a) | Part::Image(a) => (
                        Role::User,
                        MessagePart::File {
                            path: a.path.clone(),
                            mime: a.mime.clone(),
                        },
                    ),
                };
                let is_result = matches!(piece, MessagePart::ToolResult { .. });
                let prev_is_result = out.last().is_some_and(|last| {
                    matches!(last.content.last(), Some(MessagePart::ToolResult { .. }))
                });
                // A tool result always opens its own message (it must follow
                // its tool_use in the very next user turn). Everything else
                // batches with the previous message of the same role, so a
                // streamed answer plus its tool calls ship as one block.
                if !is_result && !prev_is_result {
                    if let Some(last) = out.last_mut() {
                        if last.role == role {
                            last.content.push(piece);
                            continue;
                        }
                    }
                }
                out.push(Message {
                    role,
                    content: vec![piece],
                });
            }
        }
        out
    }
}

/// Session header. Out-of-log metadata (Rule: metadata is storage, not
/// conversation state), so it lives in its own row and never enters the
/// transcript. `format_version` exists to produce a good error message on a
/// foreign database — never to migrate one (AGENTS.md Rule 5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: String,
    pub parent_id: Option<String>,
    /// Highest committed turn sequence; the next append must continue it.
    pub seq: u32,
    /// Set when the session was closed by a crash-repair pass.
    pub repaired: bool,
}

/// Commands sent from any UI/Client to the Turya Core Engine
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum TuryaCommand {
    SubmitPrompt {
        prompt: String,
        mode: AgentMode,
        /// Files handed to the model. Required vec: empty means none.
        attachments: Vec<Attachment>,
    },
    /// Enqueue work to run after the current turn finishes. The prompt is
    /// not seen by the running agent: queueing is for follow-ups you want
    /// answered in order, not for changing its mind.
    QueuePrompt {
        prompt: String,
    },
    /// Clear the pending queue.
    ClearQueue,
    ResolvePermission {
        request_id: String,
        decision: PermissionDecision,
    },
    AbortTurn,
    UpdateConfig {
        permission_mode: Option<PermissionMode>,
        provider: Option<String>,
        model: Option<String>,
        /// Per-turn step budgets (`/steps`): model generations and tool
        /// executions. `None` leaves that side unchanged. Additive schema:
        /// old clients simply never send these.
        max_steps: Option<usize>,
        max_tool_calls: Option<u32>,
        /// TUI appearance written by `/settings`. The client applies these
        /// immediately and sends them here to be persisted, so a restart
        /// keeps what the user just chose.
        ui: Option<UiSettings>,
    },
    /// List the skills available to this session (drives `/skills`).
    ListSkills,
    /// Report configured MCP servers and the tools each contributed.
    McpStatus,
    /// Load one skill's body (drives the `load_skill` tool).
    LoadSkill {
        name: String,
    },
    /// Ask what reasoning effort levels the active model accepts.
    QueryEfforts,
    /// Set the reasoning effort for subsequent turns.
    SetEffort {
        effort: Option<String>,
    },
    /// Compact the session (`/compact`). `focus` is the user's optional
    /// instruction, e.g. "focus on the auth bug fix".
    Compact {
        focus: Option<String>,
    },
    /// Ask for the context breakdown (`/context`).
    ContextReport,
    /// List stored sessions, newest first (`turya sessions`, `/sessions`).
    ListSessions {
        /// Restrict to one working directory; `None` lists every session.
        cwd: Option<String>,
        limit: Option<usize>,
    },
    /// Replay a stored session into the engine (drive by `turya resume`).
    ResumeSession {
        id: String,
    },
    /// List registered providers and their models (drives `/models`).
    ListProviders,
    /// Query dual-slot auth state for one provider (drives `/auth` badges).
    GetAuthStatus {
        provider: String,
    },
    /// Start an auth flow; the engine answers with `AuthFlowStarted`.
    BeginAuthFlow {
        provider: String,
        method: String,
    },
    /// Deliver one user input (key text, pasted code) to a running flow.
    SubmitAuthInput {
        flow_id: String,
        payload: String,
    },
    /// Abandon a running auth flow.
    CancelAuthFlow {
        flow_id: String,
    },
    /// Ask the host for the current provider selection (answered with
    /// `ProviderState`; drives the status bar without guessing).
    GetProviderState,
}

/// Events broadcast by the Turya Core Engine to all connected UIs/Clients
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum TuryaEvent {
    /// Answer to `ListSessions`.
    SessionsListed {
        sessions: Vec<SessionMeta>,
    },
    /// Answer to `ResumeSession`: the replayed transcript plus its header.
    SessionResumed {
        session: Box<SessionMeta>,
        transcript: Transcript,
    },
    /// A subagent began. The parent sees this as one collapsed row; the
    /// child's own tool calls stay inside it.
    SubagentStarted {
        task_id: String,
        name: String,
        task: String,
    },
    /// A subagent finished. `summary` is what the parent was given, so the
    /// user can see exactly what came back.
    SubagentFinished {
        task_id: String,
        name: String,
        summary: String,
        /// The child's whole transcript, bounded, for the expanded view.
        transcript: String,
    },
    /// Answer to `McpStatus`: one row per server, plus its tools.
    McpStatus {
        servers: Vec<McpServerStatus>,
    },
    /// Answer to `ListSkills`, with any discovery problems attached so a
    /// malformed skill is visible rather than silently missing.
    SkillsListed {
        skills: Vec<SkillRef>,
        warnings: Vec<String>,
    },
    /// What the active model supports, and what is currently selected.
    /// `efforts` is empty when the source does not say, and
    /// `supported` is `None` when we genuinely do not know - which is
    /// different from "this model cannot reason".
    EffortsChanged {
        model: String,
        supported: Option<bool>,
        efforts: Vec<String>,
        current: Option<String>,
    },
    /// Queue state, emitted whenever it changes so a client can show a
    /// count without tracking commands itself.
    QueueChanged {
        pending: usize,
    },
    /// Compaction is starting (spinner + a note in the transcript).
    CompactionStarted {
        turns: usize,
    },
    /// Compaction finished. `summary` is shown in a dimmed row; the full
    /// pre-compaction conversation stays in the session log and is
    /// searchable, which is what makes compaction non-destructive.
    CompactionCompleted {
        before_turns: usize,
        after_turns: usize,
        summary: String,
    },
    TurnStarted {
        turn_id: String,
        mode: AgentMode,
    },
    TokenDelta {
        chunk: String,
    },
    ToolCallInitiated(ToolCall),
    ToolCallCompleted(ToolResult),
    PermissionRequested {
        request_id: String,
        action: String,
        risk_level: RiskLevel,
        details: String,
    },
    DiagnosticsReceived {
        diagnostics: Vec<DiagnosticItem>,
    },
    TurnCompleted {
        turn_id: String,
        success: bool,
    },
    Error {
        message: String,
    },
    /// Registry snapshot answering `ListProviders`.
    ProvidersListed {
        providers: Vec<ProviderSummary>,
    },
    /// Dual-slot auth state answering `GetAuthStatus` (also pushed on change).
    AuthStatusChanged {
        provider: String,
        api_key: String,
        oauth: String,
    },
    /// An auth flow needs a user action; token exchange stays server-side.
    AuthFlowStarted {
        flow_id: String,
        action: AuthAction,
    },
    AuthFlowCompleted {
        flow_id: String,
        provider: String,
        method: String,
    },
    AuthFlowFailed {
        flow_id: String,
        reason: String,
    },
    /// Model catalog changed for a provider (refresh finished).
    CatalogUpdated {
        provider: String,
    },
    /// Current provider selection (answers `GetProviderState`; also pushed
    /// after every successful switch so clients never guess).
    ProviderState {
        provider: String,
        model: String,
        /// Credential source: `env | stored-key | oauth | mock`.
        via: String,
    },
}

/// One provider + its models, as shown in the `/models` browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelSummary>,
    /// Slot badges: `"env" | "stored" | "connected" | "missing" | "unsupported"`.
    pub api_key: String,
    pub oauth: String,
}

/// One model in the `/models` browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub id: String,
    pub display_name: String,
    /// `live | cached | snapshot | static`.
    pub source: String,
}

/// User action required to advance an auth flow (rendered by the client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum AuthAction {
    /// Open this URL in a browser; the callback is captured server-side.
    OpenBrowser { url: String },
    /// Prompt for masked text input (API key or pasted auth code).
    PromptMasked { prompt: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialization_roundtrip() {
        let cmd = TuryaCommand::SubmitPrompt {
            prompt: "Refactor auth".to_string(),
            mode: AgentMode::Build,
            attachments: Vec::new(),
        };
        let serialized = serde_json::to_string(&cmd).unwrap();
        assert!(serialized.contains("SubmitPrompt"));
    }

    #[test]
    fn test_provider_auth_messages_roundtrip() {
        // New variants must serialize with stable tags (additive-only schema).
        let cmds = vec![
            TuryaCommand::ListProviders,
            TuryaCommand::GetAuthStatus {
                provider: "gemini".to_string(),
            },
            TuryaCommand::BeginAuthFlow {
                provider: "gemini".to_string(),
                method: "api-key".to_string(),
            },
            TuryaCommand::UpdateConfig {
                permission_mode: None,
                provider: Some("gemini".to_string()),
                model: Some("gemini-2.5-pro".to_string()),
                max_steps: None,
                max_tool_calls: None,
                ui: None,
            },
        ];
        for cmd in cmds {
            let s = serde_json::to_string(&cmd).unwrap();
            let back: TuryaCommand = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
        let evt = TuryaEvent::AuthFlowStarted {
            flow_id: "f1".to_string(),
            action: AuthAction::OpenBrowser {
                url: "https://example.test/auth".to_string(),
            },
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains("AuthFlowStarted") && s.contains("OpenBrowser"));
        let listed = TuryaEvent::ProvidersListed {
            providers: vec![ProviderSummary {
                id: "gemini".to_string(),
                display_name: "Gemini".to_string(),
                models: vec![ModelSummary {
                    id: "gemini-2.5-pro".to_string(),
                    display_name: "Gemini 2.5 Pro".to_string(),
                    source: "static".to_string(),
                }],
                api_key: "missing".to_string(),
                oauth: "missing".to_string(),
            }],
        };
        let s = serde_json::to_string(&listed).unwrap();
        let back: TuryaEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);

        // Provider selection state round-trips too.
        let cmd = TuryaCommand::GetProviderState;
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(s.contains("GetProviderState"));
        let evt = TuryaEvent::ProviderState {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash".to_string(),
            via: "stored-key".to_string(),
        };
        let s = serde_json::to_string(&evt).unwrap();
        let back: TuryaEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);
    }

    #[test]
    fn every_part_variant_round_trips() {
        let parts = vec![
            Part::Text {
                text: "hello".to_string(),
            },
            Part::Reasoning {
                text: "hmm".to_string(),
            },
            Part::ToolCall {
                call_id: "c1".to_string(),
                tool_name: "run_bash".to_string(),
                arguments: serde_json::json!({"cmd": "ls"}),
                signature: Some("sig-abc".to_string()),
            },
            Part::ToolResult {
                call_id: "c1".to_string(),
                output: "ok".to_string(),
                truncated: false,
            },
            Part::Attachment(Attachment {
                path: PathBuf::from("/tmp/a.png"),
                mime: "image/png".to_string(),
            }),
            Part::Image(Attachment {
                path: PathBuf::from("/tmp/b.png"),
                mime: "image/png".to_string(),
            }),
        ];
        for p in parts {
            let s = serde_json::to_string(&p).unwrap();
            let back: Part = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
        // Forward-only: unknown shapes fail to parse, never silently drop.
        assert!(serde_json::from_str::<Part>(r#"{"type":"Nope"}"#).is_err());
        assert!(serde_json::from_str::<Part>(r#"{"type":"Text"}"#).is_err());
    }

    #[test]
    fn transcript_accumulates_in_order() {
        let mut t = Transcript::new("s1");
        t.push(Part::Text {
            text: "first".to_string(),
        });
        assert_eq!(t.turn_count(), 1);
        t.start_turn("t2");
        t.push(Part::Text {
            text: "second".to_string(),
        });
        assert_eq!(t.turn_count(), 2);
        assert_eq!(t.part_count(), 2);
        assert_eq!(t.texts(), vec!["first", "second"]);
        let s = serde_json::to_string(&t).unwrap();
        let back: Transcript = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn submit_prompt_with_attachments_round_trips() {
        let cmd = TuryaCommand::SubmitPrompt {
            prompt: "look".to_string(),
            mode: AgentMode::Build,
            attachments: vec![Attachment {
                path: PathBuf::from("/tmp/a.png"),
                mime: "image/png".to_string(),
            }],
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: TuryaCommand = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);
    }

    #[test]
    fn context_budget_clamps_and_compares() {
        let b = ContextBudget {
            total: 100_000,
            reserve: 20_000,
        };
        assert_eq!(b.usable(), 80_000);
        assert!(!b.over(80_000));
        assert!(b.over(80_001));
        let tiny = ContextBudget {
            total: 10,
            reserve: 20,
        };
        assert_eq!(tiny.usable(), 0);
        assert!(tiny.over(1));
    }
}
