//! Slash-command registry + autocomplete (TUI-plugin internals).
//!
//! Rule 3.2: client-side only. `Local` commands run inline in the TUI;
//! `EngineFlow` commands emit `turya-protocol` messages handled by the host
//! (STEP 7). No engine/auth/provider imports here — see the R2 gate.

/// Where a slash command executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// Handled inside the TUI plugin (`/help`, `/clear`).
    Local,
    /// Drives protocol messages against the host (`/models`, `/auth`).
    EngineFlow,
}

/// Command source: built-in or contributed by a plugin (later via
/// `RegisterSlashCommand`; same registry, no TUI code change).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Builtin,
    Plugin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub hint: &'static str,
    pub kind: CommandKind,
    pub source: CommandSource,
}

#[derive(Debug, Default)]
pub struct SlashRegistry {
    commands: Vec<SlashCommand>,
}

impl SlashRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a command; duplicate names are rejected (first wins).
    pub fn register(&mut self, cmd: SlashCommand) {
        if !self.commands.iter().any(|c| c.name == cmd.name) {
            self.commands.push(cmd);
        }
    }

    /// Built-in command set (v1).
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        for cmd in [
            SlashCommand {
                name: "models",
                description: "Browse providers & switch model",
                hint: "Tab to complete",
                kind: CommandKind::EngineFlow,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "auth",
                description: "Log in to providers (API key / OAuth)",
                hint: "Tab to complete",
                kind: CommandKind::EngineFlow,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "help",
                description: "Show available commands",
                hint: "",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "clear",
                description: "Clear chat and tool logs",
                hint: "",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "efforts",
                description: "Token usage and cost (coming soon)",
                hint: "",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "thinking",
                description: "Toggle reasoning visibility",
                hint: "",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "steps",
                description: "Set per-turn budgets: /steps [model_calls] [tool_calls]",
                hint: "8 32",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "compact",
                description: "Summarize older turns to free context: /compact [focus]",
                hint: "focus on the auth bug",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "context",
                description: "Show context usage against the model window",
                hint: "",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
            SlashCommand {
                name: "sessions",
                description: "List stored sessions: /sessions [id] to resume",
                hint: "s-1730000000-1234",
                kind: CommandKind::Local,
                source: CommandSource::Builtin,
            },
        ] {
            reg.register(cmd);
        }
        reg
    }

    /// Filter by query (text after `/`): case-insensitive substring,
    /// prefix matches first, then alphabetical. Empty query lists all.
    pub fn filter<'a>(&'a self, query: &str) -> Vec<&'a SlashCommand> {
        let q = query.to_lowercase();
        let mut hits: Vec<&SlashCommand> = self
            .commands
            .iter()
            .filter(|c| c.name.contains(q.as_str()))
            .collect();
        hits.sort_by_key(|c| (!c.name.starts_with(q.as_str()), c.name));
        hits
    }

    pub fn get(&self, name: &str) -> Option<&SlashCommand> {
        self.commands.iter().find(|c| c.name == name)
    }
}

/// Autocomplete session: selection index only; the query is always derived
/// from the visible input buffer (`input.strip_prefix('/')`), so the two
/// can never diverge.
#[derive(Debug, Default)]
pub struct Completer {
    pub selected: usize,
}

impl Completer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn matches<'a>(&self, input: &str, registry: &'a SlashRegistry) -> Vec<&'a SlashCommand> {
        let query = input.strip_prefix('/').unwrap_or(input);
        // Only the command token completes (args after a space don't refilter).
        let token = query.split_whitespace().next().unwrap_or("");
        registry.filter(token)
    }

    /// Move selection with wrap-around over `matches_len` rows.
    pub fn move_selection(&mut self, delta: isize, matches_len: usize) {
        if matches_len == 0 {
            self.selected = 0;
            return;
        }
        let len = matches_len as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn reset(&mut self) {
        self.selected = 0;
    }
}

/// Build popup rows (`❯` marks the selection, clamped). Pure: the draw
/// code maps these to `Line`s, keeping render logic unit-testable.
/// How many rows the command popup shows before it scrolls.
pub const POPUP_ROWS: usize = 7;

pub fn popup_rows(matches: &[&SlashCommand], selected: usize) -> Vec<String> {
    if matches.is_empty() {
        return vec![];
    }
    let sel = selected.min(matches.len() - 1);
    // Window the list so the selection is always visible. Without this, a
    // selection past the viewport rendered *no* selected row at all, which is
    // how a 10-command list silently broke arrow navigation.
    let start = sel.saturating_sub(POPUP_ROWS - 1);
    matches
        .iter()
        .skip(start)
        .take(POPUP_ROWS)
        .enumerate()
        .map(|(i, c)| {
            let marker = if i + start == sel { "❯" } else { " " };
            format!("{marker} /{} — {}", c.name, c.description)
        })
        .collect()
}

/// Resolve Enter-in-Completing to `(command_name, args)`.
/// Exact-name prefix wins with trailing args (`/auth gemini`);
/// otherwise the highlighted match with no args.
pub fn dispatch_completion(
    input: &str,
    matches: &[&SlashCommand],
    selected: usize,
) -> Option<(String, String)> {
    let rest = input.strip_prefix('/')?;
    let chosen = matches.get(selected).or_else(|| matches.first())?;
    // Args are whatever follows the command name (`/auth gemini` → "gemini");
    // a partial prefix (`/mo`) yields no args.
    let args = rest
        .strip_prefix(chosen.name)
        .unwrap_or("")
        .trim()
        .to_string();
    Some((chosen.name.to_string(), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SlashRegistry {
        SlashRegistry::with_builtins()
    }

    #[test]
    fn builtins_registered_once() {
        let reg = registry();
        assert_eq!(reg.filter("").len(), 10);
        let mut dup = registry();
        dup.register(SlashCommand {
            name: "models",
            description: "dup",
            hint: "",
            kind: CommandKind::Local,
            source: CommandSource::Plugin,
        });
        assert_eq!(dup.filter("").len(), 10);
        assert_eq!(
            dup.get("models").unwrap().description,
            "Browse providers & switch model"
        );
    }

    #[test]
    fn filter_prefix_first_then_alpha() {
        let reg = registry();
        let names: Vec<_> = reg.filter("mo").iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["models"]);
        let all: Vec<_> = reg.filter("").iter().map(|c| c.name).collect();
        assert_eq!(
            all,
            vec![
                "auth", "clear", "compact", "context", "efforts", "help", "models", "sessions",
                "steps", "thinking"
            ]
        );
        let ci: Vec<_> = reg.filter("MO").iter().map(|c| c.name).collect();
        assert_eq!(ci, vec!["models"]);
        assert!(reg.filter("zzz").is_empty());
    }

    #[test]
    fn completer_selection_wraps() {
        let mut c = Completer::new();
        c.move_selection(1, 5);
        assert_eq!(c.selected, 1);
        c.move_selection(-1, 5);
        assert_eq!(c.selected, 0);
        c.move_selection(-1, 5);
        assert_eq!(c.selected, 4);
        c.move_selection(0, 0);
        assert_eq!(c.selected, 0);
        c.reset();
        assert_eq!(c.selected, 0);
    }

    #[test]
    fn completer_matches_token_only() {
        let reg = registry();
        let c = Completer::new();
        // Args after a space don't refilter.
        let m = c.matches("/auth gemini", &reg);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "auth");
        // Bare slash lists everything.
        assert_eq!(c.matches("/", &reg).len(), 10);
    }

    #[test]
    fn popup_rows_mark_selection_and_clamp() {
        let reg = registry();
        let c = Completer::new();
        let m = c.matches("/mo", &reg);
        let rows = popup_rows(&m, 0);
        assert_eq!(rows, vec!["❯ /models — Browse providers & switch model"]);
        // Out-of-range selection clamps to last row.
        let all = c.matches("/", &reg);
        let rows = popup_rows(&all, 99);
        // The popup is a fixed-height viewport: it shows a window of the
        // matches, not all of them, and clamps the selection to the last row.
        assert!(rows.len() <= POPUP_ROWS, "viewport is bounded");
        assert_eq!(rows.len(), all.len().min(POPUP_ROWS));
        // The window scrolled to the clamped selection, and it is marked.
        assert!(rows.last().unwrap().starts_with("❯"), "rows: {rows:?}");
        assert!(rows[0].starts_with("  "));
        // Every command stays reachable by arrowing down.
        let mid = popup_rows(&all, 3);
        assert!(mid[3].starts_with("❯"), "rows: {mid:?}");
        assert!(popup_rows(&[], 0).is_empty());
    }

    #[test]
    fn dispatch_exact_name_carries_args() {
        let reg = registry();
        let c = Completer::new();
        let m = c.matches("/auth gemini", &reg);
        assert_eq!(
            dispatch_completion("/auth gemini", &m, 0),
            Some(("auth".to_string(), "gemini".to_string()))
        );
        let m = c.matches("/mo", &reg);
        assert_eq!(
            dispatch_completion("/mo", &m, 0),
            Some(("models".to_string(), String::new()))
        );
        // Selection past the end falls back to first match.
        assert_eq!(
            dispatch_completion("/mo", &m, 99),
            Some(("models".to_string(), String::new()))
        );
        assert_eq!(dispatch_completion("plain text", &[], 0), None);
    }
}
