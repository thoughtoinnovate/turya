use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use tokio::sync::mpsc;
use turya_protocol::{AgentMode, Part, PermissionDecision, Transcript, TuryaCommand, TuryaEvent};

pub mod slash;

pub mod flows;

use flows::{AuthFlow, AuthStage, BrowserFlow, BrowserMode, Flow};
use slash::{Completer, SlashRegistry};

/// Centered overlay rect sitting just above the input pane (which sits
/// above the 1-row status bar). `reserved_bottom` is the input height + 1
/// so growing multiline drafts push overlays up instead of under them.
fn centered_popup(area: Rect, width: u16, height: u16, reserved_bottom: u16) -> Rect {
    let w = width.min(area.width);
    let h = height
        .min(area.height.saturating_sub(reserved_bottom))
        .max(3);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h + reserved_bottom);
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Ctrl+C / Ctrl+D quits from anywhere — explicit quit always wins,
/// even inside permission modals, flows, or autocomplete.
fn is_quit_key(key: &KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    matches!(
        key.code,
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('d') | KeyCode::Char('D')
    )
}

/// What Esc does depends on UI state. Esc never quits and never answers
/// a permission modal (risky actions must be answered explicitly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscAction {
    CloseCompleter,
    CloseFlow,
    Ignore,
    AbortTurn,
}

/// How many submitted prompts are kept for recall. Bounded so a long-lived
/// session cannot grow the history file without limit.
const MAX_PROMPT_HISTORY: usize = 500;

/// Prompt history file: one line per entry, oldest first. Plain text so it
/// stays greppable, and session logs never live here (those may hold secrets).
fn history_path() -> std::path::PathBuf {
    let home = std::env::var("TURYA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.turya")
    });
    std::path::PathBuf::from(home).join("history")
}

/// Load recall history, newest last. A missing or unreadable file is not an
/// error: recall simply starts empty.
fn load_history() -> Vec<String> {
    let path = history_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if out.len() > MAX_PROMPT_HISTORY {
        let excess = out.len() - MAX_PROMPT_HISTORY;
        out.drain(0..excess);
    }
    out
}

/// Append one prompt to the history file, best-effort and bounded. Rewriting
/// the tail keeps the file capped without unbounded growth.
fn persist_history(history: &[String]) {
    if history.is_empty() {
        return;
    }
    let path = history_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let start = history.len().saturating_sub(MAX_PROMPT_HISTORY);
    let body: String = history[start..].iter().map(|l| format!("{l}\n")).collect();
    let _ = std::fs::write(path, body);
}

/// One retained tool output for on-demand expansion (`Ctrl+E`).
#[derive(Debug, Clone)]
struct StoredOutput {
    tool: String,
    /// Full output text, bounded at store time.
    full: String,
    expanded: bool,
}

/// Bound the retained full text: expansion is for reading, not paging
/// megabytes through the transcript.
const MAX_STORED_OUTPUT_CHARS: usize = 4000;
/// Bound the retained entries: old outputs age out, newest survive.
const MAX_OUTPUT_ENTRIES: usize = 20;
/// One transcript row: text plus its visual voice.
#[derive(Debug, Clone)]
struct TLine {
    text: String,
    /// System toasts and separators: dimmed so content stands out.
    dim: bool,
}

/// Render a user prompt into the transcript (pure).
fn format_user_message(prompt: &str) -> String {
    format!("\n👤 {prompt}\n")
}

/// Truncate to `max` bytes on a char boundary (pure).
fn truncate_preview(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [+{} chars]", &s[..end], s.len() - end)
}

/// Render a tool result as log lines: summary + bounded output preview.
/// Without this, successful calls look like nothing happened.
fn format_tool_result(tool_name: &str, res: &turya_protocol::ToolResult) -> Vec<String> {
    if res.success {
        let mut out = vec![format!("✔ {tool_name} (call {})", res.call_id)];
        let preview = res.output.trim();
        if !preview.is_empty() {
            out.push(format!(
                "  {}",
                truncate_preview(preview, 300).replace('\n', "\n  ")
            ));
        }
        out
    } else {
        vec![format!(
            "✘ {tool_name} (call {}) failed: {}",
            res.call_id,
            res.error.as_deref().unwrap_or("unknown error")
        )]
    }
}

pub struct TuiApp {
    input: String,
    /// Single chronological transcript: user messages, assistant tokens,
    /// tool activity, and toasts all render here (no separate tool box).
    /// Rows carry their own voice: system toasts and separators render
    /// dimmed so user/assistant/tool content stands out.
    transcript: Vec<TLine>,
    pending_permission: Option<(String, String)>, // (request_id, action)
    /// call_id → tool name, filled on Initiated, drained on Completed, so
    /// result lines can name the tool (`ToolResult` carries no name).
    pending_tools: std::collections::HashMap<String, String>,
    /// Retained full tool outputs for on-demand expansion (`Ctrl+E`).
    /// Only outputs longer than the inline preview are kept, newest last.
    tool_outputs: std::collections::VecDeque<StoredOutput>,
    /// Active provider selection: (provider, model, via). Set optimistically
    /// on switch, corrected by `ProviderState` events from the host.
    provider: Option<(String, String, String)>,
    /// Character counts for the status bar (token estimates ≈ chars/4).
    sent_chars: usize,
    recv_chars: usize,
    /// Reasoning visibility toggle (`/thinking`). Display-only for now.
    show_thinking: bool,
    /// Write recall history to disk. Off in tests so they stay hermetic.
    persist_history: bool,
    /// Submitted prompts, newest last, for Up/Down recall.
    prompt_history: Vec<String>,
    /// Where recall is walking: `None` means "on the live draft".
    history_cursor: Option<usize>,
    /// The draft saved when recall first walked away from it.
    stashed_draft: String,
    registry: SlashRegistry,
    completer: Option<Completer>,
    flow: Flow,
    /// `/auth <provider>` jumps straight into that provider's login once
    /// the provider list arrives.
    pending_auth: Option<String>,
    /// Turn lifecycle for the progress indicator: set by `TurnStarted`,
    /// cleared by `TurnCompleted`/`Error`. Drives the spinner in the top bar.
    turn_active: bool,
    /// Spinner animation frame, advanced by the 120ms tick in `run()`.
    /// A plain counter (not time-derived) so headless tests set it directly.
    spin_tick: u64,
    /// Manual scroll-back: lines the transcript is lifted above the bottom.
    /// `0` = follow live output. PageUp/PageDown adjust, End/new prompt resets.
    scroll_lines_up: usize,
}

impl Default for TuiApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiApp {
    /// In-memory app with no disk state. Tests and embedders use this; it
    /// never reads or writes the user's history file.
    pub fn new() -> Self {
        Self {
            input: String::new(),
            transcript: Vec::new(),
            pending_permission: None,
            pending_tools: std::collections::HashMap::new(),
            tool_outputs: std::collections::VecDeque::new(),
            provider: None,
            sent_chars: 0,
            recv_chars: 0,
            show_thinking: true,
            prompt_history: Vec::new(),
            history_cursor: None,
            stashed_draft: String::new(),
            persist_history: false,
            registry: SlashRegistry::with_builtins(),
            completer: None,
            flow: Flow::None,
            pending_auth: None,
            turn_active: false,
            spin_tick: 0,
            scroll_lines_up: 0,
        }
    }

    /// Append one line to the transcript (the single home for chat, tool
    /// activity, and toasts — there is no separate tool box).
    fn log_line(&mut self, line: String) {
        self.transcript.push(TLine {
            text: line,
            dim: false,
        });
    }

    /// Append a dimmed system toast (routing notices, usage hints).
    /// Warnings, errors, and confirmations stay full-bright.
    fn log_dim(&mut self, line: String) {
        self.transcript.push(TLine {
            text: line,
            dim: true,
        });
    }

    /// Append a (possibly multi-line) block, preserving blank lines so
    /// rendering matches the old plain-string transcript exactly.
    fn push_block(&mut self, text: &str, dim: bool) {
        for line in text.split('\n') {
            self.transcript.push(TLine {
                text: line.to_string(),
                dim,
            });
        }
    }

    /// Stream one token chunk: extend the current content line, or start a
    /// new one when the transcript is empty or ends in a dimmed row.
    fn push_text(&mut self, chunk: &str) {
        match self.transcript.last_mut() {
            Some(last) if !last.dim => last.text.push_str(chunk),
            _ => self.transcript.push(TLine {
                text: chunk.to_string(),
                dim: false,
            }),
        }
    }

    /// Plain-text view of the transcript: test assertions and scroll math.
    /// Styles live only in `transcript_lines()` at render time.
    pub fn transcript_text(&self) -> String {
        self.transcript
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Styled rows for the chat widget.
    fn transcript_lines(&self) -> Vec<Line<'_>> {
        self.transcript
            .iter()
            .map(|l| {
                let style = if l.dim {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                Line::styled(l.text.as_str(), style)
            })
            .collect()
    }

    /// Clear all transcript rows (`/clear`).
    fn clear_transcript(&mut self) {
        self.transcript.clear();
    }

    // ---- prompt history (readline semantics) ----

    /// Record a submitted prompt. Consecutive duplicates are collapsed so
    /// hammering Enter does not fill the ring with the same line.
    fn push_history(&mut self, prompt: &str) {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return;
        }
        if self.prompt_history.last().map(String::as_str) != Some(prompt) {
            self.prompt_history.push(prompt.to_string());
        }
        if self.prompt_history.len() > MAX_PROMPT_HISTORY {
            let excess = self.prompt_history.len() - MAX_PROMPT_HISTORY;
            self.prompt_history.drain(0..excess);
        }
        self.history_cursor = None;
        self.stashed_draft.clear();
        if self.persist_history {
            persist_history(&self.prompt_history);
        }
    }

    /// The real application constructor: restores the recall history file
    /// and keeps writing to it. `new()` stays hermetic for tests.
    pub fn new_restoring() -> Self {
        let mut app = Self::new();
        app.prompt_history = load_history();
        app.persist_history = true;
        app
    }

    /// Do the input box's arrow keys, or does an open flow/completer own them?
    fn input_owns_arrows(&self) -> bool {
        self.completer.is_none() && matches!(self.flow, Flow::None)
    }

    /// Any manual edit abandons the recall cursor, so the next Up starts from
    /// the newest entry instead of resuming a stale walk.
    fn on_input_edited(&mut self) {
        self.history_cursor = None;
    }

    /// One step older. The first press stashes the in-progress draft so
    /// walking back to the newest entry restores it.
    fn recall_older(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        let idx = match self.history_cursor {
            None => {
                self.stashed_draft = std::mem::take(&mut self.input);
                self.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(idx);
        self.input = self.prompt_history[idx].clone();
    }

    /// One step newer; past the newest entry the stashed draft comes back.
    fn recall_newer(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.prompt_history.len() {
            let next = idx + 1;
            self.history_cursor = Some(next);
            self.input = self.prompt_history[next].clone();
        } else {
            self.history_cursor = None;
            self.input = std::mem::take(&mut self.stashed_draft);
        }
    }

    /// Replay a stored transcript into the view (`turya resume`).
    ///
    /// Renders the same rows a live turn would have produced, so a resumed
    /// session is indistinguishable from one that never quit. Tool rows keep
    /// the collapsed head + `[+N more]` preview and are expandable with
    /// `Ctrl+E` exactly like live ones.
    pub fn load_transcript(&mut self, transcript: &Transcript) {
        self.clear_transcript();
        for turn in &transcript.turns {
            for part in &turn.parts {
                match part {
                    Part::Text { text } => {
                        self.push_block(text, false);
                    }
                    Part::UserText { text } => {
                        self.push_block(&format_user_message(text), false);
                    }
                    Part::Reasoning { .. } | Part::Instruction { .. } => {
                        // Harness/model scaffolding, not user-facing prose.
                    }
                    Part::ToolCall {
                        call_id,
                        tool_name,
                        arguments,
                        ..
                    } => {
                        self.log_dim(format!("▸ {tool_name} {arguments}"));
                        self.pending_tools
                            .insert(call_id.clone(), tool_name.clone());
                    }
                    Part::ToolResult {
                        call_id,
                        output,
                        truncated,
                    } => {
                        let name = self
                            .pending_tools
                            .remove(call_id)
                            .unwrap_or_else(|| "tool".to_string());
                        let res = turya_protocol::ToolResult {
                            call_id: call_id.clone(),
                            success: !output.contains("success=false"),
                            output: output.clone(),
                            error: None,
                        };
                        for line in format_tool_result(&name, &res) {
                            self.log_line(line);
                        }
                        if *truncated {
                            self.retain_output(&name, &res);
                        }
                    }
                    Part::Attachment(a) | Part::Image(a) => {
                        self.log_dim(format!("📎 {} ({})", a.path.display(), a.mime));
                    }
                }
            }
        }
        self.scroll_to_bottom();
    }

    /// Retain a tool's full output when it exceeds the inline preview,
    /// so `Ctrl+E` can expand it later. Failures keep their error text.
    fn retain_output(&mut self, tool_name: &str, res: &turya_protocol::ToolResult) {
        let full = if res.success {
            res.output.clone()
        } else {
            res.error.clone().unwrap_or_default()
        };
        // Same bound the preview uses: at most the preview is shown inline.
        if full.trim().len() <= 300 {
            return;
        }
        let mut text: String = full.chars().take(MAX_STORED_OUTPUT_CHARS).collect();
        if full.chars().count() > MAX_STORED_OUTPUT_CHARS {
            text.push_str("…[stored truncated]");
        }
        self.tool_outputs.push_back(StoredOutput {
            tool: tool_name.to_string(),
            full: text,
            expanded: false,
        });
        while self.tool_outputs.len() > MAX_OUTPUT_ENTRIES {
            self.tool_outputs.pop_front();
        }
    }

    /// Expand the most recent unexpanded output into the transcript
    /// (`Ctrl+E`). Each entry expands once; afterwards there is nothing
    /// left to show and the user is told so.
    fn expand_last_output(&mut self) {
        let next = self
            .tool_outputs
            .iter_mut()
            .rev()
            .find(|e| !e.expanded)
            .map(|e| {
                e.expanded = true;
                (e.tool.clone(), e.full.clone())
            });
        match next {
            Some((tool, full)) => {
                self.log_dim(format!("── full output: {tool} ──"));
                self.push_block(&full, false);
            }
            None => self.log_dim("ℹ no truncated output to expand".to_string()),
        }
    }

    /// Approximate wrapped line count for scroll math (no new deps):
    /// each logical line occupies ceil(chars/width) rows, minimum 1.
    /// Close enough to `Wrap { trim: false }` that the tail-stays-visible
    /// test holds; exactness is not required, clamping is.
    fn wrapped_lines(text: &str, width: usize) -> usize {
        let w = width.max(1);
        text.split('\n')
            .map(|l| {
                let n = l.chars().count();
                if n == 0 {
                    1
                } else {
                    n.div_ceil(w)
                }
            })
            .sum()
    }

    /// Lift the transcript viewport up (read back history).
    fn scroll_up(&mut self) {
        self.scroll_lines_up = self.scroll_lines_up.saturating_add(10);
    }

    /// Lower the viewport toward live output.
    fn scroll_down(&mut self) {
        self.scroll_lines_up = self.scroll_lines_up.saturating_sub(10);
    }

    /// Pin the viewport back to the live bottom.
    fn scroll_to_bottom(&mut self) {
        self.scroll_lines_up = 0;
    }

    /// Format a character count as estimated tokens (≈ chars/4).
    fn fmt_tokens(chars: usize) -> String {
        let t = chars / 4;
        if t >= 1000 {
            format!("{:.1}k", t as f64 / 1000.0)
        } else {
            t.to_string()
        }
    }

    /// One-row top bar: identity when idle, live state when working.
    /// The spinner sits FIRST when active (the layout shift pulls the eye);
    /// least-important info stays rightmost (narrow screens clip the right).
    fn top_line(&self) -> String {
        let sel = self
            .provider
            .as_ref()
            .map(|(p, m, via)| format!("{p}/{m} ({via})"))
            .unwrap_or_else(|| "no provider".to_string());
        if self.turn_active {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let f = frames[(self.spin_tick as usize) % frames.len()];
            // Deterministic pick (lowest tool name) so the line is stable
            // when several tools run at once.
            let mut tools: Vec<&String> = self.pending_tools.values().collect();
            tools.sort();
            match tools.first() {
                Some(tool) => format!("{f} {tool}… │ {sel}"),
                None => format!("{f} working · {sel}"),
            }
        } else {
            format!(" Turya v{} │ {sel}", env!("CARGO_PKG_VERSION"))
        }
    }

    /// One-line status bar: token estimates, thinking flag, mode.
    /// The `Esc stop · Ctrl+C quit` hint lives in the input-box title, so it
    /// is not duplicated here (frees ~22 cols on narrow screens).
    /// Token counts are char-based estimates (≈), clearly marked — true
    /// provider usage blocks are a follow-up.
    fn status_line(&self) -> String {
        format!(
            " ↑{} ↓{}≈tok │ think:{} │ Build · Review-for-me",
            Self::fmt_tokens(self.sent_chars),
            Self::fmt_tokens(self.recv_chars),
            if self.show_thinking { "on" } else { "off" },
        )
    }

    /// Submit a prompt: echo it into the transcript (so your messages are
    /// visible) and forward it to the engine. A new turn starts at the
    /// live bottom, releasing any scroll-back lock.
    async fn submit_prompt(&mut self, prompt: String, cmd_tx: &mpsc::Sender<TuryaCommand>) {
        self.sent_chars += prompt.len();
        self.scroll_to_bottom();
        // Recall before the prompt is moved out, so the draft is kept.
        self.push_history(&prompt);
        self.push_block(&format_user_message(&prompt), false);
        let _ = cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt,
                mode: AgentMode::Build,
                attachments: Vec::new(),
            })
            .await;
    }

    fn esc_action(&self) -> EscAction {
        if self.completer.is_some() {
            EscAction::CloseCompleter
        } else if !matches!(self.flow, Flow::None) {
            EscAction::CloseFlow
        } else if self.pending_permission.is_some() {
            EscAction::Ignore
        } else {
            EscAction::AbortTurn
        }
    }

    /// Dispatch a completed slash command. `Local` runs inline; `EngineFlow`
    /// commands open client flows and emit protocol messages (handled by the
    /// host router — the TUI never touches auth or providers directly).
    async fn dispatch_slash(
        &mut self,
        name: &str,
        args: &str,
        cmd_tx: &mpsc::Sender<TuryaCommand>,
    ) {
        match self.registry.get(name).map(|c| c.kind) {
            Some(slash::CommandKind::Local) => match name {
                "help" => {
                    let mut text = String::from("Commands:\n");
                    for c in self.registry.filter("") {
                        text.push_str(&format!("  /{} — {}\n", c.name, c.description));
                    }
                    self.push_block(&text, false);
                }
                "clear" => {
                    self.clear_transcript();
                }
                "thinking" => {
                    self.show_thinking = !self.show_thinking;
                    self.log_dim(format!(
                        "ℹ reasoning display {}",
                        if self.show_thinking { "on" } else { "off" }
                    ));
                }
                "compact" => {
                    // `/compact [focus...]`: summarise older turns. The focus
                    // text rides along as the summariser's instruction, which
                    // is more useful than our own guess at what matters.
                    let focus = args.trim();
                    let _ = cmd_tx
                        .send(TuryaCommand::Compact {
                            focus: (!focus.is_empty()).then(|| focus.to_string()),
                        })
                        .await;
                }
                "context" => {
                    let _ = cmd_tx.send(TuryaCommand::ContextReport).await;
                }
                "sessions" => {
                    let id = args.trim();
                    if id.is_empty() {
                        let _ = cmd_tx
                            .send(TuryaCommand::ListSessions {
                                cwd: None,
                                limit: Some(20),
                            })
                            .await;
                    } else {
                        let _ = cmd_tx
                            .send(TuryaCommand::ResumeSession { id: id.to_string() })
                            .await;
                    }
                }
                "steps" => {
                    // `/steps [model_calls] [tool_calls]`: per-turn budgets.
                    // Bare `/steps` reports the convention (engine owns truth;
                    // 8/32 are the shipped defaults).
                    let parts: Vec<&str> = args.split_whitespace().collect();
                    let parse_steps = |s: &str| s.parse::<usize>().ok().filter(|&n| n > 0);
                    let parse_tools = |s: &str| s.parse::<u32>().ok().filter(|&n| n > 0);
                    match parts.as_slice() {
                        [] => self.log_dim(
                            "ℹ usage: /steps [model_calls] [tool_calls] (defaults 8 32)"
                                .to_string(),
                        ),
                        [m] => match parse_steps(m) {
                            Some(mc) => {
                                self.log_dim(format!("→ budgets set: {mc} model calls per turn…"));
                                let _ = cmd_tx
                                    .send(TuryaCommand::UpdateConfig {
                                        permission_mode: None,
                                        provider: None,
                                        model: None,
                                        max_steps: Some(mc),
                                        max_tool_calls: None,
                                    })
                                    .await;
                            }
                            None => self.log_dim(
                                "ℹ usage: /steps [model_calls] [tool_calls] (positive integers)"
                                    .to_string(),
                            ),
                        },
                        [m, t] => match (parse_steps(m), parse_tools(t)) {
                            (Some(mc), Some(tc)) => {
                                self.log_dim(format!(
                                    "→ budgets set: {mc} model calls, {tc} tool calls per turn…"
                                ));
                                let _ = cmd_tx
                                    .send(TuryaCommand::UpdateConfig {
                                        permission_mode: None,
                                        provider: None,
                                        model: None,
                                        max_steps: Some(mc),
                                        max_tool_calls: Some(tc),
                                    })
                                    .await;
                            }
                            _ => self.log_dim(
                                "ℹ usage: /steps [model_calls] [tool_calls] (positive integers)"
                                    .to_string(),
                            ),
                        },
                        _ => self.log_line(
                            "ℹ usage: /steps [model_calls] [tool_calls] (positive integers)"
                                .to_string(),
                        ),
                    }
                }
                _ => {
                    self.log_dim(format!("ℹ /{name} is coming soon"));
                }
            },
            _ => match name {
                "models" => {
                    self.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
                    let _ = cmd_tx.send(TuryaCommand::ListProviders).await;
                }
                "auth" => {
                    if args.trim().is_empty() {
                        self.flow = Flow::Browser(BrowserFlow::new(BrowserMode::AuthPick));
                        let _ = cmd_tx.send(TuryaCommand::ListProviders).await;
                    } else {
                        self.pending_auth = Some(args.trim().to_string());
                        self.flow = Flow::Browser(BrowserFlow::new(BrowserMode::AuthPick));
                        let _ = cmd_tx.send(TuryaCommand::ListProviders).await;
                    }
                }
                _ => {
                    self.log_dim(format!("ℹ unknown command /{name}"));
                }
            },
        }
    }

    /// Route one key into the open flow. Returns after handling; the input
    /// buffer is untouched while a flow owns the keyboard.
    async fn handle_flow_key(&mut self, code: KeyCode, cmd_tx: &mpsc::Sender<TuryaCommand>) {
        // Esc closes any flow (cancelling a live server flow first) —
        // unless the browser has filter text, which Esc clears first.
        if code == KeyCode::Esc {
            if let Flow::Browser(b) = &mut self.flow {
                if !b.query.is_empty() {
                    b.set_query(String::new());
                    return;
                }
            }
            let flow_id = match &self.flow {
                Flow::Auth(a) => a.flow_id.clone(),
                _ => None,
            };
            if let Some(flow_id) = flow_id {
                let _ = cmd_tx.send(TuryaCommand::CancelAuthFlow { flow_id }).await;
            }
            self.flow = Flow::None;
            return;
        }

        enum Act {
            Nothing,
            Send(TuryaCommand),
            Close,
            ToAuth(AuthFlow),
        }
        let mut act = Act::Nothing;
        match &mut self.flow {
            Flow::None => {}
            Flow::Browser(b) => match code {
                KeyCode::Up => {
                    if b.right {
                        b.move_model(-1);
                    } else {
                        b.move_prov(-1);
                    }
                }
                KeyCode::Down => {
                    if b.right {
                        b.move_model(1);
                    } else {
                        b.move_prov(1);
                    }
                }
                KeyCode::Tab => {
                    b.right = !b.right;
                }
                // Type-to-filter (Esc clears it); Backspace edits it.
                // An empty query after Backspace keeps the flow open.
                KeyCode::Char(c) if !c.is_control() => {
                    let mut q = b.query.clone();
                    q.push(c);
                    b.set_query(q);
                }
                KeyCode::Backspace => {
                    let mut q = b.query.clone();
                    q.pop();
                    b.set_query(q);
                }
                KeyCode::Enter => match b.mode {
                    BrowserMode::Models => {
                        let current = b.current().cloned();
                        match current {
                            Some(p) if p.is_locked() => {
                                act = Act::ToAuth(AuthFlow::new(
                                    &p.id,
                                    &p.display_name,
                                    p.oauth != "unsupported",
                                ));
                            }
                            Some(_) if !b.right => {
                                b.right = true;
                            }
                            _ => {
                                if let Some((pid, mid)) = b.selected_model() {
                                    // Pending toast: failures arrive as Error events.
                                    // Optimistic label too, corrected by ProviderState.
                                    self.provider =
                                        Some((pid.clone(), mid.clone(), "…".to_string()));
                                    self.log_dim(format!("→ switching to {pid}/{mid}…"));
                                    self.flow = Flow::None;
                                    let _ = cmd_tx
                                        .send(TuryaCommand::UpdateConfig {
                                            permission_mode: None,
                                            provider: Some(pid),
                                            model: Some(mid),
                                            max_steps: None,
                                            max_tool_calls: None,
                                        })
                                        .await;
                                    return;
                                }
                            }
                        }
                    }
                    BrowserMode::AuthPick => {
                        if let Some(p) = b.current().cloned() {
                            act = Act::ToAuth(AuthFlow::new(
                                &p.id,
                                &p.display_name,
                                p.oauth != "unsupported",
                            ));
                        }
                    }
                },
                _ => {}
            },
            Flow::Auth(a) => match code {
                KeyCode::Up | KeyCode::Down => {
                    let len = a.methods().len();
                    if let AuthStage::MethodPick { sel } = &mut a.stage {
                        let delta = if code == KeyCode::Up { -1 } else { 1 };
                        *sel = (*sel as isize + delta).rem_euclid(len as isize) as usize;
                    }
                }
                KeyCode::Enter => match &a.stage {
                    AuthStage::MethodPick { sel } => {
                        let method = a.methods().get(*sel).map(|(m, _)| m.to_string());
                        if let Some(method) = method {
                            act = Act::Send(TuryaCommand::BeginAuthFlow {
                                provider: a.provider.clone(),
                                method,
                            });
                        }
                    }
                    AuthStage::KeyPrompt { buffer } | AuthStage::OAuthWait { buffer, .. } => {
                        if let Some(flow_id) = a.flow_id.clone() {
                            act = Act::Send(TuryaCommand::SubmitAuthInput {
                                flow_id,
                                payload: buffer.clone(),
                            });
                        }
                    }
                    AuthStage::Failed(_) => {
                        a.stage = AuthStage::MethodPick { sel: 0 };
                    }
                    AuthStage::Done(_) => {
                        act = Act::Close;
                    }
                },
                KeyCode::Char(c) => match &mut a.stage {
                    AuthStage::KeyPrompt { buffer } | AuthStage::OAuthWait { buffer, .. } => {
                        buffer.push(c);
                    }
                    _ => {}
                },
                KeyCode::Backspace => match &mut a.stage {
                    AuthStage::KeyPrompt { buffer } | AuthStage::OAuthWait { buffer, .. } => {
                        buffer.pop();
                    }
                    _ => {}
                },
                _ => {}
            },
        }
        match act {
            Act::Nothing => {}
            Act::Send(cmd) => {
                let _ = cmd_tx.send(cmd).await;
            }
            Act::Close => {
                self.flow = Flow::None;
            }
            Act::ToAuth(auth) => {
                let provider = auth.provider.clone();
                self.flow = Flow::Auth(auth);
                let _ = cmd_tx.send(TuryaCommand::GetAuthStatus { provider }).await;
            }
        }
    }

    /// Feed one protocol event into UI state. Single home for ALL event
    /// handling (transcript, tools, permissions, flows, provider label),
    /// so headless tests drive exactly what the live loop drives. Public so
    /// an embedding host (and the live model tests) can drive a session
    /// without a terminal.
    pub fn feed_flow_event(&mut self, evt: &TuryaEvent) {
        match evt {
            TuryaEvent::ProvidersListed { providers } => {
                let views: Vec<flows::ProviderView> = providers
                    .iter()
                    .map(|p| flows::ProviderView {
                        id: p.id.clone(),
                        display_name: p.display_name.clone(),
                        api_key: p.api_key.clone(),
                        oauth: p.oauth.clone(),
                        models: p
                            .models
                            .iter()
                            .map(|m| flows::ModelView {
                                id: m.id.clone(),
                                display_name: m.display_name.clone(),
                                source: m.source.clone(),
                            })
                            .collect(),
                    })
                    .collect();
                // Direct `/auth <provider>` jump once the list arrives.
                if let Some(target) = self.pending_auth.take() {
                    if let Some(p) = views.iter().find(|v| {
                        v.id == target || v.display_name.to_lowercase() == target.to_lowercase()
                    }) {
                        self.flow = Flow::Auth(AuthFlow::new(
                            &p.id,
                            &p.display_name,
                            p.oauth != "unsupported",
                        ));
                        return;
                    }
                    self.log_dim(format!("ℹ unknown provider '{target}'"));
                }
                if let Flow::Browser(ref mut b) = self.flow {
                    b.set_providers(views);
                }
            }
            TuryaEvent::AuthStatusChanged {
                provider,
                api_key,
                oauth,
            } => {
                if let Flow::Browser(ref mut b) = self.flow {
                    if let Some(p) = b.providers.iter_mut().find(|v| v.id == *provider) {
                        p.api_key = api_key.clone();
                        p.oauth = oauth.clone();
                    }
                }
            }
            TuryaEvent::AuthFlowStarted { flow_id, action } => {
                if let Flow::Auth(ref mut a) = self.flow {
                    a.flow_id = Some(flow_id.clone());
                    match action {
                        turya_protocol::AuthAction::PromptMasked { .. } => {
                            a.stage = AuthStage::KeyPrompt {
                                buffer: String::new(),
                            };
                        }
                        turya_protocol::AuthAction::OpenBrowser { url } => {
                            a.stage = AuthStage::OAuthWait {
                                url: url.clone(),
                                buffer: String::new(),
                            };
                        }
                    }
                }
            }
            TuryaEvent::AuthFlowCompleted {
                provider, method, ..
            } => {
                if let Flow::Auth(ref mut a) = self.flow {
                    a.stage = AuthStage::Done(format!(
                        "✔ {provider} connected via {method}. Back to /models."
                    ));
                }
                self.log_line(format!("✔ {provider} connected via {method}"));
            }
            TuryaEvent::AuthFlowFailed { reason, .. } => {
                if let Flow::Auth(ref mut a) = self.flow {
                    a.stage = AuthStage::Failed(reason.clone());
                } else {
                    self.log_line(format!("⚠ auth failed: {reason}"));
                }
            }
            TuryaEvent::CatalogUpdated { .. } => {
                // Model data changed: refresh an open browser.
                if matches!(self.flow, Flow::Browser(_)) {
                    // Re-query through the draw path is async; the host
                    // re-emits ProvidersListed on the next ListProviders, so
                    // the flow stays consistent without extra plumbing here.
                }
            }
            TuryaEvent::ProviderState {
                provider,
                model,
                via,
            } => {
                // Host truth corrects the optimistic switch label. Handled
                // here (not in the run loop) so headless tests drive it.
                self.provider = Some((provider.clone(), model.clone(), via.clone()));
            }
            TuryaEvent::TokenDelta { chunk } => {
                self.recv_chars += chunk.len();
                self.push_text(chunk);
            }
            TuryaEvent::ToolCallInitiated(call) => {
                self.log_line(format!("⚡ {}", call.tool_name));
                self.pending_tools
                    .insert(call.call_id.clone(), call.tool_name.clone());
            }
            TuryaEvent::ToolCallCompleted(res) => {
                let name = self
                    .pending_tools
                    .remove(&res.call_id)
                    .unwrap_or_else(|| "tool".to_string());
                for line in format_tool_result(&name, res) {
                    self.log_line(line);
                }
                self.retain_output(&name, res);
            }
            TuryaEvent::PermissionRequested {
                request_id, action, ..
            } => {
                self.pending_permission = Some((request_id.clone(), action.clone()));
            }
            TuryaEvent::CompactionStarted { turns } => {
                // Reuse the live spinner: a compaction is real work and the
                // user must not watch a frozen screen.
                self.turn_active = true;
                self.log_dim(format!("── compacting {turns} turns… ──"));
            }
            TuryaEvent::CompactionCompleted {
                before_turns,
                after_turns,
                summary,
            } => {
                self.turn_active = false;
                self.log_dim(format!(
                    "── compacted {before_turns} turns → {after_turns} (full history still searchable) ──"
                ));
                self.push_block(summary.trim(), false);
            }
            TuryaEvent::SessionsListed { sessions } => {
                if sessions.is_empty() {
                    self.log_dim("ℹ no stored sessions yet".to_string());
                } else {
                    for s in sessions {
                        let mark = if s.repaired { " *" } else { "" };
                        self.log_dim(format!(
                            "{} · {} turn(s) · {}{mark}   (/sessions {} to resume)",
                            s.id, s.seq, s.updated_at, s.title
                        ));
                    }
                }
            }
            TuryaEvent::SessionResumed {
                session,
                transcript,
            } => {
                self.log_dim(format!("── resumed {} ──", session.id));
                self.load_transcript(transcript);
            }
            TuryaEvent::TurnStarted { .. } => {
                // Progress indicator on: spinner runs until the turn settles.
                // Scroll lock resets — a new turn starts at the live bottom.
                self.turn_active = true;
                self.spin_tick = 0;
                self.scroll_lines_up = 0;
            }
            TuryaEvent::TurnCompleted { .. } => {
                self.turn_active = false;
                self.log_dim("────────────────────────────────────────".to_string());
            }
            TuryaEvent::Error { message } => {
                self.turn_active = false;
                self.log_line(format!("⚠ {message}"));
            }
            _ => {}
        }
    }
    /// Input pane height: grows with the draft (Alt+Enter newlines) so
    /// every line stays visible, capped so the transcript keeps its rows.
    /// Includes the 2 border rows.
    fn input_height(&self) -> u16 {
        (self.input.lines().count().max(1) as u16 + 2).min(6)
    }

    /// Render one full frame. Split out of `run()` so headless tests can
    /// drive the REAL draw path with ratatui's `TestBackend`.
    fn render(&self, f: &mut Frame) {
        let input_h = self.input_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),       // Top bar: identity + live spinner
                Constraint::Min(5),          // Chat transcript (tools inline)
                Constraint::Length(input_h), // Input Box / Permission Prompt
                Constraint::Length(1),       // Status bar (no borders: 1 row)
            ])
            .split(f.area());

        // 1. Top bar (borderless): static identity when idle, spinner plus
        // live state while a turn runs.
        f.render_widget(
            Paragraph::new(self.top_line()).style(Style::default().fg(Color::Cyan)),
            chunks[0],
        );

        // 2. Chat transcript (user messages, assistant tokens, tool
        // activity, and toasts share one chronological stream), pinned to
        // the bottom unless the user scrolled back (see scroll_lines_up).
        let inner_w = chunks[1].width.saturating_sub(2).max(1) as usize;
        let inner_h = chunks[1].height.saturating_sub(2) as usize;
        let total = Self::wrapped_lines(&self.transcript_text(), inner_w);
        let max_off = total.saturating_sub(inner_h).min(u16::MAX as usize);
        // Follow-bottom by default: the viewport sits max_off down, lifted
        // toward the top by the scroll-back lock.
        let off = max_off.saturating_sub(self.scroll_lines_up) as u16;
        let chat = Paragraph::new(self.transcript_lines())
            .wrap(Wrap { trim: false })
            .scroll((off, 0))
            .block(Block::default().borders(Borders::ALL).title("Assistant"));
        f.render_widget(chat, chunks[1]);

        // 4. Input or Permission Prompt
        if let Some((_, ref action)) = self.pending_permission {
            let prompt = Paragraph::new(format!(
                "Allow '{}'? Press [y] to allow, [n] to deny",
                action
            ))
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Permission Required"),
            );
            f.render_widget(prompt, chunks[2]);
        } else {
            let input_widget = Paragraph::new(self.input.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Prompt (Enter send · Alt+Enter newline · Ctrl+E expand · Esc stop)"),
            );
            f.render_widget(input_widget, chunks[2]);
        }

        // 5. Status bar: single borderless row — provider/model, token
        // estimates (≈), thinking flag. Every row earns its place.
        f.render_widget(
            Paragraph::new(self.status_line()).style(Style::default().fg(Color::DarkGray)),
            chunks[3],
        );

        // 6. Slash autocomplete popup (overlay above the input pane).
        if let Some(ref comp) = self.completer {
            let matches = comp.matches(&self.input, &self.registry);
            let rows: Vec<Line> = slash::popup_rows(&matches, comp.selected)
                .into_iter()
                .map(Line::from)
                .collect();
            if !rows.is_empty() {
                let area =
                    centered_popup(f.area(), 60, (rows.len() as u16 + 2).min(9), input_h + 1);
                let popup = Paragraph::new(rows)
                    .block(Block::default().borders(Borders::ALL).title("Commands"));
                f.render_widget(ratatui::widgets::Clear, area);
                f.render_widget(popup, area);
            }
        }

        // 7. Flow overlay (/models browser, /auth stages).
        match &self.flow {
            Flow::None => {}
            Flow::Browser(b) => {
                let (left, right) = flows::render_browser(b);
                let height = (left.len().max(right.len()) as u16 + 2).min(16);
                let area = centered_popup(f.area(), 72, height, input_h + 1);
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(area);
                f.render_widget(ratatui::widgets::Clear, area);
                let left_title = match b.mode {
                    BrowserMode::Models => "Providers — type to filter",
                    BrowserMode::AuthPick => "Providers — type to filter, Enter to log in",
                };
                let left_widget =
                    Paragraph::new(left.into_iter().map(Line::from).collect::<Vec<_>>())
                        .block(Block::default().borders(Borders::ALL).title(left_title));
                let right_title = b
                    .current()
                    .map(|p| format!("Models: {}", p.display_name))
                    .unwrap_or_else(|| "Models".to_string());
                let right_widget =
                    Paragraph::new(right.into_iter().map(Line::from).collect::<Vec<_>>())
                        .block(Block::default().borders(Borders::ALL).title(right_title));
                f.render_widget(left_widget, cols[0]);
                f.render_widget(right_widget, cols[1]);
            }
            Flow::Auth(a) => {
                let rows: Vec<Line> = flows::render_auth(a).into_iter().map(Line::from).collect();
                let area =
                    centered_popup(f.area(), 70, (rows.len() as u16 + 2).min(14), input_h + 1);
                let popup = Paragraph::new(rows).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Auth")
                        .style(Style::default().fg(Color::Yellow)),
                );
                f.render_widget(ratatui::widgets::Clear, area);
                f.render_widget(popup, area);
            }
        }
    }

    pub async fn run(
        mut self,
        cmd_tx: mpsc::Sender<TuryaCommand>,
        mut event_rx: mpsc::Receiver<TuryaEvent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut reader = EventStream::new();
        // Spinner clock: the loop redraws every iteration, so advancing the
        // frame here animates the top bar (~8fps) with negligible cost.
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(120));

        // Ask the host for the current selection so the status bar shows
        // truth from the first frame (the host answers with ProviderState).
        let _ = cmd_tx.send(TuryaCommand::GetProviderState).await;

        loop {
            terminal.draw(|f| {
                self.render(f);
            })?;

            tokio::select! {
                Some(Ok(event)) = reader.next() => {
                    if let Event::Key(key) = event {
                        // Explicit quit wins everywhere (modal, flow, completer).
                        if is_quit_key(&key) {
                            break;
                        }
                        // Esc never quits: it unwinds UI state, then stops
                        // the running turn. Permission modals must be answered.
                        if key.code == KeyCode::Esc {
                            match self.esc_action() {
                                EscAction::CloseCompleter => {
                                    self.completer = None;
                                    continue;
                                }
                                EscAction::CloseFlow => {
                                    self.handle_flow_key(KeyCode::Esc, &cmd_tx).await;
                                    continue;
                                }
                                EscAction::Ignore => continue,
                                EscAction::AbortTurn => {
                                    let _ = cmd_tx.send(TuryaCommand::AbortTurn).await;
                                    continue;
                                }
                            }
                        }
                        if let Some((req_id, _)) = self.pending_permission.take() {
                            match key.code {
                                KeyCode::Char('y') => {
                                    let _ = cmd_tx.send(TuryaCommand::ResolvePermission {
                                        request_id: req_id,
                                        decision: PermissionDecision::AllowOnce,
                                    }).await;
                                }
                                KeyCode::Char('n') => {
                                    let _ = cmd_tx.send(TuryaCommand::ResolvePermission {
                                        request_id: req_id,
                                        decision: PermissionDecision::Deny,
                                    }).await;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        match key.code {
                            // Open flows own the keyboard (except permission modals above).
                            _ if !matches!(self.flow, Flow::None) => {
                                self.handle_flow_key(key.code, &cmd_tx).await;
                                continue;
                            }
                            // Slash-command session: route keys through the completer.
                            _ if self.completer.is_some() => {
                                let comp = self.completer.as_mut().unwrap();
                                let matches = comp.matches(&self.input, &self.registry);
                                match key.code {
                                    KeyCode::Char(c) => {
                                        self.input.push(c);
                                        comp.reset();
                                    }
                                    KeyCode::Backspace => {
                                        self.input.pop();
                                        if self.input.is_empty() {
                                            self.completer = None;
                                        } else {
                                            comp.reset();
                                        }
                                    }
                                    KeyCode::Up => comp.move_selection(-1, matches.len()),
                                    KeyCode::Down | KeyCode::Tab => {
                                        comp.move_selection(1, matches.len())
                                    }
                                    KeyCode::Enter => {
                                        // Bare "/" keeps legacy behavior: submit as prompt.
                                        if self.input.trim() == "/" {
                                            self.completer = None;
                                            let prompt = std::mem::take(&mut self.input);
                                            self.submit_prompt(prompt, &cmd_tx).await;
                                        } else if let Some((name, args)) =
                                            slash::dispatch_completion(
                                                &self.input,
                                                &matches,
                                                comp.selected,
                                            )
                                        {
                                            self.completer = None;
                                            self.input.clear();
                                            self.dispatch_slash(&name, &args, &cmd_tx).await;
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                            KeyCode::Char('/') if self.input.is_empty() => {
                                self.completer = Some(Completer::new());
                                self.input.push('/');
                            }
                            // Ctrl+E expands the last truncated tool output.
                            // Checked before the generic Char arm; quit keys
                            // (Ctrl+C/D) are handled far above, no conflict.
                            KeyCode::Char(c)
                                if (c == 'e' || c == 'E')
                                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                self.expand_last_output();
                            }
                            KeyCode::Char(c) => {
                                self.input.push(c);
                                self.on_input_edited();
                            }
                            KeyCode::Backspace => {
                                self.input.pop();
                                self.on_input_edited();
                            }
                            KeyCode::PageUp => self.scroll_up(),
                            KeyCode::PageDown => self.scroll_down(),
                            KeyCode::End => self.scroll_to_bottom(),
                            // Readline-style prompt recall, only when the
                            // input owns the arrow keys (no flow, no
                            // completer), so the browsers keep theirs.
                            KeyCode::Up if self.input_owns_arrows() => {
                                self.recall_older();
                            }
                            KeyCode::Down if self.input_owns_arrows() => {
                                self.recall_newer();
                            }
                            // Alt+Enter inserts a newline (multiline draft);
                            // most terminals deliver it as Enter+ALT.
                            KeyCode::Enter
                                if key.modifiers.contains(KeyModifiers::ALT) =>
                            {
                                self.input.push('\n');
                            }
                            KeyCode::Enter if !self.input.trim().is_empty() => {
                                let prompt = std::mem::take(&mut self.input);
                                self.submit_prompt(prompt, &cmd_tx).await;
                            }
                            _ => {}
                        }
                    }
                }
                Some(evt) = event_rx.recv() => {
                    // Single home for ALL event handling (see feed_flow_event):
                    // the live loop and headless tests drive the same code.
                    self.feed_flow_event(&evt);
                }
                _ = ticker.tick() => {
                    // Wrapping add: process uptime is not a crash reason.
                    self.spin_tick = self.spin_tick.wrapping_add(1);
                }
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turya_protocol::{AuthAction, ModelSummary, ProviderSummary};

    fn listed() -> TuryaEvent {
        TuryaEvent::ProvidersListed {
            providers: vec![
                ProviderSummary {
                    id: "gemini".to_string(),
                    display_name: "Gemini".to_string(),
                    models: vec![ModelSummary {
                        id: "gemini-2.5-flash".to_string(),
                        display_name: "Gemini 2.5 Flash".to_string(),
                        source: "static".to_string(),
                    }],
                    api_key: "env".to_string(),
                    oauth: "missing".to_string(),
                },
                ProviderSummary {
                    id: "openai".to_string(),
                    display_name: "OpenAI".to_string(),
                    models: vec![],
                    api_key: "missing".to_string(),
                    oauth: "missing".to_string(),
                },
            ],
        }
    }

    #[test]
    fn providers_listed_populates_browser() {
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        app.feed_flow_event(&listed());
        match &app.flow {
            Flow::Browser(b) => {
                assert_eq!(b.providers.len(), 2);
                assert!(!b.loading);
            }
            _ => panic!("expected browser flow"),
        }
    }

    #[test]
    fn pending_auth_jumps_to_login() {
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::AuthPick));
        app.pending_auth = Some("gemini".to_string());
        app.feed_flow_event(&listed());
        match &app.flow {
            Flow::Auth(a) => assert_eq!(a.provider, "gemini"),
            _ => panic!("expected auth flow"),
        }
        // Unknown target keeps the browser and toasts.
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::AuthPick));
        app.pending_auth = Some("nope".to_string());
        app.feed_flow_event(&listed());
        assert!(matches!(app.flow, Flow::Browser(_)));
        assert!(app.transcript_text().contains("unknown provider"));
    }

    #[test]
    fn auth_flow_started_drives_stages() {
        let mut app = TuiApp::new();
        app.flow = Flow::Auth(AuthFlow::new("gemini", "Gemini", true));
        app.feed_flow_event(&TuryaEvent::AuthFlowStarted {
            flow_id: "f1".to_string(),
            action: AuthAction::PromptMasked {
                prompt: "API key".to_string(),
            },
        });
        match &app.flow {
            Flow::Auth(a) => {
                assert_eq!(a.flow_id.as_deref(), Some("f1"));
                assert!(matches!(a.stage, AuthStage::KeyPrompt { .. }));
            }
            _ => panic!("expected auth flow"),
        }
        app.feed_flow_event(&TuryaEvent::AuthFlowCompleted {
            flow_id: "f1".to_string(),
            provider: "gemini".to_string(),
            method: "api-key".to_string(),
        });
        match &app.flow {
            Flow::Auth(a) => assert!(matches!(a.stage, AuthStage::Done(_))),
            _ => panic!("expected auth flow"),
        }
        assert!(app.transcript_text().contains("connected via api-key"));
    }

    #[test]
    fn auth_failure_without_flow_toasts() {
        let mut app = TuiApp::new();
        app.feed_flow_event(&TuryaEvent::AuthFlowFailed {
            flow_id: "f9".to_string(),
            reason: "bad code".to_string(),
        });
        assert!(app.transcript_text().contains("bad code"));
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn quit_keys_require_control() {
        use KeyCode::Char;
        assert!(is_quit_key(&key(Char('c'), KeyModifiers::CONTROL)));
        assert!(is_quit_key(&key(Char('C'), KeyModifiers::CONTROL)));
        assert!(is_quit_key(&key(Char('d'), KeyModifiers::CONTROL)));
        assert!(is_quit_key(&key(Char('D'), KeyModifiers::CONTROL)));
        // Plain typing must never quit.
        assert!(!is_quit_key(&key(Char('c'), KeyModifiers::NONE)));
        assert!(!is_quit_key(&key(Char('d'), KeyModifiers::NONE)));
        assert!(!is_quit_key(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_quit_key(&key(KeyCode::Enter, KeyModifiers::CONTROL)));
        // Shift+letter is not quit (shift is commonly held while typing).
        assert!(!is_quit_key(&key(Char('C'), KeyModifiers::SHIFT)));
    }

    #[test]
    fn esc_routing_prefers_ui_state_over_abort() {
        let mut app = TuiApp::new();
        // Plain input → abort the running turn.
        assert_eq!(app.esc_action(), EscAction::AbortTurn);
        // Open completer wins over abort.
        app.completer = Some(Completer::new());
        assert_eq!(app.esc_action(), EscAction::CloseCompleter);
        app.completer = None;
        // Open flow wins over abort.
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        assert_eq!(app.esc_action(), EscAction::CloseFlow);
        app.flow = Flow::None;
        // Permission modal: Esc must not answer, must not abort.
        app.pending_permission = Some(("req_1".to_string(), "run_bash".to_string()));
        assert_eq!(app.esc_action(), EscAction::Ignore);
    }

    #[tokio::test]
    async fn locked_model_enter_pivots_to_auth_and_queries_status() {
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        app.feed_flow_event(&listed());
        // Select the locked provider (index 1) and press Enter.
        if let Flow::Browser(ref mut b) = app.flow {
            b.sel_prov = 1;
        }
        let (tx, mut rx) = mpsc::channel(32);
        app.handle_flow_key(KeyCode::Enter, &tx).await;
        match &app.flow {
            Flow::Auth(a) => assert_eq!(a.provider, "openai"),
            _ => panic!("expected lock-pivot to auth"),
        }
        // Pivot emits a status refresh for the target.
        let cmd = rx.recv().await.expect("expected a command");
        assert!(matches!(
            cmd,
            TuryaCommand::GetAuthStatus { provider } if provider == "openai"
        ));
    }

    #[tokio::test]
    async fn browser_typing_filters_esc_clears_then_closes() {
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        app.feed_flow_event(&listed());
        let (tx, _rx) = mpsc::channel(32);
        // Type a filter.
        app.handle_flow_key(KeyCode::Char('g'), &tx).await;
        app.handle_flow_key(KeyCode::Char('e'), &tx).await;
        match &app.flow {
            Flow::Browser(b) => assert_eq!(b.query, "ge"),
            _ => panic!("expected browser flow"),
        }
        // Backspace edits the query; flow stays open.
        app.handle_flow_key(KeyCode::Backspace, &tx).await;
        match &app.flow {
            Flow::Browser(b) => assert_eq!(b.query, "g"),
            _ => panic!("expected browser flow"),
        }
        // First Esc clears the query, flow stays open.
        app.handle_flow_key(KeyCode::Esc, &tx).await;
        match &app.flow {
            Flow::Browser(b) => assert!(b.query.is_empty()),
            _ => panic!("expected browser flow"),
        }
        // Second Esc closes.
        app.handle_flow_key(KeyCode::Esc, &tx).await;
        assert!(matches!(app.flow, Flow::None));
    }

    #[tokio::test]
    async fn typing_in_model_pane_filters_models_in_place() {
        // Exact reported sequence: open /models, Tab into models, type "flash".
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        app.feed_flow_event(&listed());
        let (tx, _rx) = mpsc::channel(32);
        app.handle_flow_key(KeyCode::Tab, &tx).await;
        for c in ['f', 'l', 'a', 's', 'h'] {
            app.handle_flow_key(KeyCode::Char(c), &tx).await;
        }
        match &app.flow {
            Flow::Browser(b) => {
                assert_eq!(b.query, "flash");
                assert!(b.right, "focus must stay in the model pane");
                assert_eq!(b.visible_providers(), vec![0]);
                assert_eq!(
                    b.selected_model(),
                    Some(("gemini".to_string(), "gemini-2.5-flash".to_string()))
                );
            }
            _ => panic!("expected browser flow"),
        }
    }

    #[tokio::test]
    async fn headless_draw_shows_model_filter_results() {
        use ratatui::{backend::TestBackend, Terminal};

        // Drive the REAL draw path headlessly: open browser, Tab into
        // models, type "flash", render, and read the pixels back.
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        app.feed_flow_event(&listed());
        let (tx, _rx) = mpsc::channel(32);
        app.handle_flow_key(KeyCode::Tab, &tx).await;
        for c in ['f', 'l', 'a', 's', 'h'] {
            app.handle_flow_key(KeyCode::Char(c), &tx).await;
        }

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        // Filter header, narrowed provider, and the flash model all visible.
        assert!(text.contains("/flash"), "filter header missing:\n{text}");
        assert!(
            text.contains("type to filter"),
            "browser overlay missing:\n{text}"
        );
        assert!(
            text.contains("Gemini 2.5 Flash"),
            "filtered model missing:\n{text}"
        );
        // Non-matching provider is filtered out of the drawn overlay.
        assert!(!text.contains("OpenAI"), "stale provider shown:\n{text}");
    }

    #[tokio::test]
    async fn unlocked_model_enter_switches_and_closes() {
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(BrowserFlow::new(BrowserMode::Models));
        app.feed_flow_event(&listed());
        // Move to the model pane, then Enter on gemini-2.5-flash.
        let (tx, mut rx) = mpsc::channel(32);
        app.handle_flow_key(KeyCode::Tab, &tx).await;
        app.handle_flow_key(KeyCode::Enter, &tx).await;
        assert!(matches!(app.flow, Flow::None));
        let cmd = rx.recv().await.expect("expected a command");
        match cmd {
            TuryaCommand::UpdateConfig {
                provider, model, ..
            } => {
                assert_eq!(provider.as_deref(), Some("gemini"));
                assert_eq!(model.as_deref(), Some("gemini-2.5-flash"));
            }
            other => panic!("expected UpdateConfig, got {:?}", other),
        }
        assert!(app.transcript_text().contains("switching to"));
    }

    #[tokio::test]
    async fn submit_prompt_echoes_into_transcript() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(32);
        app.submit_prompt("understand codebase".to_string(), &tx)
            .await;
        assert!(app.transcript_text().contains("understand codebase"));
        assert!(app.transcript_text().contains("👤"));
        match rx.recv().await.expect("expected a command") {
            TuryaCommand::SubmitPrompt { prompt, .. } => {
                assert_eq!(prompt, "understand codebase")
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_lines_name_tool_and_preview_output() {
        use turya_protocol::{ToolCall, ToolResult};
        let mut app = TuiApp::new();
        // Correlate via the Initiated event, like the live loop does.
        app.feed_flow_event(&TuryaEvent::ToolCallInitiated(ToolCall {
            call_id: "g1".to_string(),
            tool_name: "run_bash".to_string(),
            parameters: serde_json::json!({}),
            signature: None,
        }));
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "g1".to_string(),
            success: true,
            output: "line1\nline2\n".to_string(),
            error: None,
        }));
        assert!(app.transcript_text().contains("✔ run_bash"));
        assert!(app.transcript_text().contains("line1"));

        // Unknown call ids still render (never silent, never panics).
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "zzz".to_string(),
            success: false,
            output: String::new(),
            error: Some("boom".to_string()),
        }));
        assert!(app.transcript_text().contains("✘ tool"));
        assert!(app.transcript_text().contains("boom"));
    }

    #[test]
    fn truncate_preview_is_char_safe_and_bounded() {
        assert_eq!(truncate_preview("short", 300), "short");
        let big = "x".repeat(1000);
        let out = truncate_preview(&big, 300);
        assert!(out.len() < 1000 && out.contains("[+"));
        // Multi-byte boundary: never splits a char.
        let emoji = "😀".repeat(100);
        let out = truncate_preview(&emoji, 10);
        assert!(out.chars().count() <= 20);
    }

    #[test]
    fn status_line_shows_provider_counters_and_thinking() {
        let mut app = TuiApp::new();
        // No provider yet.
        assert!(app.top_line().contains("no provider"));
        assert!(app.status_line().contains("think:on"));
        app.feed_flow_event(&TuryaEvent::ProviderState {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash".to_string(),
            via: "stored-key".to_string(),
        });
        // Provider selection lives in the top bar now (status bar keeps
        // counters + flags so both lines earn their cols on narrow screens).
        let top = app.top_line();
        assert!(top.contains("gemini/gemini-2.5-flash"));
        assert!(top.contains("stored-key"));
        // Counters format as estimated tokens.
        app.sent_chars = 4000;
        app.recv_chars = 8000;
        let line = app.status_line();
        assert!(line.contains("↑1.0k") && line.contains("↓2.0k"));
        // Thinking toggle flips the indicator.
        app.show_thinking = false;
        assert!(app.status_line().contains("think:off"));
    }

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(TuiApp::fmt_tokens(0), "0");
        assert_eq!(TuiApp::fmt_tokens(399), "99");
        assert_eq!(TuiApp::fmt_tokens(4000), "1.0k");
        assert_eq!(TuiApp::fmt_tokens(10400), "2.6k");
    }

    #[tokio::test]
    async fn thinking_command_toggles_flag() {
        let mut app = TuiApp::new();
        let (tx, _rx) = mpsc::channel(32);
        assert!(app.show_thinking);
        app.dispatch_slash("thinking", "", &tx).await;
        assert!(!app.show_thinking);
        assert!(app.transcript_text().contains("reasoning display off"));
        app.dispatch_slash("thinking", "", &tx).await;
        assert!(app.show_thinking);
    }

    /// Render headlessly at any size and read the pixels back as one string.
    /// The mock screen: no pty, no server, no network.
    fn rendered(app: &TuiApp, width: u16, height: u16) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn spinner_runs_while_turn_active() {
        let mut app = TuiApp::new();
        assert!(!app.top_line().contains('⠋'));
        app.feed_flow_event(&TuryaEvent::TurnStarted {
            turn_id: "t1".to_string(),
            mode: AgentMode::Build,
        });
        assert!(app.turn_active);
        // The tick counter is set directly: no sleeping on a real timer.
        app.spin_tick = 0;
        let line = app.top_line();
        assert!(
            line.contains('⠋') && line.contains("working"),
            "line: {line}"
        );
        app.spin_tick = 3;
        assert!(app.top_line().contains('⠸'));
        app.feed_flow_event(&TuryaEvent::TurnCompleted {
            turn_id: "t1".to_string(),
            success: true,
        });
        assert!(!app.turn_active);
        assert!(!app.top_line().contains('⠋'));
    }

    #[test]
    fn spinner_names_running_tool() {
        use turya_protocol::{ToolCall, ToolResult};
        let mut app = TuiApp::new();
        app.feed_flow_event(&TuryaEvent::TurnStarted {
            turn_id: "t".to_string(),
            mode: AgentMode::Build,
        });
        app.feed_flow_event(&TuryaEvent::ToolCallInitiated(ToolCall {
            call_id: "g1".to_string(),
            tool_name: "run_bash".to_string(),
            parameters: serde_json::json!({}),
            signature: None,
        }));
        assert!(app.top_line().contains("run_bash"));
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "g1".to_string(),
            success: true,
            output: "ok".to_string(),
            error: None,
        }));
        assert!(app.top_line().contains("working"));
    }

    #[test]
    fn top_line_shows_identity_when_idle() {
        let app = TuiApp::new();
        let line = app.top_line();
        assert!(
            line.contains("Turya v") && line.contains("no provider"),
            "line: {line}"
        );
    }

    #[test]
    fn tiny_terminal_renders_without_panic() {
        let mut app = TuiApp::new();
        app.feed_flow_event(&listed());
        app.feed_flow_event(&TuryaEvent::ProviderState {
            provider: "gemini".to_string(),
            model: "gemini-flash-lite-latest".to_string(),
            via: "stored-key".to_string(),
        });
        for i in 0..50 {
            app.log_line(format!("transcript line {i}"));
        }
        // 80x10 forces every pane to its minimum: the exact shape class
        // that panicked on chunk indices before.
        let text = rendered(&app, 80, 10);
        assert!(text.contains("gemini"), "top/status lost:\n{text}");
        assert!(text.contains("transcript line 49"), "tail clipped:\n{text}");
    }

    #[test]
    fn transcript_autoscrolls_to_bottom() {
        let mut app = TuiApp::new();
        for i in 0..200 {
            app.log_line(format!("line {i:03}"));
        }
        let text = rendered(&app, 80, 15);
        assert!(text.contains("line 199"), "newest line hidden:\n{text}");
        assert!(!text.contains("line 000"), "viewport stuck at top:\n{text}");
    }

    #[test]
    fn scroll_keys_lift_and_release_viewport() {
        let mut app = TuiApp::new();
        assert_eq!(app.scroll_lines_up, 0);
        app.scroll_up();
        assert_eq!(app.scroll_lines_up, 10);
        app.scroll_down();
        assert_eq!(app.scroll_lines_up, 0);
        app.scroll_up();
        app.scroll_to_bottom();
        assert_eq!(app.scroll_lines_up, 0);
    }

    #[tokio::test]
    async fn steps_command_sends_budgets() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(32);
        app.dispatch_slash("steps", "12 40", &tx).await;
        match rx.recv().await.expect("expected a command") {
            TuryaCommand::UpdateConfig {
                max_steps,
                max_tool_calls,
                provider,
                model,
                ..
            } => {
                assert_eq!(max_steps, Some(12));
                assert_eq!(max_tool_calls, Some(40));
                assert_eq!(provider, None);
                assert_eq!(model, None);
            }
            other => panic!("expected UpdateConfig, got {other:?}"),
        }
        assert!(app.transcript_text().contains("budgets set"));
    }

    #[tokio::test]
    async fn steps_command_rejects_garbage() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(32);
        app.dispatch_slash("steps", "lots", &tx).await;
        assert!(rx.try_recv().is_err(), "no command on bad input");
        assert!(app.transcript_text().contains("usage: /steps"));
        // Bare /steps reports usage, sends nothing.
        app.dispatch_slash("steps", "", &tx).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn input_height_grows_with_draft() {
        let mut app = TuiApp::new();
        assert_eq!(app.input_height(), 3);
        app.input = "one\ntwo\nthree".to_string();
        assert_eq!(app.input_height(), 5);
        app.input = (0..20)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(app.input_height(), 6);
    }

    #[test]
    fn multiline_draft_renders_all_lines() {
        let mut app = TuiApp::new();
        app.input = "first\nsecond".to_string();
        let text = rendered(&app, 80, 20);
        assert!(text.contains("first"), "line 1 lost:\n{text}");
        assert!(text.contains("second"), "line 2 lost:\n{text}");
    }

    #[test]
    fn ctrl_e_expands_truncated_output_once() {
        use turya_protocol::{ToolCall, ToolResult};
        let mut app = TuiApp::new();
        let big = "x".repeat(500);
        app.feed_flow_event(&TuryaEvent::ToolCallInitiated(ToolCall {
            call_id: "g9".to_string(),
            tool_name: "run_bash".to_string(),
            parameters: serde_json::json!({}),
            signature: None,
        }));
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "g9".to_string(),
            success: true,
            output: big.clone(),
            error: None,
        }));
        // Inline shows the truncated preview, never the full text.
        assert!(app.transcript_text().contains("[+"));
        assert!(!app.transcript_text().contains(&big));
        // Ctrl+E pours the full text in — exactly once.
        app.expand_last_output();
        assert!(app.transcript_text().contains(&big));
        app.expand_last_output();
        assert!(app.transcript_text().contains("no truncated output"));
    }

    #[test]
    fn short_outputs_are_not_retained() {
        use turya_protocol::{ToolCall, ToolResult};
        let mut app = TuiApp::new();
        app.feed_flow_event(&TuryaEvent::ToolCallInitiated(ToolCall {
            call_id: "g8".to_string(),
            tool_name: "run_bash".to_string(),
            parameters: serde_json::json!({}),
            signature: None,
        }));
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "g8".to_string(),
            success: true,
            output: "ok".to_string(),
            error: None,
        }));
        app.expand_last_output();
        assert!(app.transcript_text().contains("no truncated output"));
    }

    #[test]
    fn system_toasts_render_dimmed() {
        let mut app = TuiApp::new();
        app.log_line("content".to_string());
        app.log_dim("toast".to_string());
        let rows = app.transcript_lines();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].style, Style::default());
        assert_eq!(rows[1].style, Style::default().fg(Color::DarkGray));
    }

    #[test]
    fn loaded_transcript_matches_live_turn_rows() {
        use turya_protocol::{Part, Transcript, TurnId};

        // A stored transcript replays into the same rows a live turn emits.
        let mut stored = Transcript::new("s1");
        stored.turns.push(turya_protocol::Turn {
            id: TurnId("t1".to_string()),
            parts: vec![
                Part::Text {
                    text: "running the tests".to_string(),
                },
                Part::ToolCall {
                    call_id: "c1".to_string(),
                    tool_name: "run_bash".to_string(),
                    arguments: serde_json::json!({"command": "make test"}),
                    signature: None,
                },
                Part::ToolResult {
                    call_id: "c1".to_string(),
                    output: "Tool 'run_bash' result (success=true): 143 passed".to_string(),
                    truncated: false,
                },
                Part::Text {
                    text: "all green".to_string(),
                },
            ],
        });

        let mut app = TuiApp::new();
        app.log_line("stale content".to_string());
        app.load_transcript(&stored);
        let text = app.transcript_text();
        assert!(!text.contains("stale"), "load replaces the view");
        assert!(text.contains("running the tests"));
        assert!(text.contains("run_bash"), "tool head kept: {text}");
        assert!(text.contains("143 passed"));
        assert!(text.contains("all green"));
        // Order is preserved: prose, then tool, then closing prose.
        let prose = text.find("running the tests").unwrap();
        let tool = text.find("run_bash").unwrap();
        let closing = text.find("all green").unwrap();
        assert!(prose < tool && tool < closing, "out of order: {text}");
    }

    #[test]
    fn loaded_truncated_output_is_expandable() {
        use turya_protocol::{Part, Transcript, TurnId};
        let mut stored = Transcript::new("s1");
        stored.turns.push(turya_protocol::Turn {
            id: TurnId("t1".to_string()),
            parts: vec![
                Part::ToolCall {
                    call_id: "c1".to_string(),
                    tool_name: "view_file".to_string(),
                    arguments: serde_json::json!({"path": "big.rs"}),
                    signature: None,
                },
                Part::ToolResult {
                    call_id: "c1".to_string(),
                    output: "y".repeat(900),
                    truncated: true,
                },
            ],
        });
        let mut app = TuiApp::new();
        app.load_transcript(&stored);
        app.expand_last_output();
        assert!(app.transcript_text().contains(&"y".repeat(900)));
    }

    #[test]
    fn empty_transcript_clears_the_view() {
        let mut app = TuiApp::new();
        app.log_line("old".to_string());
        app.load_transcript(&Transcript::new("s1"));
        assert_eq!(app.transcript_text().trim(), "");
    }

    #[test]
    fn token_stream_starts_new_line_after_dim() {
        let mut app = TuiApp::new();
        app.push_text("hel");
        app.push_text("lo");
        app.log_dim("───".to_string());
        app.push_text("next");
        assert_eq!(app.transcript_text(), "hello\n───\nnext");
    }
    #[test]
    fn recall_walks_back_to_the_newest_prompt() {
        let mut app = TuiApp::new();
        app.push_history("first");
        app.push_history("second");
        app.input = "draft".to_string();

        app.recall_older();
        assert_eq!(app.input, "second", "newest first");
        app.recall_older();
        assert_eq!(app.input, "first");
        // Past the oldest entry it stops, it does not wrap or clear.
        app.recall_older();
        assert_eq!(app.input, "first");
    }

    #[test]
    fn recall_forward_restores_the_stashed_draft() {
        let mut app = TuiApp::new();
        app.push_history("first");
        app.push_history("second");
        app.input = "half-written thought".to_string();

        app.recall_older();
        app.recall_older();
        assert_eq!(app.input, "first");
        app.recall_newer();
        assert_eq!(app.input, "second");
        app.recall_newer();
        assert_eq!(
            app.input, "half-written thought",
            "walking back to the end restores the draft"
        );
        // A further Down is a no-op rather than a panic or a clear.
        app.recall_newer();
        assert_eq!(app.input, "half-written thought");
    }

    #[test]
    fn editing_abandons_the_recall_cursor() {
        let mut app = TuiApp::new();
        app.push_history("first");
        app.recall_older();
        assert!(app.history_cursor.is_some());
        app.on_input_edited();
        assert!(
            app.history_cursor.is_none(),
            "the next Up starts from the newest entry"
        );
    }

    #[test]
    fn arrows_belong_to_the_input_only_when_nothing_else_is_open() {
        let mut app = TuiApp::new();
        assert!(app.input_owns_arrows(), "idle: the prompt owns Up/Down");
        app.completer = Some(Completer::new());
        assert!(!app.input_owns_arrows(), "completer: it owns Up/Down");
        app.completer = None;
        app.flow = Flow::Browser(flows::BrowserFlow::new(flows::BrowserMode::Models));
        assert!(!app.input_owns_arrows(), "browser: it owns Up/Down");
    }

    #[test]
    fn history_skips_blanks_and_collapses_repeats() {
        let mut app = TuiApp::new();
        app.push_history("same");
        app.push_history("same");
        app.push_history("   ");
        app.push_history("other");
        assert_eq!(app.prompt_history, vec!["same", "other"]);
    }

    #[test]
    fn history_is_bounded() {
        let mut app = TuiApp::new();
        for i in 0..(MAX_PROMPT_HISTORY + 25) {
            app.push_history(&format!("p{i}"));
        }
        assert_eq!(app.prompt_history.len(), MAX_PROMPT_HISTORY);
        assert_eq!(
            app.prompt_history.last().map(String::as_str),
            Some(format!("p{}", MAX_PROMPT_HISTORY + 24).as_str()),
            "newest kept, oldest dropped"
        );
    }

    #[tokio::test]
    async fn a_submitted_prompt_lands_in_recall_history() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(8);
        app.input = "remember me".to_string();
        let prompt = std::mem::take(&mut app.input);
        app.submit_prompt(prompt, &tx).await;
        assert_eq!(app.prompt_history, vec!["remember me"]);
        assert!(matches!(
            rx.try_recv(),
            Ok(TuryaCommand::SubmitPrompt { .. })
        ));
        // And the next recall returns it.
        app.recall_older();
        assert_eq!(app.input, "remember me");
    }
}
