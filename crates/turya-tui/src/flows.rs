//! `/models` + `/auth` flow state machines (TUI-plugin internals).
//!
//! Rule 3.2: pure client state. These types never touch auth, providers,
//! or the network — they only decide which `TuryaCommand` to emit next and
//! what to render from the last `TuryaEvent`. Fully unit-tested without a
//! terminal or an engine.

/// One provider row in the browser (from `ProvidersListed`).
#[derive(Debug, Clone, Default)]
pub struct ProviderView {
    pub id: String,
    pub display_name: String,
    pub api_key: String,
    pub oauth: String,
    pub models: Vec<ModelView>,
}

impl ProviderView {
    pub fn badge(&self) -> &'static str {
        if self.api_key == "env"
            || self.api_key == "stored"
            || self.api_key == "connected"
            || self.oauth == "connected"
        {
            "●"
        } else {
            "○"
        }
    }

    pub fn is_locked(&self) -> bool {
        self.badge() == "○"
    }
}

/// One model row.
#[derive(Debug, Clone)]
pub struct ModelView {
    pub id: String,
    pub display_name: String,
    pub source: String,
}

/// What the browser is for: switching models or picking an auth target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    Models,
    AuthPick,
}

/// Visible rows per pane before the list scrolls.
pub const BROWSER_VISIBLE_ROWS: usize = 8;

/// Provider-first browser (mock M2).
#[derive(Debug, Clone)]
pub struct BrowserFlow {
    pub providers: Vec<ProviderView>,
    pub sel_prov: usize,
    pub sel_model: usize,
    pub right: bool,
    pub mode: BrowserMode,
    pub loading: bool,
    /// Live filter text (typed directly into the browser).
    pub query: String,
    prov_offset: usize,
    model_offset: usize,
}

fn matches(haystack: &str, query: &str) -> bool {
    query.is_empty() || haystack.to_lowercase().contains(&query.to_lowercase())
}

impl BrowserFlow {
    pub fn new(mode: BrowserMode) -> Self {
        Self {
            providers: vec![],
            sel_prov: 0,
            sel_model: 0,
            right: false,
            mode,
            loading: true,
            query: String::new(),
            prov_offset: 0,
            model_offset: 0,
        }
    }

    pub fn set_providers(&mut self, providers: Vec<ProviderView>) {
        self.providers = providers;
        self.loading = false;
        self.reset_selection();
    }

    /// Replace the filter text (resets selection + scroll, but PRESERVES
    /// the focused pane — typing must never yank focus back to providers).
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.sel_prov = 0;
        self.sel_model = 0;
        self.prov_offset = 0;
        self.model_offset = 0;
    }

    fn reset_selection(&mut self) {
        self.sel_prov = 0;
        self.sel_model = 0;
        self.right = false;
        self.prov_offset = 0;
        self.model_offset = 0;
    }

    /// Provider indices passing the filter (empty query = all). A provider
    /// is also visible when one of its models matches (discovery by model).
    pub fn visible_providers(&self) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                matches(&p.id, &self.query)
                    || matches(&p.display_name, &self.query)
                    || p.models.iter().any(|m| {
                        matches(&m.id, &self.query) || matches(&m.display_name, &self.query)
                    })
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Model indices of the current visible provider passing the filter.
    pub fn visible_models(&self) -> Vec<usize> {
        self.current()
            .map(|p| {
                p.models
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| {
                        matches(&m.id, &self.query) || matches(&m.display_name, &self.query)
                    })
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn current(&self) -> Option<&ProviderView> {
        let idx = *self.visible_providers().get(self.sel_prov)?;
        self.providers.get(idx)
    }

    pub fn move_prov(&mut self, delta: isize) {
        let len = self.visible_providers().len();
        if len == 0 {
            return;
        }
        self.sel_prov = (self.sel_prov as isize + delta).rem_euclid(len as isize) as usize;
        self.sel_model = 0;
        self.model_offset = 0;
        Self::follow(&mut self.prov_offset, self.sel_prov, len);
    }

    pub fn move_model(&mut self, delta: isize) {
        let len = self.visible_models().len();
        if len == 0 {
            return;
        }
        self.sel_model = (self.sel_model as isize + delta).rem_euclid(len as isize) as usize;
        Self::follow(&mut self.model_offset, self.sel_model, len);
    }

    /// Keep `selected` inside the `[offset, offset+WINDOW)` viewport.
    fn follow(offset: &mut usize, selected: usize, len: usize) {
        if selected < *offset {
            *offset = selected;
        } else if selected >= *offset + BROWSER_VISIBLE_ROWS {
            *offset = selected + 1 - BROWSER_VISIBLE_ROWS;
        }
        let max_offset = len.saturating_sub(BROWSER_VISIBLE_ROWS);
        *offset = (*offset).min(max_offset);
    }

    /// Currently highlighted model, if the provider is unlocked.
    pub fn selected_model(&self) -> Option<(String, String)> {
        let p = self.current()?;
        if p.is_locked() {
            return None;
        }
        let models = self.visible_models();
        let m = p.models.get(*models.get(self.sel_model)?)?;
        Some((p.id.clone(), m.id.clone()))
    }
}

/// Auth flow stages (mocks M3–M5).
#[derive(Debug, Clone)]
pub enum AuthStage {
    MethodPick { sel: usize },
    KeyPrompt { buffer: String },
    OAuthWait { url: String, buffer: String },
    Failed(String),
    Done(String),
}

/// Interactive login for one provider.
#[derive(Debug, Clone)]
pub struct AuthFlow {
    pub provider: String,
    pub display_name: String,
    pub oauth_supported: bool,
    pub stage: AuthStage,
    pub flow_id: Option<String>,
}

impl AuthFlow {
    pub fn new(provider: &str, display_name: &str, oauth_supported: bool) -> Self {
        Self {
            provider: provider.to_string(),
            display_name: display_name.to_string(),
            oauth_supported,
            stage: AuthStage::MethodPick { sel: 0 },
            flow_id: None,
        }
    }

    /// Method rows shown in the picker (OAuth hidden when unsupported).
    pub fn methods(&self) -> Vec<(&'static str, &'static str)> {
        let mut rows = vec![("api-key", "API key (masked prompt, verified, stored)")];
        if self.oauth_supported {
            rows.push(("oauth", "OAuth login (browser, Workspace/Enterprise)"));
        }
        rows
    }
}

/// Top-level TUI flow state.
#[derive(Debug, Clone)]
pub enum Flow {
    None,
    Browser(BrowserFlow),
    Auth(AuthFlow),
}

/// Render lines for the browser overlay. Pure (draw maps to `Line`s).
/// Both panes show a scrolling window of [`BROWSER_VISIBLE_ROWS`] rows with
/// `↑n`/`↓n` overflow markers; a non-empty filter adds a status header.
pub fn render_browser(flow: &BrowserFlow) -> (Vec<String>, Vec<String>) {
    if flow.loading {
        return (vec!["  ⠋ loading providers…".to_string()], vec![]);
    }
    let vis_prov = flow.visible_providers();
    if vis_prov.is_empty() {
        let empty = if flow.query.is_empty() {
            "  (no providers registered)".to_string()
        } else {
            format!("  (no match for “{}”)", flow.query)
        };
        return (vec![empty], vec![]);
    }
    let header = if flow.query.is_empty() {
        None
    } else {
        Some(format!(
            "  /{} ({} match{})",
            flow.query,
            vis_prov.len(),
            if vis_prov.len() == 1 { "" } else { "es" }
        ))
    };
    let left: Vec<String> = windowed(vis_prov.len(), flow.prov_offset, flow.sel_prov, |i| {
        let p = &flow.providers[vis_prov[i]];
        let lock = if p.is_locked() { " (locked)" } else { "" };
        format!("{} {}{}", p.badge(), p.display_name, lock)
    });
    let right: Vec<String> = flow
        .current()
        .map(|p| {
            if p.is_locked() {
                vec![
                    "  🔒 log in to see models".to_string(),
                    "  Enter → /auth".to_string(),
                ]
            } else {
                let vis_models = flow.visible_models();
                if vis_models.is_empty() && !flow.query.is_empty() {
                    vec![format!("  (no match for “{}”)", flow.query)]
                } else if vis_models.is_empty() {
                    vec!["  (no models)".to_string()]
                } else {
                    windowed(vis_models.len(), flow.model_offset, flow.sel_model, |i| {
                        let m = &p.models[vis_models[i]];
                        format!("{} [{}]", m.display_name, m.source)
                    })
                }
            }
        })
        .unwrap_or_default();
    let mut left = left;
    if let Some(h) = header {
        left.insert(0, h);
    }
    (left, right)
}

/// Slice `[offset, offset+WINDOW)` with selection markers + overflow counts.
fn windowed(
    len: usize,
    offset: usize,
    selected: usize,
    mut render: impl FnMut(usize) -> String,
) -> Vec<String> {
    let end = (offset + BROWSER_VISIBLE_ROWS).min(len);
    let mut rows = Vec::new();
    if offset > 0 {
        rows.push(format!("  ↑{offset} more"));
    }
    for i in offset..end {
        let marker = if i == selected { "❯" } else { " " };
        rows.push(format!("{marker} {}", render(i)));
    }
    if end < len {
        rows.push(format!("  ↓{} more", len - end));
    }
    rows
}

/// Render lines for the auth overlay. Pure.
pub fn render_auth(flow: &AuthFlow) -> Vec<String> {
    match &flow.stage {
        AuthStage::MethodPick { sel } => {
            let mut lines = vec![format!("Auth → {} — choose a method:", flow.display_name)];
            for (i, (id, desc)) in flow.methods().iter().enumerate() {
                let marker = if i == *sel { "❯" } else { " " };
                lines.push(format!("{marker} ① {id} — {desc}"));
            }
            lines
        }
        AuthStage::KeyPrompt { buffer } => {
            let masked: String = buffer.chars().map(|_| '•').collect();
            vec![
                format!("Auth → {} › API key:", flow.display_name),
                format!("Key: {masked}"),
                "Enter to verify & store · Esc to cancel".to_string(),
            ]
        }
        AuthStage::OAuthWait { url, buffer } => vec![
            format!("Auth → {} › OAuth:", flow.display_name),
            "Open this URL (also copied by the CLI):".to_string(),
            url.clone(),
            "Workspace/Enterprise accounts only — consumer accounts are rejected.".to_string(),
            format!("Paste code (or wait for browser): {buffer}"),
            "Enter to submit · Esc to cancel".to_string(),
        ],
        AuthStage::Failed(reason) => vec![
            format!("Auth → {} failed:", flow.display_name),
            reason.clone(),
            "Esc to go back".to_string(),
        ],
        AuthStage::Done(msg) => vec![msg.clone(), "Esc to continue".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BrowserFlow {
        let mut f = BrowserFlow::new(BrowserMode::Models);
        f.set_providers(vec![
            ProviderView {
                id: "gemini".into(),
                display_name: "Gemini".into(),
                api_key: "env".into(),
                oauth: "missing".into(),
                models: vec![
                    ModelView {
                        id: "gemini-2.5-pro".into(),
                        display_name: "Gemini 2.5 Pro".into(),
                        source: "static".into(),
                    },
                    ModelView {
                        id: "gemini-2.5-flash".into(),
                        display_name: "Gemini 2.5 Flash".into(),
                        source: "static".into(),
                    },
                ],
            },
            ProviderView {
                id: "openai".into(),
                display_name: "OpenAI".into(),
                api_key: "missing".into(),
                oauth: "missing".into(),
                models: vec![ModelView {
                    id: "gpt-x".into(),
                    display_name: "GPT X".into(),
                    source: "static".into(),
                }],
            },
        ]);
        f
    }

    #[test]
    fn badges_and_lock_state() {
        let f = sample();
        assert_eq!(f.providers[0].badge(), "●");
        assert!(!f.providers[0].is_locked());
        assert_eq!(f.providers[1].badge(), "○");
        assert!(f.providers[1].is_locked());
    }

    #[test]
    fn navigation_wraps_and_resets_model() {
        let mut f = sample();
        f.move_prov(1);
        assert_eq!(f.sel_prov, 1);
        f.move_model(0);
        f.move_prov(1);
        assert_eq!(f.sel_prov, 0); // wraps
        assert_eq!(f.sel_model, 0); // reset on provider change
    }

    #[test]
    fn locked_provider_yields_no_model() {
        let mut f = sample();
        f.sel_prov = 1;
        assert!(f.selected_model().is_none());
        f.sel_prov = 0;
        f.sel_model = 1;
        assert_eq!(
            f.selected_model(),
            Some(("gemini".to_string(), "gemini-2.5-flash".to_string()))
        );
    }

    #[test]
    fn render_browser_marks_selection_and_locks() {
        let f = sample();
        let (left, right) = render_browser(&f);
        assert!(left[0].starts_with("❯ ●"));
        assert!(left[1].contains("(locked)"));
        assert!(right[0].starts_with("❯"));
        let mut f = sample();
        f.sel_prov = 1;
        let (_, right) = render_browser(&f);
        assert!(right.iter().any(|l| l.contains("log in")));
    }

    #[test]
    fn query_filters_providers_and_models() {
        let mut f = sample();
        f.set_query("gem".to_string());
        assert_eq!(f.visible_providers(), vec![0]);
        assert_eq!(f.sel_prov, 0);
        let (left, _) = render_browser(&f);
        assert!(left[0].contains("/gem"));
        assert!(left.iter().any(|l| l.contains("1 match")));

        // Model-level filter surfaces the owning provider.
        f.set_query("flash".to_string());
        assert_eq!(f.visible_providers(), vec![0]);
        assert_eq!(
            f.selected_model(),
            // sel_model 0 over visible [flash] → first flash model
            Some(("gemini".to_string(), "gemini-2.5-flash".to_string()))
        );

        // No match anywhere.
        f.set_query("zzz-nope".to_string());
        assert!(f.visible_providers().is_empty());
        assert!(f.selected_model().is_none());
        let (left, _) = render_browser(&f);
        assert!(left.iter().any(|l| l.contains("no match")));
    }

    #[test]
    fn query_preserves_focused_pane() {
        // Regression: typing in model selection yanked focus back to
        // providers, making search appear provider-only.
        let mut f = sample();
        f.right = true;
        f.set_query("flash".to_string());
        assert!(f.right, "typing must not steal pane focus");
        assert_eq!(
            f.selected_model(),
            Some(("gemini".to_string(), "gemini-2.5-flash".to_string()))
        );
        // Clearing the query keeps the pane too.
        f.set_query(String::new());
        assert!(f.right);
    }

    #[test]
    fn scroll_viewport_follows_selection() {
        let mut f = sample();
        // Grow gemini to 12 models so the 8-row window must scroll.
        if let Some(p) = f.providers.get_mut(0) {
            for i in 0..10 {
                p.models.push(ModelView {
                    id: format!("extra-{i}"),
                    display_name: format!("Extra {i}"),
                    source: "static".into(),
                });
            }
        }
        f.right = true;
        for _ in 0..8 {
            f.move_model(1);
        }
        assert_eq!(f.sel_model, 8);
        let (_, right) = render_browser(&f);
        assert!(right.iter().any(|l| l.starts_with("  ↑")));
        assert!(right.iter().any(|l| l.starts_with("❯")));
        // Scroll back to top clears the marker.
        for _ in 0..8 {
            f.move_model(-1);
        }
        let (_, right) = render_browser(&f);
        assert!(!right.iter().any(|l| l.starts_with("  ↑")));
    }

    #[test]
    fn render_loading_and_empty() {
        let f = BrowserFlow::new(BrowserMode::Models);
        let (left, _) = render_browser(&f);
        assert!(left[0].contains("loading"));
    }

    #[test]
    fn auth_methods_hide_oauth_when_unsupported() {
        let a = AuthFlow::new("anthropic", "Anthropic", false);
        assert_eq!(a.methods().len(), 1);
        let g = AuthFlow::new("gemini", "Gemini", true);
        assert_eq!(g.methods().len(), 2);
        let lines = render_auth(&g);
        assert!(lines.iter().any(|l| l.contains("OAuth login")));
    }

    #[test]
    fn render_key_prompt_masks_input() {
        let mut f = AuthFlow::new("gemini", "Gemini", true);
        f.stage = AuthStage::KeyPrompt {
            buffer: "sk-secret".into(),
        };
        let lines = render_auth(&f);
        assert!(lines.iter().any(|l| l.contains("•••••••••")));
        assert!(!lines.iter().any(|l| l.contains("sk-secret")));
    }

    #[test]
    fn render_oauth_wait_and_failed() {
        let mut f = AuthFlow::new("gemini", "Gemini", true);
        f.stage = AuthStage::OAuthWait {
            url: "https://x.test".into(),
            buffer: String::new(),
        };
        let lines = render_auth(&f);
        assert!(lines.iter().any(|l| l.contains("Workspace/Enterprise")));
        f.stage = AuthStage::Failed("bad code".into());
        assert!(render_auth(&f).iter().any(|l| l.contains("bad code")));
    }
}
