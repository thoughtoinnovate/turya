use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
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
use turya_protocol::{
    AgentMode, Attachment, Part, PermissionDecision, Transcript, TuryaCommand, TuryaEvent,
};

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
/// Wrapped lines per wheel notch. Three reads as "a real scroll" without
/// overshooting a short transcript.
const WHEEL_LINES: usize = 3;
/// Bound the retained entries: old outputs age out, newest survive.
const MAX_OUTPUT_ENTRIES: usize = 20;

/// Best-effort MIME guess from the extension. Unknown types are sent as a
/// generic file, which every provider accepts; a wrong guess on a known type
/// is worse than a vague one.
fn guess_mime(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "md" => "text/markdown",
        "json" => "application/json",
        "rs" | "py" | "js" | "ts" | "sh" | "txt" | "log" | "toml" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Who a transcript row belongs to. Identity is carried by a gutter glyph and
/// a label, never by colour alone: that survives monochrome terminals, every
/// theme, and colour-vision differences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Speaker {
    User,
    Assistant,
    Tool,
    System,
}

/// Optional background tints, off by default. A hard-pinned background is
/// invisible on some themes and hostile on others, so these stay opt-in
/// (`/settings`); the default identity is the gutter.
#[derive(Debug, Clone, Copy, Default)]
struct Tints {
    user: Option<Color>,
    assistant: Option<Color>,
    tool: Option<Color>,
}

/// Convert a markdown block into plain terminal lines.
///
/// Styles are dropped on purpose: the transcript already colours by role, and
/// layering markdown emphasis on top of it produces noise nobody can read in
/// a terminal. What the renderer buys us is structure - real indentation for
/// lists, stripped heading hashes, aligned code blocks - which is the part
/// that makes a model answer scannable. The raw text is always still stored.
fn render_markdown(text: &str, _tints: &Tints) -> Vec<String> {
    let rendered = tui_markdown::from_str(text).to_string();
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in rendered.split('\n') {
        let trimmed = line.trim_start();
        // The renderer strips inline emphasis but keeps heading markers and
        // fence lines; a terminal transcript should not show either.
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if hashes <= 6 && trimmed[hashes..].starts_with(' ') {
                out.push(trimmed[hashes..].trim_start().to_string());
                continue;
            }
        }
        out.push(line.trim_end().to_string());
    }
    // A single trailing blank from a closing fence is noise in a scrollback.
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Render a tint for display: hex back, or a readable name.
fn tint_name(c: Option<Color>) -> String {
    match c {
        None => "none".to_string(),
        Some(Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        Some(other) => format!("{other:?}").to_lowercase(),
    }
}

impl Tints {
    /// The tint for a role, if the user enabled one.
    fn for_role(&self, role: Speaker) -> Option<Color> {
        match role {
            Speaker::User => self.user,
            Speaker::Assistant => self.assistant,
            Speaker::Tool => self.tool,
            // System chrome stays on the terminal's own background.
            Speaker::System => None,
        }
    }

    /// Apply settings: any unset or unparsable value means "no tint".
    fn from_settings(user: Option<&str>, assistant: Option<&str>, tool: Option<&str>) -> Self {
        Self {
            user: user.and_then(Self::parse),
            assistant: assistant.and_then(Self::parse),
            tool: tool.and_then(Self::parse),
        }
    }

    /// Hex (`#rrggbb`) or a ratatui colour name, or `none`. Returns `None`
    /// for anything unrecognised rather than guessing a colour.
    fn parse(raw: &str) -> Option<Color> {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("none") || raw.eq_ignore_ascii_case("default")
        {
            return None;
        }
        if let Some(hex) = raw.strip_prefix('#') {
            if hex.len() == 6 {
                if let Ok(v) = u32::from_str_radix(hex, 16) {
                    return Some(Color::Rgb(
                        (v >> 16) as u8,
                        ((v >> 8) & 0xff) as u8,
                        (v & 0xff) as u8,
                    ));
                }
            }
            return None;
        }
        match raw.to_ascii_lowercase().as_str() {
            "black" => Some(Color::Black),
            "red" => Some(Color::Red),
            "green" => Some(Color::Green),
            "yellow" => Some(Color::Yellow),
            "blue" => Some(Color::Blue),
            "magenta" => Some(Color::Magenta),
            "cyan" => Some(Color::Cyan),
            "gray" | "grey" => Some(Color::Gray),
            "darkgray" | "darkgrey" => Some(Color::DarkGray),
            "lightred" => Some(Color::LightRed),
            "lightgreen" => Some(Color::LightGreen),
            "lightyellow" => Some(Color::LightYellow),
            "lightblue" => Some(Color::LightBlue),
            "lightmagenta" => Some(Color::LightMagenta),
            "lightcyan" => Some(Color::LightCyan),
            "white" => Some(Color::White),
            _ => None,
        }
    }
}

struct TLine {
    text: String,
    /// System toasts and separators: dimmed so content stands out.
    dim: bool,
    role: Speaker,
}

impl TLine {
    fn new(text: String, dim: bool, role: Speaker) -> Self {
        Self { text, dim, role }
    }
}

/// Gutter + label for a user prompt. A vertical rule and a word, readable in
/// monochrome, which is what makes "my messages" findable when scrolling.
fn format_user_message(prompt: &str) -> String {
    format!("\n┃ You\n┃ {prompt}\n")
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
    format_tool_result_timed(tool_name, res, None)
}

/// A tool result as a collapsed head plus a short preview.
///
/// `elapsed` appears when known: "passed (1.2s)" says whether a call was
/// worth the wait, which the output alone does not. The preview is short on
/// purpose - the full output is retained and re-shown on demand with Ctrl+E.
fn format_tool_result_timed(
    tool_name: &str,
    res: &turya_protocol::ToolResult,
    elapsed: Option<std::time::Duration>,
) -> Vec<String> {
    let took = elapsed
        .map(|d| format!(" ({:.1}s)", d.as_secs_f32()))
        .unwrap_or_default();
    if res.success {
        let mut out = vec![format!("✔ {tool_name}{took}")];
        let preview = res.output.trim();
        if !preview.is_empty() {
            let shown = truncate_preview(preview, 120);
            let hidden = preview.len().saturating_sub(shown.len());
            out.push(format!(
                "  {}{}",
                shown.replace('\n', "\n  "),
                if hidden > 0 {
                    format!(" [+{hidden} more · Ctrl+E]")
                } else {
                    String::new()
                }
            ));
        }
        out
    } else {
        let err = res.error.as_deref().unwrap_or("unknown error");
        vec![format!(
            "✘ {tool_name}{took} failed: {}",
            truncate_preview(err, 200)
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
    /// Optional background tints; off unless the user opts in.
    tints: Tints,
    /// Prompts waiting for the current turn to finish (`/queue`).
    queued: usize,
    /// call_id → when the tool started, so a completion can say how long it
    /// took. Only kept for in-flight calls, so it cannot grow.
    tool_started: std::collections::HashMap<String, std::time::Instant>,
    /// Files staged for the next prompt via `@path`, shown as chips.
    attachments: Vec<Attachment>,
    /// True while the input starts with an unclosed `@` word, which is when
    /// the path completer takes over.
    at_completer: Option<(usize, Vec<String>, usize)>,
    /// `auto` (default) | `on` | `off`, from `/settings mouse`.
    mouse: Option<String>,
    /// Render assistant prose as markdown. On by default; the stored
    /// transcript keeps the raw text either way, so this is presentation
    /// only and reversible.
    render_markdown: bool,
    /// The assistant's answer as it streams in, plus where it started in the
    /// transcript. Markdown cannot be rendered mid-stream, so the plain text
    /// is shown live and swapped for the rendered block when the turn ends.
    streaming: String,
    stream_start: Option<usize>,
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
            tints: Tints::default(),
            mouse: Some("auto".to_string()),
            queued: 0,
            attachments: Vec::new(),
            at_completer: None,
            tool_started: std::collections::HashMap::new(),
            render_markdown: true,
            streaming: String::new(),
            stream_start: None,
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
        self.log_line_as(line, Speaker::Assistant);
    }

    fn log_line_as(&mut self, line: String, role: Speaker) {
        self.transcript.push(TLine::new(line, false, role));
    }

    /// Append a dimmed system toast (routing notices, usage hints).
    /// Warnings, errors, and confirmations stay full-bright.
    fn log_dim(&mut self, line: String) {
        self.transcript
            .push(TLine::new(line, true, Speaker::System));
    }

    /// Append a (possibly multi-line) block, preserving blank lines so
    /// rendering matches the old plain-string transcript exactly.
    fn push_block(&mut self, text: &str, dim: bool) {
        self.push_block_as(text, dim, Speaker::Assistant)
    }

    fn push_block_as(&mut self, text: &str, dim: bool, role: Speaker) {
        // Only assistant prose is markdown. Prompts, tool output and system
        // rows are literal: a user pasting `**stars**` must see the stars.
        if !dim && role == Speaker::Assistant && self.render_markdown {
            for line in render_markdown(text, &self.tints) {
                self.transcript.push(TLine::new(line, false, role));
            }
            return;
        }
        for line in text.split('\n') {
            self.transcript
                .push(TLine::new(line.to_string(), dim, role));
        }
    }

    /// Mark a block as the final answer, so it renders as markdown.
    pub fn push_assistant_answer(&mut self, text: &str) {
        self.push_block_as(text, false, Speaker::Assistant);
    }

    /// Stream one token chunk: extend the current content line, or start a
    /// new one when the transcript is empty or ends in a dimmed row.
    fn push_text(&mut self, chunk: &str) {
        if self.stream_start.is_none() {
            self.stream_start = Some(self.transcript.len());
        }
        self.streaming.push_str(chunk);
        match self.transcript.last_mut() {
            Some(last) if !last.dim => last.text.push_str(chunk),
            _ => self
                .transcript
                .push(TLine::new(chunk.to_string(), false, Speaker::Assistant)),
        }
    }

    /// Swap the live plain-text stream for the rendered markdown block. The
    /// text is identical; only its structure changes, so nothing is lost.
    fn finish_streaming_answer(&mut self) {
        let (Some(start), true) = (self.stream_start.take(), self.render_markdown) else {
            self.streaming.clear();
            return;
        };
        if self.streaming.trim().is_empty() {
            return;
        }
        self.transcript.truncate(start);
        let text = std::mem::take(&mut self.streaming);
        self.push_block_as(&text, false, Speaker::Assistant);
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
                let mut style = Style::default();
                // A tint is an *addition* to the gutter, never the carrier of
                // identity: with tints off the roles are still distinct.
                if let Some(bg) = self.tints.for_role(l.role) {
                    style = style.bg(bg);
                }
                if l.dim {
                    style = style.fg(Color::DarkGray);
                } else if l.role == Speaker::User {
                    style = style.fg(Color::Cyan);
                } else if l.role == Speaker::Tool {
                    style = style.fg(Color::Gray);
                }
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
                        self.push_block_as(&format_user_message(text), false, Speaker::User);
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
                            self.log_line_as(line, Speaker::Tool);
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

    /// Should we capture the mouse? `auto` (the default) enables it only
    /// where it is known to work: not inside a multiplexer whose mouse mode
    /// may be off, and not on a terminal with no colour/graphics support.
    /// A terminal that cannot answer is treated as "do not capture" — losing
    /// the wheel is better than breaking the user's selection.
    fn mouse_enabled(&self) -> bool {
        match self.mouse.as_deref().unwrap_or("auto") {
            "on" | "true" | "yes" => return true,
            "off" | "false" | "no" => return false,
            _ => {}
        }
        if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
            return false;
        }
        match std::env::var("TERM") {
            Ok(term) => {
                let t = term.to_ascii_lowercase();
                !(t.is_empty() || t == "dumb" || t.starts_with("screen"))
            }
            Err(_) => false,
        }
    }

    /// Mouse input. Only the wheel is consumed: clicks would steal the
    /// terminal's own text selection, which is the one thing a user cannot
    /// get back.
    fn on_mouse(&mut self, mouse: MouseEvent) {
        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => -(WHEEL_LINES as i32),
            MouseEventKind::ScrollDown => WHEEL_LINES as i32,
            _ => return,
        };
        // A flow owns the keyboard, so it owns the wheel too: scroll its
        // list rather than the transcript behind the popup.
        if !matches!(self.flow, Flow::None) {
            self.flow_scroll(delta);
            return;
        }
        self.scroll_by(delta);
    }

    /// Move an open flow's selection by a wheel notch.
    ///
    /// Deliberately synchronous and limited to selection movement: the wheel
    /// cannot press Enter or type, so it needs none of the async routing that
    /// `handle_flow_key` does for real key presses.
    fn flow_scroll(&mut self, delta: i32) {
        let up = delta < 0;
        match &mut self.flow {
            Flow::Browser(b) => {
                if b.right {
                    b.move_model(if up { -1 } else { 1 });
                } else {
                    b.move_prov(if up { -1 } else { 1 });
                }
            }
            Flow::Auth(a) => {
                let len = a.methods().len();
                if len == 0 {
                    return;
                }
                if let AuthStage::MethodPick { sel } = &mut a.stage {
                    let step = if up { -1i32 } else { 1 };
                    *sel = (*sel as i32 + step).rem_euclid(len as i32) as usize;
                }
            }
            Flow::None => {}
        }
    }

    /// Apply persisted presentation settings from the host.
    pub fn apply_settings(&mut self, tints: Option<(&str, &str, &str)>, mouse: Option<&str>) {
        if let Some((user, assistant, tool)) = tints {
            self.tints = Tints::from_settings(Some(user), Some(assistant), Some(tool));
        }
        if let Some(m) = mouse {
            self.mouse = Some(m.to_string());
        }
    }

    /// Lift the transcript viewport up (read back history).
    fn scroll_up(&mut self) {
        self.scroll_lines_up = self.scroll_lines_up.saturating_add(10);
    }

    /// Lower the viewport toward live output.
    fn scroll_down(&mut self) {
        self.scroll_lines_up = self.scroll_lines_up.saturating_sub(10);
    }

    /// Scroll by a signed number of wrapped lines (wheel and Shift+arrows).
    fn scroll_by(&mut self, delta: i32) {
        if delta < 0 {
            self.scroll_lines_up = self.scroll_lines_up.saturating_add((-delta) as usize);
        } else {
            self.scroll_lines_up = self.scroll_lines_up.saturating_sub(delta as usize);
        }
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
        // The queue count lives in the status bar, not the transcript: it is
        // state about the session, not a message, and it must be visible
        // while scrolling back through history.
        let queue = if self.queued == 0 {
            String::new()
        } else {
            format!(" ⏳{}", self.queued)
        };
        format!(
            " ↑{} ↓{}≈tok{queue} │ think:{} │ Build · Review-for-me",
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
        // The chips are echoed with the prompt, so what was attached is
        // visible in the scrollback afterwards.
        let chips = self.attachment_summary();
        self.push_block(&format_user_message(&format!("{prompt}{chips}")), false);
        self.push_block_as(&chips, false, Speaker::User);
        let attachments = std::mem::take(&mut self.attachments);
        let _ = cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt,
                mode: AgentMode::Build,
                attachments,
            })
            .await;
    }

    /// The `@word` being typed, if the caret is inside one.
    fn current_at_word(&self) -> Option<(usize, String)> {
        let at = self.input.rfind('@')?;
        // Only at a word boundary, so `a@b.example.com` is never mistaken for
        // a path.
        let before_ok = at == 0
            || self.input[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if !before_ok {
            return None;
        }
        let after = &self.input[at + 1..];
        // A finished word is not a path any more.
        if after.contains(char::is_whitespace) {
            return None;
        }
        Some((at + 1, after.to_string()))
    }

    /// Recompute the path candidates for the current `@word`.
    fn refresh_at_completer(&mut self) {
        match self.current_at_word() {
            Some((start, word)) => {
                let (len, hits) = self.at_candidates(&word);
                self.at_completer = if hits.is_empty() {
                    None
                } else {
                    Some((start, hits, 0))
                };
                let _ = len;
            }
            None => self.at_completer = None,
        }
    }

    fn move_at_selection(&mut self, delta: i32) {
        if let Some((_, hits, sel)) = &mut self.at_completer {
            let n = hits.len() as i32;
            *sel = (*sel as i32 + delta).rem_euclid(n) as usize;
        }
    }

    /// Replace the `@word` with the highlighted path and stage the file.
    fn accept_at_completion(&mut self) {
        let Some((start, hits, sel)) = self.at_completer.take() else {
            return;
        };
        let Some(name) = hits.get(sel).cloned() else {
            return;
        };
        let path = name.trim_end_matches('/').to_string();
        self.input.truncate(start);
        self.input.push_str(&name);
        match self.attach_path(&path) {
            Ok(()) => {
                self.at_completer = None;
                let summary = self.attachment_summary();
                self.log_dim(format!("→ attached {path}{summary}"));
            }
            Err(e) => {
                // Leave the text alone so the user can fix the path.
                self.at_completer = None;
                self.log_dim(format!("⚠ {e}"));
            }
        }
    }

    /// The staged-attachment line shown under the input and in the
    /// transcript. Empty when nothing is attached.
    fn attachment_summary(&self) -> String {
        if self.attachments.is_empty() {
            return String::new();
        }
        let names: Vec<String> = self
            .attachments
            .iter()
            .map(|a| {
                a.path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| a.path.to_string_lossy().to_string())
            })
            .collect();
        format!("\n📎 {}", names.join("  "))
    }

    /// Stage a file for the next prompt. A path that does not exist is
    /// refused here, where the user can see why, rather than becoming a
    /// silent empty part in the request.
    fn attach_path(&mut self, raw: &str) -> Result<(), String> {
        let path = std::path::PathBuf::from(raw.trim());
        if !path.exists() {
            return Err(format!("no such file: {raw}"));
        }
        let mime = guess_mime(&path);
        self.attachments.push(Attachment { path, mime });
        Ok(())
    }

    /// Complete a partial `@word` against the working directory. Returns
    /// the matches and the byte range the word occupies.
    fn at_candidates(&self, prefix: &str) -> (usize, Vec<String>) {
        let dir = std::path::Path::new(".");
        let needle = prefix.to_lowercase();
        let mut hits = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if name.to_lowercase().contains(&needle) {
                    let suffix = if e.path().is_dir() { "/" } else { "" };
                    hits.push(format!("{name}{suffix}"));
                }
            }
        }
        hits.sort();
        hits.truncate(8);
        (prefix.len(), hits)
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
            Some(slash::CommandKind::Local) => {
                match name {
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
                    "queue" => {
                        // `/queue <prompt>` sends the prompt to run after the
                        // current turn. It never interrupts: to change a running
                        // turn's mind, press Esc and rephrase.
                        let prompt = args.trim();
                        if prompt.is_empty() {
                            if self.queued == 0 {
                                self.log_dim("ℹ nothing queued".to_string());
                            } else {
                                self.log_dim(format!(
                                    "⏳ {} prompt(s) queued; they run as the current turn finishes",
                                    self.queued
                                ));
                            }
                            return;
                        }
                        if prompt.eq_ignore_ascii_case("clear") {
                            let _ = cmd_tx.send(TuryaCommand::ClearQueue).await;
                            self.log_dim("→ queue cleared".to_string());
                            return;
                        }
                        let _ = cmd_tx
                            .send(TuryaCommand::QueuePrompt {
                                prompt: prompt.to_string(),
                            })
                            .await;
                        self.log_dim(format!("⏳ queued: {prompt}"));
                    }
                    "settings" => {
                        // `/settings` alone shows the current values; with a key it
                        // is an immediate, discoverable setter rather than a modal
                        // the user has to learn.
                        let mut parts = args.split_whitespace();
                        match (parts.next(), parts.next()) {
                            (None, _) => {
                                let t = self.tints;
                                self.log_dim(format!(
                                    "tints: user={} assistant={} tool={} (none = terminal default)",
                                    tint_name(t.user),
                                    tint_name(t.assistant),
                                    tint_name(t.tool)
                                ));
                                self.log_dim(format!(
                                    "mouse: {} ({})",
                                    self.mouse.clone().unwrap_or_else(|| "auto".into()),
                                    if self.mouse_enabled() {
                                        "captured"
                                    } else {
                                        "not captured"
                                    }
                                ));
                                self.log_dim(
                                    "set one with: /settings user_bg #1b2735  \
                                 (/settings user_bg none to clear)"
                                        .to_string(),
                                );
                            }
                            (Some(key), Some(value)) => {
                                if key == "mouse" {
                                    let v = value.to_ascii_lowercase();
                                    let mode = match v.as_str() {
                                        "on" | "true" | "yes" => Some("on"),
                                        "off" | "false" | "no" => Some("off"),
                                        "auto" => Some("auto"),
                                        _ => None,
                                    };
                                    match mode {
                                        Some(m) => {
                                            self.mouse = Some(m.to_string());
                                            self.log_dim(format!(
                                                "→ mouse = {m} ({})",
                                                if self.mouse_enabled() {
                                                    "captured on this terminal"
                                                } else {
                                                    "not captured here"
                                                }
                                            ));
                                        }
                                        None => self
                                            .log_dim("ℹ mouse takes auto | on | off".to_string()),
                                    }
                                    return;
                                }
                                let parsed = Tints::parse(value);
                                let slot = match key {
                                    "user_bg" => &mut self.tints.user,
                                    "assistant_bg" => &mut self.tints.assistant,
                                    "tool_bg" => &mut self.tints.tool,
                                    _ => {
                                        self.log_dim(format!(
                                        "ℹ unknown setting '{key}'; try user_bg, assistant_bg, tool_bg"
                                    ));
                                        return;
                                    }
                                };
                                *slot = parsed;
                                self.log_dim(format!("→ {key} = {}", tint_name(parsed)));
                            }
                            (Some(key), None) => self
                                .log_dim(format!("ℹ {key} needs a value (e.g. #1b2735 or none)")),
                        }
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
                }
            }
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
                // A one-line head, not a growing log: the tool's own output is
                // folded away and only shown when the user asks for it.
                self.log_line_as(format!("▸ {}", call.tool_name), Speaker::Tool);
                self.pending_tools
                    .insert(call.call_id.clone(), call.tool_name.clone());
                self.tool_started
                    .insert(call.call_id.clone(), std::time::Instant::now());
            }
            TuryaEvent::ToolCallCompleted(res) => {
                let name = self
                    .pending_tools
                    .remove(&res.call_id)
                    .unwrap_or_else(|| "tool".to_string());
                let elapsed = self.tool_started.remove(&res.call_id).map(|t| t.elapsed());
                for line in format_tool_result_timed(&name, res, elapsed) {
                    self.log_line_as(line, Speaker::Tool);
                }
                self.retain_output(&name, res);
            }
            TuryaEvent::PermissionRequested {
                request_id, action, ..
            } => {
                self.pending_permission = Some((request_id.clone(), action.clone()));
            }
            TuryaEvent::QueueChanged { pending } => {
                // Count only: the running turn must not be interrupted by a
                // follow-up the user typed while it worked.
                self.queued = *pending;
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
                self.finish_streaming_answer();
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
        let base = self.input.lines().count().max(1) as u16 + 2;
        // A staged attachment or an open `@` picker needs its own rows, or it
        // would be drawn over the transcript.
        let extra = if self.attachments.is_empty() && self.at_completer.is_none() {
            0
        } else {
            1 + self
                .at_completer
                .as_ref()
                .map(|(_, hits, _)| hits.len().min(4) as u16)
                .unwrap_or(0)
        };
        (base + extra).min(10)
    }

    /// The `@` picker's rows, rendered directly under the input.
    fn at_picker_rows(&self) -> Vec<Line<'_>> {
        let Some((_, hits, sel)) = &self.at_completer else {
            return Vec::new();
        };
        hits.iter()
            .enumerate()
            .map(|(i, name)| {
                let marker = if i == *sel { "❯ " } else { "  " };
                Line::styled(
                    format!("{marker}{name}"),
                    Style::default().fg(if i == *sel { Color::Cyan } else { Color::Gray }),
                )
            })
            .collect()
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

        // 3b. Staged attachments + the @ picker, between transcript and
        // input, so the user can see what they are about to send.
        let mut input_area = chunks[2];
        if !self.attachments.is_empty() || self.at_completer.is_some() {
            let mut rows: Vec<Line<'_>> = Vec::new();
            if !self.attachments.is_empty() {
                let summary = self.attachment_summary();
                rows.push(Line::styled(
                    summary.trim_start().to_string(),
                    Style::default().fg(Color::Green),
                ));
            }
            rows.extend(self.at_picker_rows());
            let h = rows.len() as u16;
            let picker = Rect {
                x: chunks[2].x + 1,
                y: chunks[2].y,
                width: chunks[2].width.saturating_sub(2),
                height: h,
            };
            f.render_widget(Paragraph::new(rows), picker);
            input_area = Rect {
                y: chunks[2].y + h,
                height: chunks[2].height.saturating_sub(h),
                ..chunks[2]
            };
        }

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
            f.render_widget(prompt, input_area);
        } else {
            let input_widget =
                Paragraph::new(self.input.as_str())
                    .block(Block::default().borders(Borders::ALL).title(
                    "Prompt (Enter send · Alt+Enter newline · @ file · Ctrl+E expand · Esc stop)",
                ));
            f.render_widget(input_widget, input_area);
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
        // Mouse capture is opt-out: wheel scrolling is what people expect in
        // 2026, but it also disables the terminal's own text selection. Where
        // capture is unreliable (tmux/screen without mouse mode) it is worse
        // than nothing, so we probe and can be turned off in /settings.
        if self.mouse_enabled() {
            execute!(stdout, EnableMouseCapture)?;
        }
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
                    if let Event::Mouse(mouse) = event {
                        // Wheel events route to whatever has focus: a popup
                        // scrolls its own list, otherwise the transcript does.
                        self.on_mouse(mouse);
                        continue;
                    }
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
                            // `@` opens a file picker in place. This is the
                            // attachment method that works everywhere:
                            // clipboard image paste fails over SSH, in some
                            // Windows terminals and under some tmux setups,
                            // but a path on disk is just a path.
                            KeyCode::Char('@') => {
                                self.input.push('@');
                                self.on_input_edited();
                                self.refresh_at_completer();
                            }
                            KeyCode::Tab if self.at_completer.is_some() => {
                                self.accept_at_completion();
                            }
                            KeyCode::Backspace if self.at_completer.is_some() => {
                                self.input.pop();
                                self.on_input_edited();
                                if self.input.ends_with('@') {
                                    self.at_completer = None;
                                } else {
                                    self.refresh_at_completer();
                                }
                            }
                            KeyCode::Esc if self.at_completer.is_some() => {
                                self.at_completer = None;
                                continue;
                            }
                            KeyCode::Enter if self.at_completer.is_some() => {
                                // Enter with the picker open attaches the
                                // highlighted file and keeps editing, rather
                                // than submitting a half-typed @word.
                                self.accept_at_completion();
                                continue;
                            }
                            KeyCode::Up if self.at_completer.is_some() => {
                                self.move_at_selection(-1);
                                continue;
                            }
                            KeyCode::Down if self.at_completer.is_some() => {
                                self.move_at_selection(1);
                                continue;
                            }
                            KeyCode::Char(c) => {
                                self.input.push(c);
                                self.on_input_edited();
                                if self.at_completer.is_some() {
                                    self.refresh_at_completer();
                                }
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

        if self.mouse_enabled() {
            let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
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
        // The gutter replaces the old emoji: readable in monochrome, and a
        // stable column to scan when scrolling.
        assert!(app.transcript_text().contains("┃ You"));
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
        // Bounded, so a long draft cannot eat the transcript.
        assert_eq!(app.input_height(), 10);
        assert!(app.input_height() <= 10);
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
    #[test]
    fn user_prompts_carry_a_gutter_and_label() {
        // Identity must not depend on colour: this string is what the user
        // scans for when scrolling back through a long session.
        let line = format_user_message("fix the bug");
        assert!(line.contains("┃"), "gutter rule present: {line:?}");
        assert!(line.contains("You"), "label present: {line:?}");
        assert!(line.contains("fix the bug"), "content kept");
    }

    #[test]
    fn roles_map_to_distinct_styles() {
        let mut app = TuiApp::new();
        app.log_line_as("user text".to_string(), Speaker::User);
        app.log_line_as("model text".to_string(), Speaker::Assistant);
        app.log_line_as("tool text".to_string(), Speaker::Tool);
        app.log_dim("system text".to_string());

        let rows = app.transcript_lines();
        let style_of = |needle: &str| {
            rows.iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
                .map(|l| l.style)
                .unwrap()
        };
        let user = style_of("user text");
        let assistant = style_of("model text");
        let tool = style_of("tool text");
        let system = style_of("system text");
        assert_eq!(user.fg, Some(Color::Cyan), "user is accented");
        assert_eq!(assistant.fg, None, "assistant stays on default");
        assert_eq!(tool.fg, Some(Color::Gray), "tool is subdued");
        assert_eq!(system.fg, Some(Color::DarkGray), "system is dimmed");
        // No background unless the user asked for one.
        for s in [user, assistant, tool, system] {
            assert_eq!(s.bg, None, "tints are off by default");
        }
    }

    #[test]
    fn tints_are_opt_in_and_must_parse() {
        assert_eq!(Tints::parse("none"), None);
        assert_eq!(Tints::parse(""), None);
        assert_eq!(Tints::parse("default"), None);
        assert_eq!(Tints::parse("chartreuse"), None, "unknown is not guessed");
        assert_eq!(Tints::parse("cyan"), Some(Color::Cyan));
        assert_eq!(Tints::parse("#112233"), Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(Tints::parse("#12345"), None, "malformed hex is refused");

        let tints = Tints::from_settings(Some("#112233"), None, Some("blue"));
        assert_eq!(tints.user, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(tints.assistant, None);
        assert_eq!(tints.tool, Some(Color::Blue));
        // System chrome never picks up a tint.
        assert_eq!(tints.for_role(Speaker::System), None);
    }

    #[test]
    fn an_enabled_tint_reaches_only_that_role() {
        let mut app = TuiApp::new();
        app.tints = Tints::from_settings(Some("#101820"), None, None);
        app.log_line_as("mine".to_string(), Speaker::User);
        app.log_line_as("theirs".to_string(), Speaker::Assistant);
        let rows = app.transcript_lines();
        let bg_of = |needle: &str| {
            rows.iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
                .map(|l| l.style.bg)
                .unwrap()
        };
        assert_eq!(bg_of("mine"), Some(Color::Rgb(0x10, 0x18, 0x20)));
        assert_eq!(bg_of("theirs"), None, "one role's tint, not everyone's");
    }

    #[test]
    fn a_tint_never_drops_the_role_colour() {
        // Tints are additive: turning one on must not make a user prompt stop
        // looking like a user prompt.
        let mut app = TuiApp::new();
        app.tints = Tints::from_settings(Some("#101820"), None, None);
        app.log_line_as("mine".to_string(), Speaker::User);
        let rows = app.transcript_lines();
        let row = rows
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("mine")))
            .unwrap();
        assert_eq!(row.style.fg, Some(Color::Cyan));
        assert!(row.style.bg.is_some());
    }

    #[test]
    fn monochrome_terminals_still_show_roles() {
        // The gutter survives even with every colour removed, which is the
        // whole reason identity is not colour-alone.
        let line = format_user_message("hello");
        let visible = line.chars().filter(|c| !c.is_whitespace()).count();
        assert!(visible > 0);
        assert!(line.contains('┃'));
    }
    #[test]
    fn wheel_scrolls_the_transcript() {
        let mut app = TuiApp::new();
        app.scroll_to_bottom();
        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_lines_up, WHEEL_LINES);
        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_lines_up, WHEEL_LINES * 2);
        app.on_mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_lines_up, WHEEL_LINES);
        app.scroll_to_bottom();
        // Scrolling past the bottom is a no-op, never a wrap or a panic.
        app.on_mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll_lines_up, 0);
    }

    #[test]
    fn clicks_are_ignored_so_selection_still_works() {
        let mut app = TuiApp::new();
        app.on_mouse(mouse(MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        )));
        app.on_mouse(mouse(MouseEventKind::Moved));
        assert_eq!(app.scroll_lines_up, 0, "we never consume clicks");
        assert!(app.input.is_empty(), "and never type anything");
    }

    #[test]
    fn wheel_moves_a_browser_selection_instead_of_the_transcript() {
        let mut app = TuiApp::new();
        app.flow = Flow::Browser(flows::BrowserFlow::new(flows::BrowserMode::Models));
        app.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_lines_up, 0, "the popup owns the wheel");
    }

    #[test]
    fn mouse_capture_respects_setting_and_environment() {
        // Explicit wins over the probe.
        let mut app = TuiApp::new();
        app.mouse = Some("on".into());
        assert!(app.mouse_enabled());
        app.mouse = Some("off".into());
        assert!(!app.mouse_enabled());

        // `auto` is the default, and a dumb or absent TERM is not captured.
        app.mouse = Some("auto".into());
        if std::env::var("TERM").ok().as_deref() == Some("dumb") {
            assert!(!app.mouse_enabled());
        }
    }

    #[tokio::test]
    async fn settings_report_tints_and_mouse() {
        let mut app = TuiApp::new();
        app.dispatch_slash("settings", "user_bg #101820", &mpsc::channel(1).0)
            .await;
        assert_eq!(app.tints.user, Some(Color::Rgb(0x10, 0x18, 0x20)));
        let (tx, mut rx) = mpsc::channel(8);
        app.dispatch_slash("settings", "", &tx).await;
        let text = app.transcript_text();
        assert!(text.contains("tints:"), "{text}");
        assert!(text.contains("mouse:"), "{text}");
        // Bare /settings is a report: it sends nothing to the engine.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn settings_rejects_an_unknown_tint_without_guessing() {
        let mut app = TuiApp::new();
        app.dispatch_slash("settings", "user_bg notacolor", &mpsc::channel(1).0)
            .await;
        assert_eq!(app.tints.user, None, "unparsable means no tint");
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }
    #[test]
    fn markdown_headings_lose_their_hashes_and_lists_indent() {
        let mut app = TuiApp::new();
        app.push_block_as(
            "## Findings\n\n- first item\n- second item\n",
            false,
            Speaker::Assistant,
        );
        let text = app.transcript_text();
        assert!(text.contains("Findings"), "heading text kept: {text}");
        assert!(
            !text.contains("## "),
            "heading markers are consumed, not shown raw: {text}"
        );
        assert!(text.contains("first item"), "list content kept: {text}");
    }

    #[test]
    fn code_fences_render_as_an_indented_block() {
        let mut app = TuiApp::new();
        app.push_block_as(
            "Run it:\n\n```sh\nmake test\n```\n",
            false,
            Speaker::Assistant,
        );
        let text = app.transcript_text();
        assert!(text.contains("make test"), "code content kept: {text}");
        assert!(!text.contains("```"), "fences consumed: {text}");
    }

    #[test]
    fn user_text_is_never_markdown() {
        // A user pasting `**bold**` must see the asterisks, not lose them.
        let mut app = TuiApp::new();
        app.push_block_as("**not bold**", false, Speaker::User);
        assert!(app.transcript_text().contains("**not bold**"));
    }

    #[test]
    fn tool_output_is_never_markdown() {
        let mut app = TuiApp::new();
        app.push_block_as("## not a heading", false, Speaker::Tool);
        assert!(app.transcript_text().contains("## not a heading"));
    }

    #[test]
    fn a_streamed_answer_is_rendered_when_the_turn_ends() {
        let mut app = TuiApp::new();
        // Tokens arrive one at a time, as they do live.
        for chunk in ["## Plan\n\n", "- step one\n", "- step two\n"] {
            app.push_text(chunk);
        }
        // Mid-stream the text is still literal: markdown cannot be rendered
        // from half a token.
        assert!(app.transcript_text().contains("## Plan"));

        app.feed_flow_event(&TuryaEvent::TurnCompleted {
            turn_id: "t1".to_string(),
            success: true,
        });
        let text = app.transcript_text();
        assert!(!text.contains("## "), "rendered on completion: {text}");
        assert!(text.contains("Plan") && text.contains("step one"), "{text}");
        // The words survive: rendering changes shape, not content.
        assert!(text.contains("step one"), "content survives rendering");
    }

    #[test]
    fn an_empty_stream_leaves_no_stray_row() {
        let mut app = TuiApp::new();
        app.push_text("");
        app.feed_flow_event(&TuryaEvent::TurnCompleted {
            turn_id: "t1".to_string(),
            success: true,
        });
        let text = app.transcript_text();
        assert!(!text.contains("context compacted"), "{text}");
    }

    #[test]
    fn an_unterminated_code_fence_never_panics() {
        // A truncated stream is normal: the turn ended mid-answer.
        let out = render_markdown("```rust\nfn main() {\n    let x = 1;", &Tints::default());
        assert!(!out.is_empty(), "something always renders");
        assert!(out.iter().any(|l| l.contains("fn main")));
    }
    #[tokio::test]
    async fn queue_command_enqueues_without_interrupting() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(8);
        app.dispatch_slash("queue", "then run the tests", &tx).await;
        match rx.recv().await.expect("a command") {
            TuryaCommand::QueuePrompt { prompt } => assert_eq!(prompt, "then run the tests"),
            other => panic!("expected QueuePrompt, got {other:?}"),
        }
        assert!(
            app.transcript_text().contains("queued"),
            "{:?}",
            app.transcript_text()
        );
    }

    #[tokio::test]
    async fn queue_clear_empties_the_queue() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(8);
        app.dispatch_slash("queue", "clear", &tx).await;
        assert!(matches!(rx.recv().await, Some(TuryaCommand::ClearQueue)));
    }

    #[tokio::test]
    async fn bare_queue_reports_and_sends_nothing() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(8);
        app.dispatch_slash("queue", "", &tx).await;
        assert!(rx.try_recv().is_err(), "a report must not enqueue");
        assert!(app.transcript_text().contains("nothing queued"));
        app.feed_flow_event(&TuryaEvent::QueueChanged { pending: 2 });
        app.dispatch_slash("queue", "", &tx).await;
        assert!(app.transcript_text().contains("2 prompt(s) queued"));
    }

    #[test]
    fn the_status_bar_shows_the_queue_depth() {
        let mut app = TuiApp::new();
        assert!(!app.status_line().contains('⏳'), "no count when empty");
        app.feed_flow_event(&TuryaEvent::QueueChanged { pending: 3 });
        let line = app.status_line();
        assert!(line.contains("⏳3"), "{line}");
    }
    #[test]
    fn attaching_a_missing_file_is_refused_with_a_reason() {
        let mut app = TuiApp::new();
        let err = app.attach_path("/definitely/not/here.txt").unwrap_err();
        assert!(err.contains("no such file"), "{err}");
        assert!(app.attachments.is_empty(), "nothing half-staged");
    }

    #[test]
    fn attaching_a_real_file_records_its_mime() {
        let mut app = TuiApp::new();
        let path = std::env::temp_dir().join("turya-attach-test.png");
        std::fs::write(&path, b"not really a png").unwrap();
        app.attach_path(&path.to_string_lossy()).expect("stages");
        assert_eq!(app.attachments.len(), 1);
        assert_eq!(app.attachments[0].mime, "image/png");
        assert!(app.attachment_summary().contains("turya-attach-test.png"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mime_guessing_falls_back_rather_than_guessing_wrong() {
        let p = std::path::Path::new("a.rs");
        assert_eq!(guess_mime(p), "text/plain");
        assert_eq!(guess_mime(std::path::Path::new("a.pdf")), "application/pdf");
        assert_eq!(
            guess_mime(std::path::Path::new("a.weird")),
            "application/octet-stream"
        );
    }

    #[test]
    fn the_at_word_is_only_recognised_while_typing_one() {
        let mut app = TuiApp::new();
        // "look at @": the word starts at byte 9, just past the '@'.
        app.input = "look at @".to_string();
        assert_eq!(app.current_at_word(), Some((9, String::new())));
        app.input = "look at @src/".to_string();
        assert_eq!(app.current_at_word(), Some((9, "src/".to_string())));
        // An email address is not a path, however much it looks like one.
        app.input = "email me at a@b".to_string();
        assert_eq!(app.current_at_word(), None);
        // A finished word is not being typed.
        app.input = "see @src/lib.rs for details".to_string();
        assert_eq!(app.current_at_word(), None);
    }

    #[tokio::test]
    async fn a_submitted_prompt_carries_its_attachments() {
        let mut app = TuiApp::new();
        let path = std::env::temp_dir().join("turya-attach-send.txt");
        std::fs::write(&path, "hello").unwrap();
        app.attach_path(&path.to_string_lossy()).expect("stages");
        let (tx, mut rx) = mpsc::channel(8);
        let prompt = std::mem::take(&mut app.input);
        // `submit_prompt` is async; drive it to completion so the assertion
        // is not racing the send.
        let mut send = Box::pin(app.submit_prompt(prompt, &tx));
        send.as_mut().await;
        drop(send);
        match rx.try_recv() {
            Ok(TuryaCommand::SubmitPrompt { attachments, .. }) => {
                assert_eq!(attachments.len(), 1, "the file rides along");
            }
            other => panic!("expected SubmitPrompt with attachments, got {other:?}"),
        }
        assert!(app.attachments.is_empty(), "staging is consumed by send");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_input_grows_a_row_for_staged_attachments() {
        let mut app = TuiApp::new();
        let base = app.input_height();
        app.attachments.push(Attachment {
            path: std::path::PathBuf::from("/tmp/x.png"),
            mime: "image/png".to_string(),
        });
        assert!(app.input_height() > base, "the chip needs a row");
    }
    #[test]
    fn a_tool_result_collapses_to_a_head_plus_a_short_preview() {
        use turya_protocol::ToolResult;
        let big = "y".repeat(900);
        let res = ToolResult {
            call_id: "c1".to_string(),
            success: true,
            output: big,
            error: None,
        };
        let lines = format_tool_result_timed(
            "run_bash",
            &res,
            Some(std::time::Duration::from_millis(1200)),
        );
        assert!(lines[0].contains("✔ run_bash"), "head names the tool");
        assert!(lines[0].contains("1.2s"), "duration is shown: {}", lines[0]);
        let preview = lines[1].clone();
        assert!(preview.chars().count() < 200, "preview stays short");
        assert!(
            preview.contains("Ctrl+E"),
            "and says how to see the rest: {preview}"
        );
    }

    #[test]
    fn a_short_tool_output_has_no_expand_hint() {
        use turya_protocol::ToolResult;
        let res = ToolResult {
            call_id: "c1".to_string(),
            success: true,
            output: "ok".to_string(),
            error: None,
        };
        let lines = format_tool_result_timed("view_file", &res, None);
        assert_eq!(lines.len(), 2);
        assert!(
            !lines[1].contains("Ctrl+E"),
            "nothing to expand: {}",
            lines[1]
        );
        assert!(!lines[0].contains('s'), "no invented duration");
    }

    #[test]
    fn a_failed_tool_is_one_line_and_keeps_the_reason() {
        use turya_protocol::ToolResult;
        let res = ToolResult {
            call_id: "c1".to_string(),
            success: false,
            output: String::new(),
            error: Some("permission denied".to_string()),
        };
        let lines = format_tool_result_timed("write_file", &res, None);
        assert_eq!(lines.len(), 1, "a failure is one line");
        assert!(lines[0].contains("✘ write_file"), "{}", lines[0]);
        assert!(lines[0].contains("permission denied"), "{}", lines[0]);
    }

    #[tokio::test]
    async fn a_completed_tool_records_its_duration() {
        use turya_protocol::{ToolCall, ToolResult};
        let mut app = TuiApp::new();
        app.feed_flow_event(&TuryaEvent::ToolCallInitiated(ToolCall {
            call_id: "c1".to_string(),
            tool_name: "run_bash".to_string(),
            parameters: serde_json::json!({}),
            signature: None,
        }));
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "c1".to_string(),
            success: true,
            output: "done".to_string(),
            error: None,
        }));
        let text = app.transcript_text();
        assert!(text.contains("run_bash"), "{text}");
        assert!(text.contains('s'), "a duration is reported: {text}");
        // In-flight bookkeeping is cleaned up, not accumulated.
        assert!(app.tool_started.is_empty());
    }
}
