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
use turya_protocol::{AgentMode, PermissionDecision, TuryaCommand, TuryaEvent};

pub mod slash;

pub mod flows;

use flows::{AuthFlow, AuthStage, BrowserFlow, BrowserMode, Flow};
use slash::{Completer, SlashRegistry};

/// Centered overlay rect sitting just above the input pane (which sits
/// above the 1-row status bar, hence the +4 offset).
fn centered_popup(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height.saturating_sub(4)).max(3);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h + 4);
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
    streamed_text: String,
    pending_permission: Option<(String, String)>, // (request_id, action)
    /// call_id → tool name, filled on Initiated, drained on Completed, so
    /// result lines can name the tool (`ToolResult` carries no name).
    pending_tools: std::collections::HashMap<String, String>,
    /// Active provider selection: (provider, model, via). Set optimistically
    /// on switch, corrected by `ProviderState` events from the host.
    provider: Option<(String, String, String)>,
    /// Character counts for the status bar (token estimates ≈ chars/4).
    sent_chars: usize,
    recv_chars: usize,
    /// Reasoning visibility toggle (`/thinking`). Display-only for now.
    show_thinking: bool,
    registry: SlashRegistry,
    completer: Option<Completer>,
    flow: Flow,
    /// `/auth <provider>` jumps straight into that provider's login once
    /// the provider list arrives.
    pending_auth: Option<String>,
}

impl Default for TuiApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiApp {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            streamed_text: String::new(),
            pending_permission: None,
            pending_tools: std::collections::HashMap::new(),
            provider: None,
            sent_chars: 0,
            recv_chars: 0,
            show_thinking: true,
            registry: SlashRegistry::with_builtins(),
            completer: None,
            flow: Flow::None,
            pending_auth: None,
        }
    }

    /// Append one line to the transcript (the single home for chat, tool
    /// activity, and toasts — there is no separate tool box).
    fn log_line(&mut self, line: String) {
        self.streamed_text.push_str(&line);
        self.streamed_text.push('\n');
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

    /// One-line status bar: provider/model, token estimates, thinking flag.
    /// Token counts are char-based estimates (≈), clearly marked — true
    /// provider usage blocks are a follow-up.
    fn status_line(&self) -> String {
        let model = self
            .provider
            .as_ref()
            .map(|(p, m, via)| format!("{p}/{m} ({via})"))
            .unwrap_or_else(|| "no provider".to_string());
        format!(
            " {} │ ↑{} ↓{}≈tok │ think:{} │ Esc stop · Ctrl+C quit",
            model,
            Self::fmt_tokens(self.sent_chars),
            Self::fmt_tokens(self.recv_chars),
            if self.show_thinking { "on" } else { "off" },
        )
    }

    /// Submit a prompt: echo it into the transcript (so your messages are
    /// visible) and forward it to the engine.
    async fn submit_prompt(&mut self, prompt: String, cmd_tx: &mpsc::Sender<TuryaCommand>) {
        self.sent_chars += prompt.len();
        self.streamed_text.push_str(&format_user_message(&prompt));
        let _ = cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt,
                mode: AgentMode::Build,
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
                    self.streamed_text.push_str(&text);
                }
                "clear" => {
                    self.streamed_text.clear();
                }
                "thinking" => {
                    self.show_thinking = !self.show_thinking;
                    self.log_line(format!(
                        "ℹ reasoning display {}",
                        if self.show_thinking { "on" } else { "off" }
                    ));
                }
                _ => {
                    self.log_line(format!("ℹ /{name} is coming soon"));
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
                    self.log_line(format!("ℹ unknown command /{name}"));
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
                                    self.log_line(format!("→ switching to {pid}/{mid}…"));
                                    self.flow = Flow::None;
                                    let _ = cmd_tx
                                        .send(TuryaCommand::UpdateConfig {
                                            permission_mode: None,
                                            provider: Some(pid),
                                            model: Some(mid),
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

    /// Feed a protocol event into the open flow (badges, pickers, stages).
    fn feed_flow_event(&mut self, evt: &TuryaEvent) {
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
                    self.log_line(format!("ℹ unknown provider '{target}'"));
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
            _ => {}
        }
    }
    /// Render one full frame. Split out of `run()` so headless tests can
    /// drive the REAL draw path with ratatui's `TestBackend`.
    fn render(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Min(5),    // Chat transcript (tools inline)
                Constraint::Length(3), // Input Box / Permission Prompt
                Constraint::Length(1), // Status bar (no borders: 1 row)
            ])
            .split(f.area());

        // 1. Header
        let header = Paragraph::new(format!(
            " Turya v{} | Mode: Build | Security: Review-for-me",
            env!("CARGO_PKG_VERSION")
        ))
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL).title("Status"));
        f.render_widget(header, chunks[0]);

        // 2. Chat transcript (user messages, assistant tokens, tool
        // activity, and toasts share one chronological stream).
        let chat = Paragraph::new(self.streamed_text.as_str())
            .wrap(Wrap { trim: false })
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
            f.render_widget(prompt, chunks[3]);
        } else {
            let input_widget = Paragraph::new(self.input.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Prompt (Enter send · / commands · Esc stop · Ctrl+C quit)"),
            );
            f.render_widget(input_widget, chunks[3]);
        }

        // 5. Status bar: single borderless row — provider/model, token
        // estimates (≈), thinking flag. Every row earns its place.
        f.render_widget(
            Paragraph::new(self.status_line()).style(Style::default().fg(Color::DarkGray)),
            chunks[4],
        );

        // 6. Slash autocomplete popup (overlay above the input pane).
        if let Some(ref comp) = self.completer {
            let matches = comp.matches(&self.input, &self.registry);
            let rows: Vec<Line> = slash::popup_rows(&matches, comp.selected)
                .into_iter()
                .map(Line::from)
                .collect();
            if !rows.is_empty() {
                let area = centered_popup(f.area(), 60, (rows.len() as u16 + 2).min(9));
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
                let area = centered_popup(f.area(), 72, height);
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
                let area = centered_popup(f.area(), 70, (rows.len() as u16 + 2).min(14));
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
                            KeyCode::Char(c) => self.input.push(c),
                            KeyCode::Backspace => { self.input.pop(); }
                            KeyCode::Enter if !self.input.trim().is_empty() => {
                                let prompt = std::mem::take(&mut self.input);
                                self.submit_prompt(prompt, &cmd_tx).await;
                            }
                            _ => {}
                        }
                    }
                }
                Some(evt) = event_rx.recv() => {
                    // Flows observe every event first (badges, pickers, stages).
                    self.feed_flow_event(&evt);
                    match evt {
                        TuryaEvent::TokenDelta { chunk } => {
                            self.recv_chars += chunk.len();
                            self.streamed_text.push_str(&chunk);
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
                            for line in format_tool_result(&name, &res) {
                                self.log_line(line);
                            }
                        }
                        TuryaEvent::PermissionRequested { request_id, action, .. } => {
                            self.pending_permission = Some((request_id, action));
                        }
                        TuryaEvent::TurnCompleted { .. } => {
                            self.streamed_text.push_str("\n[Turn Finished]\n");
                        }
                        TuryaEvent::Error { message } => {
                            self.log_line(format!("⚠ {message}"));
                        }
                        TuryaEvent::ProviderState {
                            provider,
                            model,
                            via,
                        } => {
                            // Host truth corrects the optimistic switch label.
                            self.provider = Some((provider, model, via));
                        }
                        _ => {}
                    }
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
        assert!(app.streamed_text.contains("unknown provider"));
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
        assert!(app.streamed_text.contains("connected via api-key"));
    }

    #[test]
    fn auth_failure_without_flow_toasts() {
        let mut app = TuiApp::new();
        app.feed_flow_event(&TuryaEvent::AuthFlowFailed {
            flow_id: "f9".to_string(),
            reason: "bad code".to_string(),
        });
        assert!(app.streamed_text.contains("bad code"));
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
        assert!(app.streamed_text.contains("switching to"));
    }

    #[tokio::test]
    async fn submit_prompt_echoes_into_transcript() {
        let mut app = TuiApp::new();
        let (tx, mut rx) = mpsc::channel(32);
        app.submit_prompt("understand codebase".to_string(), &tx)
            .await;
        assert!(app.streamed_text.contains("understand codebase"));
        assert!(app.streamed_text.contains("👤"));
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
        }));
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "g1".to_string(),
            success: true,
            output: "line1\nline2\n".to_string(),
            error: None,
        }));
        assert!(app.streamed_text.contains("✔ run_bash"));
        assert!(app.streamed_text.contains("line1"));

        // Unknown call ids still render (never silent, never panics).
        app.feed_flow_event(&TuryaEvent::ToolCallCompleted(ToolResult {
            call_id: "zzz".to_string(),
            success: false,
            output: String::new(),
            error: Some("boom".to_string()),
        }));
        assert!(app.streamed_text.contains("✘ tool"));
        assert!(app.streamed_text.contains("boom"));
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
        assert!(app.status_line().contains("no provider"));
        assert!(app.status_line().contains("think:on"));
        app.feed_flow_event(&TuryaEvent::ProviderState {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash".to_string(),
            via: "stored-key".to_string(),
        });
        let line = app.status_line();
        assert!(line.contains("gemini/gemini-2.5-flash"));
        assert!(line.contains("stored-key"));
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
        assert!(app.streamed_text.contains("reasoning display off"));
        app.dispatch_slash("thinking", "", &tx).await;
        assert!(app.show_thinking);
    }
}
