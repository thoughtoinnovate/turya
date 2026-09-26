//! Host-side command router (CLI process).
//!
//! Rule 3.2: the host links plugin crates and answers provider/auth/catalog
//! protocol commands. `turya-server` stays a thin proxy (it ignores these
//! variants); `turya-core` never names a vendor. Any future host (headless
//! daemon, IDE bridge) replicates this small router against the same traits.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use turya_auth::{CredentialStore, SlotState};
use turya_core::{ProviderRegistry, TuryaEngine};
use turya_protocol::{AuthAction, ModelSummary, ProviderSummary, TuryaCommand, TuryaEvent};

/// Pending interactive auth flow (OAuth browser wait or key prompt).
struct PendingFlow {
    provider: String,
    method: String,
    verifier: String,
    redirect_uri: String,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Minimal `~/.turya/config.toml` (`provider = "…"`, `model = "…"`, line-based).
#[derive(Debug, Default, Clone)]
pub struct HostConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl HostConfig {
    pub fn load(path: &str) -> Self {
        let mut cfg = Self::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let (k, v) = match line.split_once('=') {
                    Some(p) => p,
                    None => continue,
                };
                let v = v.trim().trim_matches('"').to_string();
                match k.trim() {
                    "provider" => cfg.provider = Some(v),
                    "model" => cfg.model = Some(v),
                    _ => {}
                }
            }
        }
        cfg
    }

    pub fn save(&self, path: &str) {
        let mut text = String::new();
        if let Some(p) = &self.provider {
            text.push_str(&format!("provider = \"{p}\"\n"));
        }
        if let Some(m) = &self.model {
            text.push_str(&format!("model = \"{m}\"\n"));
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    }
}

pub struct HostServices {
    registry: Arc<ProviderRegistry>,
    store: Arc<dyn CredentialStore>,
    catalog: turya_catalog::Catalog,
    engine: Arc<TuryaEngine>,
    config_path: String,
    /// Session database path. The host owns session reads/writes so the
    /// kernel never learns a file path (Rule 3.1).
    db_path: String,
    /// Skill discovery warnings, captured at boot.
    skills_warnings: Vec<String>,
    flows: Mutex<HashMap<String, PendingFlow>>,
    flow_seq: Mutex<u64>,
}

impl HostServices {
    pub fn new(
        registry: Arc<ProviderRegistry>,
        store: Arc<dyn CredentialStore>,
        catalog: turya_catalog::Catalog,
        engine: Arc<TuryaEngine>,
        config_path: String,
        db_path: String,
        skills_warnings: Vec<String>,
    ) -> Self {
        Self {
            registry,
            store,
            catalog,
            engine,
            config_path,
            db_path,
            skills_warnings,
            flows: Mutex::new(HashMap::new()),
            flow_seq: Mutex::new(0),
        }
    }

    fn next_flow_id(&self) -> String {
        let mut seq = self.flow_seq.lock().unwrap();
        *seq += 1;
        format!("flow_{}", *seq)
    }

    fn slot_word(state: &SlotState) -> &'static str {
        match state {
            SlotState::Env => "env",
            SlotState::Stored => "stored",
            SlotState::Connected { .. } => "connected",
            SlotState::Missing => "missing",
            SlotState::Unsupported => "unsupported",
        }
    }

    fn auth_status_words(&self, provider: &str) -> (String, String) {
        let plugin = self.registry.get(provider);
        let (has_env, has_oauth) = match plugin.as_ref() {
            Some(p) => {
                let methods = p.auth_methods();
                (
                    methods
                        .iter()
                        .any(|m| matches!(m, turya_core::AuthMethodKind::ApiKey { .. })),
                    methods
                        .iter()
                        .any(|m| matches!(m, turya_core::AuthMethodKind::OAuth)),
                )
            }
            None => (false, false),
        };
        let api_key = if !has_env {
            SlotState::Unsupported
        } else {
            let env = plugin
                .and_then(|p| {
                    p.auth_methods().into_iter().find_map(|m| match m {
                        turya_core::AuthMethodKind::ApiKey { env_var } => Some(env_var),
                        _ => None,
                    })
                })
                .unwrap_or("");
            if std::env::var(env)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                SlotState::Env
            } else if self
                .store
                .get(&turya_auth::api_key_account(provider))
                .is_some()
            {
                SlotState::Stored
            } else {
                SlotState::Missing
            }
        };
        let oauth = if !has_oauth {
            SlotState::Unsupported
        } else if self
            .store
            .get(&turya_auth::oauth_refresh_account(provider))
            .is_some()
        {
            SlotState::Connected {
                account: String::new(),
            }
        } else {
            SlotState::Missing
        };
        (
            Self::slot_word(&api_key).to_string(),
            Self::slot_word(&oauth).to_string(),
        )
    }

    fn is_authenticated(&self, provider: &str) -> bool {
        let (a, o) = self.auth_status_words(provider);
        a == "env" || a == "stored" || a == "connected" || o == "connected"
    }

    /// Resolve credentials for a provider (env → stored → OAuth refresh).
    async fn resolve_for(&self, provider: &str) -> Result<turya_core::ResolvedCreds, String> {
        let client_id = std::env::var("TURYA_OAUTH_CLIENT_ID").ok().or_else(|| {
            self.store
                .get(&turya_auth::oauth_client_id_account(provider))
        });
        let cfg = client_id.map(|id| turya_auth::OAuthConfig::google(&id));
        turya_auth::resolver::resolve(provider, None, cfg.as_ref(), self.store.as_ref())
            .await
            .map_err(|e| e.to_string())
    }

    async fn live_ids(&self, provider: &str) -> Vec<String> {
        let plugin = match self.registry.get(provider) {
            Some(p) => p,
            None => return vec![],
        };
        let creds = match self.resolve_for(provider).await {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        plugin.list_models(&creds).await
    }

    async fn fetch_meta_doc(url: &str) -> Result<String, String> {
        let text = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent("turya-catalog")
            .build()
            .map_err(|e| e.to_string())?
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// Handle one client command. Takes `&TuryaCommand` so the router can
    /// forward unconsumed commands to the session proxy afterwards.
    pub async fn handle(&self, cmd: &TuryaCommand, events: &HostEventSink) -> bool {
        match cmd {
            // Compaction and context live here, not in the server: the host
            // owns the session store, the kernel owns the policy.
            TuryaCommand::Compact { focus } => {
                // Events go through the caller's sink, so the client that
                // asked for the compaction is the one that sees it.
                match self.engine.compact(focus.as_deref(), events.sender()).await {
                    Ok((_, marker)) => {
                        events
                            .send(TuryaEvent::TokenDelta {
                                chunk: format!("\n{marker}\n"),
                            })
                            .await
                    }
                    Err(e) => events.send(TuryaEvent::Error { message: e }).await,
                }
                true
            }
            TuryaCommand::ListSkills => {
                let skills = self.engine.available_skills().await;
                let warnings = self.skill_warnings();
                events
                    .send(TuryaEvent::SkillsListed { skills, warnings })
                    .await;
                true
            }
            TuryaCommand::LoadSkill { name } => {
                // An unknown name is not an error the model needs to handle:
                // it gets a clear "no such skill" text as a tool result.
                let body = self.engine.load_skill_body(name).await;
                events
                    .send(TuryaEvent::SkillsListed {
                        skills: self.engine.available_skills().await,
                        warnings: self.skill_warnings(),
                    })
                    .await;
                events
                    .send(TuryaEvent::TokenDelta {
                        chunk: match body {
                            Some(b) => b,
                            None => format!("no skill named '{name}'"),
                        },
                    })
                    .await;
                true
            }
            TuryaCommand::QueryEfforts => {
                let (provider, model) = self.current_selection();
                let reasoning = self.cached_reasoning(&provider, &model);
                events
                    .send(TuryaEvent::EffortsChanged {
                        model,
                        supported: reasoning.reasoning.supported,
                        efforts: reasoning.reasoning.efforts.clone(),
                        current: self.effort(),
                    })
                    .await;
                true
            }
            TuryaCommand::SetEffort { effort } => {
                // Validate against what the model actually advertises: an
                // invented level would be silently ignored by the provider.
                let (provider, model) = self.current_selection();
                let reasoning = self.cached_reasoning(&provider, &model);
                match &effort {
                    None => {
                        self.set_effort(None);
                        self.engine.provider().set_effort(None);
                        events
                            .send(TuryaEvent::EffortsChanged {
                                model,
                                supported: reasoning.reasoning.supported,
                                efforts: reasoning.reasoning.efforts.clone(),
                                current: None,
                            })
                            .await;
                    }
                    Some(level) => {
                        // Apply it to the live provider, not just the file:
                        // a setting the engine never sees is a lie.
                        self.engine.provider().set_effort(Some(level.clone()));
                        if !reasoning.reasoning.efforts.is_empty()
                            && !reasoning.reasoning.supports_effort(level)
                        {
                            events
                                .send(TuryaEvent::Error {
                                    message: format!(
                                        "{model} accepts {:?}; '{level}' is not one of them",
                                        reasoning.reasoning.efforts
                                    ),
                                })
                                .await;
                            return true;
                        }
                        self.set_effort(Some(level.clone()));
                        events
                            .send(TuryaEvent::EffortsChanged {
                                model,
                                supported: reasoning.reasoning.supported,
                                efforts: reasoning.reasoning.efforts.clone(),
                                current: Some(level.clone()),
                            })
                            .await;
                    }
                }
                true
            }
            TuryaCommand::ContextReport => {
                let report = self.context_report().await;
                events.send(TuryaEvent::TokenDelta { chunk: report }).await;
                true
            }
            TuryaCommand::ListSessions { cwd, limit } => {
                let sessions = self
                    .list_sessions(cwd.as_deref(), limit.unwrap_or(20))
                    .await;
                events.send(TuryaEvent::SessionsListed { sessions }).await;
                true
            }
            TuryaCommand::ResumeSession { id } => {
                match self.resume_session(id).await {
                    Ok(ev) => events.send(ev).await,
                    Err(e) => events.send(TuryaEvent::Error { message: e }).await,
                }
                true
            }
            TuryaCommand::ListProviders => {
                // Auth gate: metadata refresh only when at least one provider
                // is authenticated — locked setups never phone home.
                let any_authed = self
                    .registry
                    .ids()
                    .iter()
                    .any(|id| self.is_authenticated(id));
                let meta_doc = if any_authed {
                    let meta_url = turya_catalog::metadata_url();
                    Self::fetch_meta_doc(&meta_url).await.ok()
                } else {
                    None
                };
                let mut providers = Vec::new();
                for id in self.registry.ids() {
                    let plugin = match self.registry.get(&id) {
                        Some(p) => p,
                        None => continue,
                    };
                    let authed = self.is_authenticated(&id);
                    let live = if authed {
                        self.live_ids(&id).await
                    } else {
                        vec![]
                    };
                    let models = self.catalog.ensure_loaded(
                        &id,
                        authed,
                        || live,
                        |_| meta_doc.clone().ok_or_else(|| "no metadata".to_string()),
                    );
                    let (api_key, oauth) = self.auth_status_words(&id);
                    providers.push(ProviderSummary {
                        id: id.clone(),
                        display_name: plugin.display_name().to_string(),
                        models: models
                            .into_iter()
                            .map(|m| ModelSummary {
                                id: m.id,
                                display_name: m.display_name,
                                source: match m.source {
                                    turya_catalog::ModelSource::Live => "live".to_string(),
                                    turya_catalog::ModelSource::Cached => "cached".to_string(),
                                    turya_catalog::ModelSource::Snapshot => "snapshot".to_string(),
                                    turya_catalog::ModelSource::Static => "static".to_string(),
                                },
                            })
                            .collect(),
                        api_key,
                        oauth,
                    });
                }
                events.send(TuryaEvent::ProvidersListed { providers }).await;
                true
            }
            TuryaCommand::GetAuthStatus { provider } => {
                let (api_key, oauth) = self.auth_status_words(provider);
                events
                    .send(TuryaEvent::AuthStatusChanged {
                        provider: provider.clone(),
                        api_key,
                        oauth,
                    })
                    .await;
                true
            }
            TuryaCommand::GetProviderState => {
                let (provider, model, via) = self.describe_selection();
                events
                    .send(TuryaEvent::ProviderState {
                        provider,
                        model,
                        via,
                    })
                    .await;
                true
            }
            TuryaCommand::BeginAuthFlow { provider, method } => {
                self.begin_flow(provider.clone(), method.clone(), events)
                    .await;
                true
            }
            TuryaCommand::SubmitAuthInput { flow_id, payload } => {
                self.submit_input(flow_id.clone(), payload.clone(), events)
                    .await;
                true
            }
            TuryaCommand::CancelAuthFlow { flow_id } => {
                self.cancel_flow(flow_id, events, "cancelled").await;
                true
            }
            TuryaCommand::UpdateConfig {
                permission_mode: _,
                provider,
                model,
                max_steps,
                max_tool_calls,
            } => {
                // Budgets apply even when no provider/model switch rides
                // along. Infallible (engine clamps): the client echoes
                // optimistically, like the model-switch toast.
                if max_steps.is_some() || max_tool_calls.is_some() {
                    self.engine.set_budgets(*max_steps, *max_tool_calls);
                }
                // Engine-bound switching; permission_mode passes through to session.
                if provider.is_none() && model.is_none() {
                    return false;
                }
                self.switch_model(provider.clone(), model.clone(), events)
                    .await;
                true
            }
            _ => false,
        }
    }

    /// Resolve a model's window from the catalog and hand it to the engine.
    /// Unknown stays unknown: the engine keeps its generous default rather
    /// than compacting against a guessed number.
    async fn apply_context_budget(&self, provider: &str, model: &str) {
        // Catalog facts first (they carry the window); the plugin's static
        // list is the fallback for the models it knows about.
        let context_window = self
            .catalog
            .cached_models(provider)
            .into_iter()
            .find(|m| m.id == model)
            .and_then(|m| m.context_window);
        if let Some(ctx) = context_window {
            // Reserve a slice for the reply. Bigger windows get a bigger
            // reserve, because one turn can produce proportionally more.
            let reserve = (ctx / 10).clamp(8_000, 64_000) as u32;
            self.engine
                .set_context_budget(ctx.min(u32::MAX as u64) as u32, reserve);
        }
    }

    /// Session listing, scoped to a working directory when asked. The store
    /// is the only source; the kernel holds no session index.
    async fn list_sessions(
        &self,
        cwd: Option<&str>,
        limit: usize,
    ) -> Vec<turya_protocol::SessionMeta> {
        let Ok(store) = self.session_store() else {
            return Vec::new();
        };
        // Fully qualified: the store also has a synchronous inherent method
        // of the same name, and the trait one is the async seam.
        use turya_core::MemoryHook;
        MemoryHook::list_sessions(store.as_ref(), cwd, limit)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Turya: cannot list sessions: {e}");
                Vec::new()
            })
    }

    /// Replay one session into the client.
    async fn resume_session(&self, id: &str) -> Result<TuryaEvent, String> {
        let store = self.session_store()?;
        use turya_core::MemoryHook;
        let session = MemoryHook::session_meta(store.as_ref(), id)
            .await?
            .ok_or_else(|| format!("no session with id '{id}' (try /sessions)"))?;
        let turns = MemoryHook::load_transcript(store.as_ref(), id).await?;
        Ok(TuryaEvent::SessionResumed {
            session: Box::new(session),
            transcript: turya_protocol::Transcript {
                session_id: id.to_string(),
                turns,
            },
        })
    }

    /// Open the session store for host-side session work.
    fn session_store(&self) -> Result<Arc<turya_memory::MemoryStore>, String> {
        turya_memory::MemoryStore::open(&self.db_path)
            .map(|(store, note)| {
                if let Some(note) = note {
                    eprintln!("Turya: {note}");
                }
                Arc::new(store)
            })
            .map_err(|e| format!("cannot open the session store: {e}"))
    }

    /// Discovery problems, kept alongside the provider so `/skills` can show
    /// that a skill exists but did not load.
    fn skill_warnings(&self) -> Vec<String> {
        self.skills_warnings.clone()
    }

    /// The provider/model the session is actually using.
    fn current_selection(&self) -> (String, String) {
        let cfg = HostConfig::load(&self.config_path);
        (
            cfg.provider.unwrap_or_else(|| "anthropic".to_string()),
            cfg.model.unwrap_or_default(),
        )
    }

    /// Cached catalog facts for a model, or empty facts when unknown.
    fn cached_reasoning(&self, provider: &str, model: &str) -> turya_catalog::ModelFacts {
        self.catalog
            .cached_models(provider)
            .into_iter()
            .find(|m| m.id == model)
            .map(|m| turya_catalog::ModelFacts {
                context: m.context_window,
                input: m.input_limit,
                output: m.output_limit,
                tool_call: m.supports_tools,
                attachment: m.supports_attachments,
                reasoning: m.reasoning,
                input_modalities: m.input_modalities,
            })
            .unwrap_or_default()
    }

    /// The selected effort, persisted next to the other settings.
    fn effort(&self) -> Option<String> {
        let (cfg, _) = crate::config::TuryaConfig::load(&self.config_path);
        cfg.effort.clone()
    }

    fn set_effort(&self, effort: Option<String>) {
        let (mut cfg, _) = crate::config::TuryaConfig::load(&self.config_path);
        cfg.effort = effort;
        let _ = cfg.save(&self.config_path);
    }

    /// Human-readable context breakdown for `/context`. Every number is an
    /// estimate and says so; provider-reported usage is authoritative when
    /// the provider gives us any.
    async fn context_report(&self) -> String {
        let budget = self.engine.context_budget();
        let stored = self.engine.stored_transcript().await;
        let tokens = turya_core::context::estimate_transcript_tokens(&stored);
        let turns = stored.turns.len();
        let usable = budget.usable();
        let pct = if usable == 0 {
            0
        } else {
            tokens
                .saturating_mul(100)
                .checked_div(usable)
                .unwrap_or(0)
                .min(999)
        };
        format!(
            "context ≈{tokens} of {usable} usable tokens ({pct}%) across {turns} turn(s)\n\
             model window {} tokens, reserve {} for the reply\n\
             estimates are ~4 chars/token (lower bound); CJK and images count more",
            budget.total, budget.reserve
        )
    }

    async fn switch_model(
        &self,
        provider: Option<String>,
        model: Option<String>,
        events: &HostEventSink,
    ) {
        // Resolve current selection from config when partially specified.
        let cfg = HostConfig::load(&self.config_path);
        let id = provider
            .or(cfg.provider)
            .unwrap_or_else(|| "anthropic".to_string());
        let plugin = match self.registry.get(&id) {
            Some(p) => p,
            None => {
                events
                    .send(TuryaEvent::Error {
                        message: format!("unknown provider '{id}'"),
                    })
                    .await;
                return;
            }
        };
        let model = model.or(cfg.model).unwrap_or_else(|| {
            plugin
                .models()
                .first()
                .map(|m| m.id.clone())
                .unwrap_or_default()
        });
        // Validate against the same sources the /models browser shows:
        // static list (offline, instant) → disk cache (offline) → live
        // discovery (network). Rejecting live-known models here was a bug:
        // the picker offered them but the switch refused.
        if !plugin.models().iter().any(|m| m.id == model)
            && !self.catalog.cached_ids(&id).iter().any(|m| m == &model)
            && !self.live_ids(&id).await.iter().any(|m| m == &model)
        {
            events
                .send(TuryaEvent::Error {
                    message: format!("unknown model '{model}' for provider '{id}'"),
                })
                .await;
            return;
        }
        let creds = match self.resolve_for(&id).await {
            Ok(c) => c,
            Err(e) => {
                events
                    .send(TuryaEvent::Error {
                        message: format!("cannot use {id}: {e} (run /auth)"),
                    })
                    .await;
                return;
            }
        };
        // Hand the loop the new model's token budget as two plain integers.
        // The kernel never learns a model id or a catalog concept (Rule 3.1),
        // but it does need to know when a switch shrank the window.
        self.apply_context_budget(&id, &model).await;
        match plugin.connect(creds, &model) {
            Ok(p) => {
                self.engine.set_provider(p);
                HostConfig {
                    provider: Some(id.clone()),
                    model: Some(model.clone()),
                }
                .save(&self.config_path);
                // Push truth (not optimism): clients correct their labels here.
                let (_, _, via) = self.describe_selection();
                events
                    .send(TuryaEvent::ProviderState {
                        provider: id,
                        model,
                        via,
                    })
                    .await;
            }
            Err(e) => {
                events
                    .send(TuryaEvent::Error {
                        message: format!("cannot connect {id}: {e}"),
                    })
                    .await;
            }
        }
    }

    /// Current selection + credential source, without touching the network
    /// (no refresh, no listing). Answers `GetProviderState`.
    fn describe_selection(&self) -> (String, String, String) {
        let cfg = HostConfig::load(&self.config_path);
        let id = cfg.provider.unwrap_or_else(|| "anthropic".to_string());
        let model = cfg.model.unwrap_or_else(|| {
            self.registry
                .get(&id)
                .and_then(|p| p.models().first().map(|m| m.id.clone()))
                .unwrap_or_default()
        });
        (id.clone(), model, self.credential_via(&id))
    }

    /// Credential provenance without resolving (resolving may refresh over
    /// the network — too heavy for a status query).
    fn credential_via(&self, provider: &str) -> String {
        let plugin = match self.registry.get(provider) {
            Some(p) => p,
            None => return "mock".to_string(),
        };
        let env_hit = plugin.auth_methods().iter().any(|m| match m {
            turya_core::AuthMethodKind::ApiKey { env_var } => std::env::var(env_var)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            _ => false,
        });
        if env_hit {
            return "env".to_string();
        }
        if self
            .store
            .get(&turya_auth::api_key_account(provider))
            .is_some()
        {
            return "stored-key".to_string();
        }
        if self
            .store
            .get(&turya_auth::oauth_refresh_account(provider))
            .is_some()
        {
            return "oauth".to_string();
        }
        "mock".to_string()
    }

    async fn begin_flow(&self, provider: String, method: String, events: &HostEventSink) {
        if self.registry.get(&provider).is_none() {
            events
                .send(TuryaEvent::AuthFlowFailed {
                    flow_id: String::new(),
                    reason: format!("unknown provider '{provider}'"),
                })
                .await;
            return;
        }
        if method == "api-key" {
            let flow_id = self.next_flow_id();
            self.flows.lock().unwrap().insert(
                flow_id.clone(),
                PendingFlow {
                    provider,
                    method,
                    verifier: String::new(),
                    redirect_uri: String::new(),
                    task: None,
                },
            );
            events
                .send(TuryaEvent::AuthFlowStarted {
                    flow_id,
                    action: AuthAction::PromptMasked {
                        prompt: "API key".to_string(),
                    },
                })
                .await;
            return;
        }
        if method == "oauth" {
            let client_id = std::env::var("TURYA_OAUTH_CLIENT_ID").ok().or_else(|| {
                self.store
                    .get(&turya_auth::oauth_client_id_account(&provider))
            });
            let client_id = match client_id {
                Some(id) => id,
                None => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id: String::new(),
                            reason: "OAuth needs your Google Cloud client ID (consumer accounts unsupported)".to_string(),
                        })
                        .await;
                    return;
                }
            };
            let cfg = turya_auth::OAuthConfig::google(&client_id);
            let (verifier, challenge) = match turya_auth::oauth::pkce_pair() {
                Ok(p) => p,
                Err(e) => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id: String::new(),
                            reason: e.to_string(),
                        })
                        .await;
                    return;
                }
            };
            // Bind now so the redirect URI is known before emitting the URL.
            let probe = match tokio::net::TcpListener::bind(("127.0.0.1", cfg.redirect_port)).await
            {
                Ok(l) => l,
                Err(e) => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id: String::new(),
                            reason: format!("cannot bind localhost callback: {e}"),
                        })
                        .await;
                    return;
                }
            };
            let port = match probe.local_addr() {
                Ok(a) => a.port(),
                Err(e) => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id: String::new(),
                            reason: e.to_string(),
                        })
                        .await;
                    return;
                }
            };
            drop(probe);
            let redirect = turya_auth::oauth::redirect_uri(port);
            let state = format!("turya-{}", std::process::id());
            let url = match turya_auth::oauth::build_auth_url(&cfg, &redirect, &challenge, &state) {
                Ok(u) => u,
                Err(e) => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id: String::new(),
                            reason: e.to_string(),
                        })
                        .await;
                    return;
                }
            };
            let flow_id = self.next_flow_id();
            // Background wait: browser callback completes the flow; pasted
            // codes arrive via SubmitAuthInput which aborts this task.
            let store_clone = self.store.clone();
            let provider_clone = provider.clone();
            let client_id_clone = client_id.clone();
            let redirect_clone = redirect.clone();
            let verifier_clone = verifier.clone();
            let events_clone = events.clone();
            let flow_id_clone = flow_id.clone();
            let task = tokio::spawn(async move {
                match turya_auth::oauth::wait_for_callback(
                    port,
                    &state,
                    std::time::Duration::from_secs(300),
                )
                .await
                {
                    Ok((code, _)) => {
                        let cfg = turya_auth::OAuthConfig::google(&client_id_clone);
                        match turya_auth::oauth::exchange_code(
                            &cfg,
                            &redirect_clone,
                            code.trim(),
                            &verifier_clone,
                        )
                        .await
                        {
                            Ok(tokens) => {
                                if let Some(refresh) = tokens.refresh_token {
                                    let _ = store_clone.set(
                                        &turya_auth::oauth_refresh_account(&provider_clone),
                                        &refresh,
                                    );
                                    let _ = store_clone.set(
                                        &turya_auth::oauth_client_id_account(&provider_clone),
                                        &client_id_clone,
                                    );
                                    events_clone
                                        .send(TuryaEvent::AuthFlowCompleted {
                                            flow_id: flow_id_clone,
                                            provider: provider_clone,
                                            method: "oauth".to_string(),
                                        })
                                        .await;
                                } else {
                                    events_clone
                                        .send(TuryaEvent::AuthFlowFailed {
                                            flow_id: flow_id_clone,
                                            reason: "no refresh token returned".to_string(),
                                        })
                                        .await;
                                }
                            }
                            Err(e) => {
                                events_clone
                                    .send(TuryaEvent::AuthFlowFailed {
                                        flow_id: flow_id_clone,
                                        reason: e.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Err(_) => {
                        // Timeout or bind race: stay silent unless the flow
                        // is still pending (cancel already reported).
                    }
                }
            });
            self.flows.lock().unwrap().insert(
                flow_id.clone(),
                PendingFlow {
                    provider,
                    method,
                    verifier,
                    redirect_uri: redirect,
                    task: Some(task),
                },
            );
            events
                .send(TuryaEvent::AuthFlowStarted {
                    flow_id,
                    action: AuthAction::OpenBrowser { url },
                })
                .await;
            return;
        }
        events
            .send(TuryaEvent::AuthFlowFailed {
                flow_id: String::new(),
                reason: format!("unknown method '{method}': expected api-key|oauth"),
            })
            .await;
    }

    async fn submit_input(&self, flow_id: String, payload: String, events: &HostEventSink) {
        // Bind the Option first: match-scrutinee temporaries (the guard)
        // live until the match ends, which would cross awaits below.
        let flow: Option<PendingFlow> = self.flows.lock().unwrap().remove(&flow_id);
        let flow = match flow {
            Some(f) => f,
            None => {
                events
                    .send(TuryaEvent::AuthFlowFailed {
                        flow_id,
                        reason: "unknown or expired flow".to_string(),
                    })
                    .await;
                return;
            }
        };
        if let Some(task) = flow.task {
            task.abort();
        }
        if flow.method == "api-key" {
            let key = payload.trim().to_string();
            if key.is_empty() {
                events
                    .send(TuryaEvent::AuthFlowFailed {
                        flow_id,
                        reason: "empty key — nothing stored".to_string(),
                    })
                    .await;
                return;
            }
            let plugin = match self.registry.get(&flow.provider) {
                Some(p) => p,
                None => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id,
                            reason: "unknown provider".to_string(),
                        })
                        .await;
                    return;
                }
            };
            let creds = turya_core::ResolvedCreds {
                token: key.clone(),
                expires_at: None,
                via: "login",
            };
            if plugin.list_models(&creds).await.is_empty() {
                events
                    .send(TuryaEvent::AuthFlowFailed {
                        flow_id,
                        reason: "key rejected (no models listed) — not stored".to_string(),
                    })
                    .await;
                return;
            }
            match self
                .store
                .set(&turya_auth::api_key_account(&flow.provider), &key)
            {
                Ok(()) => {
                    let (api_key, oauth) = self.auth_status_words(&flow.provider);
                    events
                        .send(TuryaEvent::AuthFlowCompleted {
                            flow_id,
                            provider: flow.provider.clone(),
                            method: "api-key".to_string(),
                        })
                        .await;
                    events
                        .send(TuryaEvent::AuthStatusChanged {
                            provider: flow.provider,
                            api_key,
                            oauth,
                        })
                        .await;
                }
                Err(e) => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id,
                            reason: e.to_string(),
                        })
                        .await;
                }
            }
            return;
        }
        // OAuth pasted-code path (browser wait was aborted above).
        let code = extract_pasted_code(&payload);
        if code.is_empty() {
            events
                .send(TuryaEvent::AuthFlowFailed {
                    flow_id,
                    reason: "empty input — login cancelled".to_string(),
                })
                .await;
            return;
        }
        let client_id = match std::env::var("TURYA_OAUTH_CLIENT_ID").ok().or_else(|| {
            self.store
                .get(&turya_auth::oauth_client_id_account(&flow.provider))
        }) {
            Some(id) => id,
            None => {
                events
                    .send(TuryaEvent::AuthFlowFailed {
                        flow_id,
                        reason: "missing OAuth client id".to_string(),
                    })
                    .await;
                return;
            }
        };
        let cfg = turya_auth::OAuthConfig::google(&client_id);
        match turya_auth::oauth::exchange_code(&cfg, &flow.redirect_uri, &code, &flow.verifier)
            .await
        {
            Ok(tokens) => match tokens.refresh_token {
                Some(refresh) => {
                    let _ = self
                        .store
                        .set(&turya_auth::oauth_refresh_account(&flow.provider), &refresh);
                    let _ = self.store.set(
                        &turya_auth::oauth_client_id_account(&flow.provider),
                        &client_id,
                    );
                    events
                        .send(TuryaEvent::AuthFlowCompleted {
                            flow_id,
                            provider: flow.provider,
                            method: "oauth".to_string(),
                        })
                        .await;
                }
                None => {
                    events
                        .send(TuryaEvent::AuthFlowFailed {
                            flow_id,
                            reason: "no refresh token returned".to_string(),
                        })
                        .await;
                }
            },
            Err(e) => {
                events
                    .send(TuryaEvent::AuthFlowFailed {
                        flow_id,
                        reason: e.to_string(),
                    })
                    .await;
            }
        }
    }

    async fn cancel_flow(&self, flow_id: &str, events: &HostEventSink, reason: &str) {
        let flow = {
            // Scope the lock: guards must never cross an await (Send).
            self.flows.lock().unwrap().remove(flow_id)
        };
        if let Some(flow) = flow {
            if let Some(task) = flow.task {
                task.abort();
            }
            events
                .send(TuryaEvent::AuthFlowFailed {
                    flow_id: flow_id.to_string(),
                    reason: reason.to_string(),
                })
                .await;
        }
    }
}

/// Accept a full callback URL or a bare code (shared with CLI login).
pub fn extract_pasted_code(line: &str) -> String {
    let line = line.trim();
    if let Some((_, q)) = line.split_once('?') {
        for pair in q.split('&') {
            if let Some(code) = pair.strip_prefix("code=") {
                let code = code.split('&').next().unwrap_or("");
                if !code.is_empty() {
                    return code.to_string();
                }
            }
        }
    }
    line.to_string()
}

/// Cloneable event sink for background flow tasks.
#[derive(Clone)]
pub struct HostEventSink {
    tx: mpsc::Sender<TuryaEvent>,
}

impl HostEventSink {
    pub fn new(tx: mpsc::Sender<TuryaEvent>) -> Self {
        Self { tx }
    }

    pub async fn send(&self, event: TuryaEvent) {
        let _ = self.tx.send(event).await;
    }

    /// The channel itself, for engine calls that emit progress events.
    pub fn sender(&self) -> &mpsc::Sender<TuryaEvent> {
        &self.tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turya_auth::MemStore;

    fn harness() -> (
        HostServices,
        mpsc::Sender<TuryaEvent>,
        mpsc::Receiver<TuryaEvent>,
    ) {
        harness_with(&["anthropic"])
    }

    /// Harness with explicit providers, isolated catalog dir + config file.
    fn harness_with(
        providers: &[&str],
    ) -> (
        HostServices,
        mpsc::Sender<TuryaEvent>,
        mpsc::Receiver<TuryaEvent>,
    ) {
        let registry = Arc::new(ProviderRegistry::new());
        for id in providers {
            match *id {
                "anthropic" => {
                    registry.register(Arc::new(turya_provider_anthropic::AnthropicPlugin))
                }
                "gemini" => registry.register(Arc::new(turya_provider_gemini::GeminiPlugin)),
                _ => {}
            }
        }
        let store: Arc<dyn CredentialStore> = Arc::new(MemStore::new());
        let dir = std::env::temp_dir().join("turya-host-router-test");
        let catalog = turya_catalog::Catalog::new(dir);
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let mock: Arc<dyn turya_core::LlmProvider> =
            Arc::new(turya_core::MockProvider { responses: vec![] });
        let engine = Arc::new(TuryaEngine::new(
            mock,
            tools,
            turya_protocol::PermissionMode::Open,
        ));
        let (tx, rx) = mpsc::channel(32);
        let host = HostServices::new(
            registry,
            store,
            catalog,
            engine,
            "/tmp/turya-host-router-test-config.toml".to_string(),
            "/tmp/turya-host-router-test.db".to_string(),
            Vec::new(),
        );
        (host, tx, rx)
    }

    fn drain(rx: &mut mpsc::Receiver<TuryaEvent>) -> Vec<TuryaEvent> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            out.push(evt);
        }
        out
    }

    /// Env vars are process-global: tests that set/remove provider keys and
    /// tests that assert their absence must never run concurrently.
    /// Async-aware mutex: never hold a std guard across awaits.
    static ENV_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn list_providers_without_creds_uses_static() {
        let _guard = ENV_GUARD.lock().await;
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        // No creds configured: router must still answer from static lists
        // (no network: plugin live calls would fail fast to empty).
        let consumed = host.handle(&TuryaCommand::ListProviders, &sink).await;
        assert!(consumed);
        let evts = drain(&mut rx);
        let listed = evts.iter().find_map(|e| match e {
            TuryaEvent::ProvidersListed { providers } => Some(providers),
            _ => None,
        });
        let providers = listed.expect("ProvidersListed event");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "anthropic");
        assert_eq!(providers[0].api_key, "missing");
        assert!(!providers[0].models.is_empty());
    }

    #[tokio::test]
    async fn get_auth_status_reports_slots() {
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        let consumed = host
            .handle(
                &TuryaCommand::GetAuthStatus {
                    provider: "anthropic".to_string(),
                },
                &sink,
            )
            .await;
        assert!(consumed);
        let evts = drain(&mut rx);
        assert!(evts.iter().any(|e| matches!(
            e,
            TuryaEvent::AuthStatusChanged { provider, api_key, .. }
                if provider == "anthropic" && api_key == "missing"
        )));
    }

    #[tokio::test]
    async fn unknown_provider_switch_errors() {
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        let consumed = host
            .handle(
                &TuryaCommand::UpdateConfig {
                    permission_mode: None,
                    provider: Some("nope".to_string()),
                    model: None,
                    max_steps: None,
                    max_tool_calls: None,
                },
                &sink,
            )
            .await;
        assert!(consumed);
        let evts = drain(&mut rx);
        assert!(evts.iter().any(|e| matches!(
            e,
            TuryaEvent::Error { message } if message.contains("unknown provider")
        )));
    }

    #[tokio::test]
    async fn budgets_only_update_applies_without_switch() {
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        // No provider/model: budgets apply, nothing to switch, no error.
        let consumed = host
            .handle(
                &TuryaCommand::UpdateConfig {
                    permission_mode: None,
                    provider: None,
                    model: None,
                    max_steps: Some(12),
                    max_tool_calls: Some(40),
                },
                &sink,
            )
            .await;
        assert!(!consumed);
        let evts = drain(&mut rx);
        assert!(
            !evts.iter().any(|e| matches!(e, TuryaEvent::Error { .. })),
            "budget-only update must be silent: {evts:?}"
        );
    }

    #[tokio::test]
    async fn switch_accepts_cached_live_only_model() {
        let _guard = ENV_GUARD.lock().await;
        // Regression: the /models browser offered gemini-flash-latest (live)
        // but the switch rejected it (static-only validation).
        let saved_env = std::env::var("GEMINI_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        let dir = std::env::temp_dir().join("turya-host-switch-test");
        let _ = std::fs::remove_dir_all(&dir);
        let config_path = dir.join("config.toml");
        let registry = Arc::new(ProviderRegistry::new());
        registry.register(Arc::new(turya_provider_gemini::GeminiPlugin));
        let mem = Arc::new(MemStore::new());
        mem.set("gemini-api-key", "fake-key-for-offline-test")
            .unwrap();
        let store: Arc<dyn CredentialStore> = mem;
        let catalog = turya_catalog::Catalog::new(dir.join("catalog"));
        // Seed the disk cache with a live-only model (no network below).
        let _ = catalog.ensure_loaded(
            "gemini",
            true,
            || vec!["gemini-flash-latest".to_string()],
            |_| Ok("{}".to_string()),
        );
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let mock: Arc<dyn turya_core::LlmProvider> =
            Arc::new(turya_core::MockProvider { responses: vec![] });
        let engine = Arc::new(TuryaEngine::new(
            mock,
            tools,
            turya_protocol::PermissionMode::Open,
        ));
        let (tx, mut rx) = mpsc::channel(32);
        let host = HostServices::new(
            registry,
            store,
            catalog,
            engine,
            config_path.to_string_lossy().to_string(),
            config_path
                .with_extension("db")
                .to_string_lossy()
                .to_string(),
            Vec::new(),
        );
        let sink = HostEventSink::new(tx);
        let consumed = host
            .handle(
                &TuryaCommand::UpdateConfig {
                    permission_mode: None,
                    provider: Some("gemini".to_string()),
                    model: Some("gemini-flash-latest".to_string()),
                    max_steps: None,
                    max_tool_calls: None,
                },
                &sink,
            )
            .await;
        assert!(consumed);
        let evts = drain(&mut rx);
        assert!(
            !evts.iter().any(|e| matches!(e, TuryaEvent::Error { .. })),
            "unexpected error: {evts:?}"
        );
        // Selection persisted for next launch.
        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("gemini-flash-latest"));
        if let Some(v) = saved_env {
            std::env::set_var("GEMINI_API_KEY", v);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn begin_api_key_flow_prompts() {
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        let consumed = host
            .handle(
                &TuryaCommand::BeginAuthFlow {
                    provider: "anthropic".to_string(),
                    method: "api-key".to_string(),
                },
                &sink,
            )
            .await;
        assert!(consumed);
        let evts = drain(&mut rx);
        assert!(evts.iter().any(|e| matches!(
            e,
            TuryaEvent::AuthFlowStarted {
                action: AuthAction::PromptMasked { .. },
                ..
            }
        )));
    }

    #[tokio::test]
    async fn unknown_method_fails_fast() {
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        let consumed = host
            .handle(
                &TuryaCommand::BeginAuthFlow {
                    provider: "anthropic".to_string(),
                    method: "oauth".to_string(),
                },
                &sink,
            )
            .await;
        assert!(consumed);
        // Anthropic offers no OAuth: fails without touching the network.
        let evts = drain(&mut rx);
        assert!(evts
            .iter()
            .any(|e| matches!(e, TuryaEvent::AuthFlowFailed { .. })));
    }

    #[tokio::test]
    async fn passthrough_commands_are_not_consumed() {
        let (host, tx, _rx) = harness();
        let sink = HostEventSink::new(tx);
        assert!(
            !host
                .handle(
                    &TuryaCommand::SubmitPrompt {
                        prompt: "hi".to_string(),
                        mode: turya_protocol::AgentMode::Build,
                        attachments: Vec::new(),
                    },
                    &sink,
                )
                .await
        );
        assert!(!host.handle(&TuryaCommand::AbortTurn, &sink).await);
    }

    #[tokio::test]
    async fn get_provider_state_reports_defaults_offline() {
        // No config file, no creds: defaults without touching the network.
        let _ = std::fs::remove_file("/tmp/turya-host-router-test-config.toml");
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        assert!(host.handle(&TuryaCommand::GetProviderState, &sink).await);
        let evts = drain(&mut rx);
        let state = evts.iter().find_map(|e| match e {
            TuryaEvent::ProviderState {
                provider,
                model,
                via,
            } => Some((provider.clone(), model.clone(), via.clone())),
            _ => None,
        });
        let (provider, model, via) = state.expect("ProviderState event");
        assert_eq!(provider, "anthropic");
        assert!(!model.is_empty());
        assert_eq!(via, "mock");
    }

    #[tokio::test]
    async fn successful_switch_pushes_provider_state() {
        let _guard = ENV_GUARD.lock().await;
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::set_var("ANTHROPIC_API_KEY", "fake-key-for-offline-test");
        let _ = std::fs::remove_file("/tmp/turya-host-router-test-config.toml");
        let (host, tx, mut rx) = harness();
        let sink = HostEventSink::new(tx);
        assert!(
            host.handle(
                &TuryaCommand::UpdateConfig {
                    permission_mode: None,
                    provider: Some("anthropic".to_string()),
                    model: Some("claude-sonnet-4-5".to_string()),
                    max_steps: None,
                    max_tool_calls: None,
                },
                &sink,
            )
            .await
        );
        let evts = drain(&mut rx);
        assert!(
            !evts.iter().any(|e| matches!(e, TuryaEvent::Error { .. })),
            "unexpected error: {evts:?}"
        );
        let state = evts.iter().find_map(|e| match e {
            TuryaEvent::ProviderState {
                provider,
                model,
                via,
            } => Some((provider.clone(), model.clone(), via.clone())),
            _ => None,
        });
        assert_eq!(
            state,
            Some((
                "anthropic".to_string(),
                "claude-sonnet-4-5".to_string(),
                "env".to_string()
            ))
        );
        match saved {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
    }

    #[test]
    fn host_config_roundtrip() {
        let path = "/tmp/turya-host-router-test-config.toml";
        let _ = std::fs::remove_file(path);
        HostConfig {
            provider: Some("gemini".to_string()),
            model: Some("gemini-2.5-flash".to_string()),
        }
        .save(path);
        let back = HostConfig::load(path);
        assert_eq!(back.provider.as_deref(), Some("gemini"));
        assert_eq!(back.model.as_deref(), Some("gemini-2.5-flash"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extract_pasted_code_cases() {
        assert_eq!(
            extract_pasted_code("http://x/callback?code=ABC&state=s"),
            "ABC"
        );
        assert_eq!(extract_pasted_code("  XYZ  "), "XYZ");
        assert_eq!(extract_pasted_code(""), "");
    }
}
