use crossterm::{
    event::{Event, EventStream, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use turya_protocol::{AgentMode, TuryaCommand, TuryaEvent, PermissionDecision};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::io::stdout;
use tokio::sync::mpsc;

pub struct TuiApp {
    input: String,
    streamed_text: String,
    tool_logs: Vec<String>,
    pending_permission: Option<(String, String)>, // (request_id, action)
}

impl TuiApp {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            streamed_text: String::new(),
            tool_logs: Vec::new(),
            pending_permission: None,
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

        loop {
            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3), // Header
                        Constraint::Min(5),    // Content & Streamed Text
                        Constraint::Length(5), // Tool Activity Log
                        Constraint::Length(3), // Input Box / Permission Prompt
                    ])
                    .split(f.area());

                // 1. Header
                let header = Paragraph::new(" Turya v0.1.0 | Mode: Build | Security: Review-for-me")
                    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                    .block(Block::default().borders(Borders::ALL).title("Status"));
                f.render_widget(header, chunks[0]);

                // 2. Chat Stream
                let chat = Paragraph::new(self.streamed_text.as_str())
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title("Assistant"));
                f.render_widget(chat, chunks[1]);

                // 3. Tool Activity
                let logs: Vec<Line> = self.tool_logs.iter().map(|l| Line::from(Span::raw(l))).collect();
                let tools_widget = Paragraph::new(logs)
                    .block(Block::default().borders(Borders::ALL).title("Tool Activity"));
                f.render_widget(tools_widget, chunks[2]);

                // 4. Input or Permission Prompt
                if let Some((_, ref action)) = self.pending_permission {
                    let prompt = Paragraph::new(format!(
                        "Allow '{}'? Press [y] to allow, [n] to deny",
                        action
                    ))
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                    .block(Block::default().borders(Borders::ALL).title("Permission Required"));
                    f.render_widget(prompt, chunks[3]);
                } else {
                    let input_widget = Paragraph::new(self.input.as_str()).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Prompt (Enter to send, Esc to exit)"),
                    );
                    f.render_widget(input_widget, chunks[3]);
                }
            })?;

            tokio::select! {
                Some(Ok(event)) = reader.next() => {
                    if let Event::Key(key) = event {
                        if key.code == KeyCode::Esc {
                            break;
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
                            KeyCode::Char(c) => self.input.push(c),
                            KeyCode::Backspace => { self.input.pop(); },
                            KeyCode::Enter => {
                                if !self.input.trim().is_empty() {
                                    let prompt = std::mem::take(&mut self.input);
                                    let _ = cmd_tx.send(TuryaCommand::SubmitPrompt {
                                        prompt,
                                        mode: AgentMode::Build,
                                    }).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some(evt) = event_rx.recv() => {
                    match evt {
                        TuryaEvent::TokenDelta { chunk } => {
                            self.streamed_text.push_str(&chunk);
                        }
                        TuryaEvent::ToolCallInitiated(call) => {
                            self.tool_logs.push(format!("Invoking: {}", call.tool_name));
                        }
                        TuryaEvent::ToolCallCompleted(res) => {
                            self.tool_logs.push(format!("Result (call {}): success={}", res.call_id, res.success));
                        }
                        TuryaEvent::PermissionRequested { request_id, action, .. } => {
                            self.pending_permission = Some((request_id, action));
                        }
                        TuryaEvent::TurnCompleted { .. } => {
                            self.streamed_text.push_str("\n[Turn Finished]\n");
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
