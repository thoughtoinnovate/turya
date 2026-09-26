use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::mpsc;
use turya_cli::{auth_cmd, config, host_services, update};
use turya_core::{LlmProvider, MockProvider, ProviderStep, TuryaEngine};
use turya_mcp::McpToolHandle;
use turya_protocol::{PermissionMode, ToolCall};
use turya_server::TuryaSession;
use turya_tools::ToolRegistry;
use turya_tui::TuiApp;

#[derive(Parser, Debug)]
#[command(
    name = "turya",
    version,
    about = "Fast, modular agentic coding harness"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// open | review-for-me | manual. Unset means "use the saved setting".
    #[arg(short, long)]
    permission_mode: Option<String>,
    #[arg(long)]
    model: Option<String>,
    /// Provider id (registry-driven; default: anthropic).
    #[arg(long)]
    provider: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Update to the latest patch/minor release (never crosses majors).
    Update {
        /// Install a specific version (e.g. --version v0.1.2).
        #[arg(long)]
        version: Option<String>,
        /// Reinstall even when already up to date.
        #[arg(long)]
        force: bool,
        /// Report latest vs installed versions, change nothing.
        #[arg(long)]
        check: bool,
    },
    /// Upgrade across major versions (confirms breaking-change risk).
    Upgrade {
        /// Install a specific version (e.g. --version v1.0.0).
        #[arg(long)]
        version: Option<String>,
        /// Reinstall even when already up to date.
        #[arg(long)]
        force: bool,
        /// Report latest vs installed versions, change nothing.
        #[arg(long)]
        check: bool,
        /// Skip the major-version confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Authenticate providers (API key or OAuth into the OS keychain).
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List stored sessions (newest first).
    Sessions {
        /// Show sessions from every directory, not just this one.
        #[arg(long)]
        all: bool,
    },
    /// Continue a stored session (its conversation is replayed into the TUI).
    Resume {
        /// Session id (see `turya sessions`).
        id: String,
    },
    /// Export one session as newline-delimited JSON.
    Export {
        /// Session id (see `turya sessions`).
        id: String,
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AuthAction {
    /// Log in to a provider (default method: api-key).
    Login {
        /// Provider id (e.g. gemini, anthropic).
        provider: String,
        /// Auth method: api-key | oauth.
        #[arg(long)]
        method: Option<String>,
        /// OAuth client id (or TURYA_OAUTH_CLIENT_ID).
        #[arg(long)]
        client_id: Option<String>,
    },
    /// Forget stored credentials (default: both methods).
    Logout {
        /// Provider id.
        provider: String,
        /// Only forget this method: api-key | oauth.
        #[arg(long)]
        method: Option<String>,
    },
    /// Show dual-slot auth state for all registered providers.
    Status,
}

fn parse_permission_mode(raw: &str) -> PermissionMode {
    match raw.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "open" => PermissionMode::Open,
        "manual" => PermissionMode::Manual,
        _ => PermissionMode::ReviewForMe,
    }
}

/// Fresh session id: UTC timestamp + pid. Sortable, unique enough for one
/// machine, and readable in `turya sessions`.
fn new_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("s-{now}-{}", std::process::id())
}

fn open_store(turya_home: &str) -> Option<Arc<turya_memory::MemoryStore>> {
    let db_path = format!("{turya_home}/turya.db");
    if let Err(e) = std::fs::create_dir_all(turya_home) {
        eprintln!("Turya: cannot create {turya_home}: {e}");
        return None;
    }
    match turya_memory::MemoryStore::open(&db_path) {
        // Rule 5.3: a foreign format is reported loudly, never migrated.
        Ok((store, note)) => {
            if let Some(note) = note {
                eprintln!("Turya: {note}");
            }
            Some(Arc::new(store))
        }
        Err(e) => {
            eprintln!("Turya: memory disabled ({db_path}: {e})");
            None
        }
    }
}

fn print_sessions(store: &turya_memory::MemoryStore, cwd: Option<&str>) {
    match store.list_sessions(cwd, 50) {
        Ok(sessions) if sessions.is_empty() => {
            println!("No sessions yet.");
        }
        Ok(sessions) => {
            println!("{:<22} {:<7} {:<19} TITLE", "SESSION", "TURNS", "UPDATED");
            for s in sessions {
                let mark = if s.repaired { " *" } else { "" };
                println!(
                    "{:<22} {:<7} {:<19} {}{}",
                    s.id, s.seq, s.updated_at, s.title, mark
                );
            }
            println!("\n* closed by crash repair; resume to continue it.");
        }
        Err(e) => eprintln!("Turya: cannot list sessions: {e}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Durable settings: file first, then flags/env override it. An
    // incompatible file is replaced and reported, never migrated.
    let (settings, load_outcome) = config::TuryaConfig::load(config_path());
    match load_outcome {
        Some(config::LoadOutcome::Replaced { found, backup }) => eprintln!(
            "Turya: config.toml was schema v{found}: kept as {backup} and reset to defaults \
             (forward-only, no migration)."
        ),
        Some(config::LoadOutcome::ReplacedUnparsable { reason, backup }) => eprintln!(
            "Turya: config.toml was unreadable ({reason}); kept as {backup} and reset to defaults."
        ),
        _ => {}
    }

    // Self-management subcommands never touch the agent engine.
    // Auth shares the turya-auth functions the TUI /auth flow will call.
    let store: std::sync::Arc<dyn turya_auth::CredentialStore> =
        std::sync::Arc::new(auth_cmd::default_store());
    let registry = std::sync::Arc::new(build_provider_registry());
    match args.command {
        Some(Command::Update {
            version,
            force,
            check,
        }) => {
            return update::run_self_update(version.as_deref(), force, check)
                .await
                .map_err(|e| e.into());
        }
        Some(Command::Upgrade {
            version,
            force,
            check,
            yes,
        }) => {
            return update::run_upgrade(version.as_deref(), force, check, yes)
                .await
                .map_err(|e| e.into());
        }
        Some(Command::Sessions { all }) => {
            let home = turya_home_dir();
            let store = open_store(&home).ok_or("cannot open the session store")?;
            let cwd = if all {
                None
            } else {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            };
            print_sessions(&store, cwd.as_deref());
            return Ok(());
        }
        Some(Command::Export { id, out }) => {
            let home = turya_home_dir();
            let store = open_store(&home).ok_or("cannot open the session store")?;
            let jsonl = store
                .export_jsonl(&id)
                .map_err(|e| format!("cannot export session {id}: {e}"))?;
            match out {
                Some(path) => {
                    std::fs::write(&path, jsonl)
                        .map_err(|e| format!("cannot write {path}: {e}"))?;
                    println!("Wrote {path}");
                }
                None => print!("{jsonl}"),
            }
            return Ok(());
        }
        Some(Command::Auth { action }) => {
            return match action {
                AuthAction::Login {
                    provider,
                    method,
                    client_id,
                } => auth_cmd::login(
                    &registry,
                    &provider,
                    method.as_deref(),
                    client_id.as_deref(),
                    store.as_ref(),
                )
                .await
                .map_err(|e| e.into()),
                AuthAction::Logout { provider, method } => {
                    auth_cmd::logout(&provider, method.as_deref(), store.as_ref())
                        .map_err(|e| e.into())
                }
                AuthAction::Status => {
                    auth_cmd::status(&registry, store.as_ref()).map_err(|e| e.into())
                }
            };
        }
        // `Resume` deliberately falls through: it needs the full engine, but
        // with the stored session's id and its transcript replayed below.
        _ => {}
    }

    // Settings supply defaults; an explicit flag still wins.
    if let Some(p) = &settings.provider {
        if args.provider.is_none() && std::env::var_os("TURYA_PROVIDER").is_none() {
            std::env::set_var("TURYA_PROVIDER", p);
        }
    }
    if let Some(m) = &settings.model {
        if args.model.is_none() && std::env::var_os("TURYA_MODEL").is_none() {
            std::env::set_var("TURYA_MODEL", m);
        }
    }
    // A local daemon is often not on the default port, or on another host.
    // The plugin reads OLLAMA_HOST itself; an explicit environment variable
    // still wins, exactly as for provider and model above.
    if let Some(h) = &settings.ollama_host {
        if std::env::var_os("OLLAMA_HOST").is_none() {
            std::env::set_var("OLLAMA_HOST", h);
        }
    }
    if let Some(model) = args.model.clone() {
        // Export for provider defaults; CLI flag wins over env.
        std::env::set_var("TURYA_MODEL", model);
    }
    let permission_mode = parse_permission_mode(
        args.permission_mode
            .as_deref()
            .or(settings.permission_mode.as_deref())
            .unwrap_or("review-for-me"),
    );

    // Registry-driven bootstrap (Rule 3.2: host links plugins, core names
    // no vendor). Mock fallback keeps sim/offline working.
    let provider: std::sync::Arc<dyn LlmProvider> =
        select_provider(&registry, args.provider.as_deref(), store.as_ref()).await;

    let mut tools = ToolRegistry::standard();

    // MCP: connect the configured servers and fold their tools into the same
    // registry the model already uses, so an MCP tool and a builtin go
    // through one lookup path and one permission broker. Each server is
    // independent — one that fails to start costs its own tools and nothing
    // else, and says so rather than vanishing.
    let mut mcp = turya_mcp::McpRegistry::empty();
    for server in settings.mcp_servers.clone().unwrap_or_default() {
        match mcp.connect(&server.name, &server.command, &server.args) {
            Ok(tools) if tools.is_empty() => {
                eprintln!(
                    "Turya: MCP '{}' connected but offered no tools",
                    server.name
                );
            }
            Ok(names) => {
                for t in mcp.tools_for(&server.name) {
                    tools.register(Box::new(McpToolHandle(t)));
                }
                eprintln!("Turya: MCP '{}' added {} tool(s)", server.name, names.len());
            }
            Err(e) => eprintln!("Turya: MCP '{}': {e}", server.name),
        }
    }
    let mcp = Arc::new(std::sync::Mutex::new(mcp));
    let tools = Arc::new(tools);
    let mut engine = TuryaEngine::new(provider, tools, permission_mode);

    // Step 8: episodic memory at $TURYA_HOME or ~/.turya/turya.db (best-effort).
    // Rule 3.1: the host injects the plugin adapter through the kernel seam;
    // the engine never touches SQLite directly.
    let turya_home = turya_home_dir();
    let session_store = open_store(&turya_home);
    let session_id = match &args.command {
        Some(Command::Resume { id }) => id.clone(),
        _ => new_session_id(),
    };
    if let Some(store) = &session_store {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if let Err(e) = <turya_memory::MemoryStore as turya_core::MemoryHook>::begin_session(
            store,
            &session_id,
            &cwd,
            "",
        )
        .await
        {
            eprintln!("Turya: cannot open session: {e}");
        }
    }
    if let Some(store) = session_store {
        engine = engine.with_memory_hook(store).with_session_id(&session_id);
    }

    // Agent Skills: discovered from .agents/skills and any configured paths.
    // Internal plugin, injected through the kernel seam.
    let extra = settings.skills_paths.clone().unwrap_or_default();
    let skills = turya_skills::FileSkillProvider::new(&extra);
    for w in skills.warnings() {
        eprintln!("Turya: skills: {w}");
    }
    let found = skills.skills().len();
    if found > 0 {
        eprintln!("Turya: {found} skill(s) available");
    }
    let skill_warnings = skills.warnings();
    engine = engine.with_skills_hook(Arc::new(skills));

    // Step 10: live diagnostics when a language server is on PATH.
    engine = engine.with_diagnostics_hook(Arc::new(turya_lsp::LspDiagnosticsHook::rust_analyzer()));

    let engine = Arc::new(engine);

    // Command flow: TUI → host router → session proxy. The router answers
    // provider/auth/catalog commands locally; everything else forwards.
    // (Rule 3.2: server stays a thin proxy; hosts own service routing.)
    let (ui_cmd_tx, mut ui_cmd_rx) = mpsc::channel(32);
    let (sess_cmd_tx, sess_cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::channel(64);

    let session = TuryaSession::new(engine.clone(), sess_cmd_rx, event_tx.clone());
    tokio::spawn(async move {
        session.run_loop().await;
    });

    let catalog = turya_catalog::Catalog::new(catalog_dir());
    let config_path = config_path();
    // Apply persisted provider/model selection before the first turn.
    let persisted = host_services::HostConfig::load(&config_path);
    if persisted.provider.is_some() || persisted.model.is_some() {
        eprintln!("Turya: restoring saved provider/model selection");
    }
    let host = Arc::new(host_services::HostServices::new(
        registry,
        store,
        catalog,
        engine,
        host_services::HostPaths {
            config: config_path,
            db: format!("{}/turya.db", turya_home_dir()),
        },
        skill_warnings,
        mcp,
    ));
    let host_sink = host_services::HostEventSink::new(event_tx);
    tokio::spawn(async move {
        while let Some(cmd) = ui_cmd_rx.recv().await {
            if !host.handle(&cmd, &host_sink).await {
                let _ = sess_cmd_tx.send(cmd).await;
            }
        }
    });

    let mut app = TuiApp::new_restoring();
    // Presentation settings from the config file: tints are opt-in, and the
    // mouse mode is `auto` unless the user pinned it.
    app.apply_settings(
        Some((
            settings.user_bg.as_deref().unwrap_or("none"),
            settings.assistant_bg.as_deref().unwrap_or("none"),
            settings.tool_bg.as_deref().unwrap_or("none"),
        )),
        settings.mouse.as_deref(),
    );
    // The saved colour mode, resolved the same way the host resolves it, so a
    // restart keeps a `NO_COLOR` decision the user made with `/settings`.
    app.apply_color(settings.no_color);
    // `turya resume <id>`: replay the stored conversation so the session looks
    // exactly as it did before the process exited. The engine also loads it
    // for the model; this is the user-visible half.
    if let Some(Command::Resume { id }) = &args.command {
        match <turya_memory::MemoryStore as turya_core::MemoryHook>::load_transcript(
            open_store(&turya_home_dir())
                .ok_or("cannot open the session store")?
                .as_ref(),
            id,
        )
        .await
        {
            Ok(turns) => {
                let count = turns.len();
                app.load_transcript(&turya_protocol::Transcript {
                    session_id: id.clone(),
                    turns,
                });
                println!("Resumed {id} ({count} turns).");
            }
            Err(e) => return Err(format!("cannot resume {id}: {e}").into()),
        }
    }
    app.run(ui_cmd_tx, event_rx).await?;

    Ok(())
}

/// `~/.turya` (or `$TURYA_HOME`) support directory.
fn turya_home_dir() -> String {
    std::env::var("TURYA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.turya")
    })
}

fn catalog_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(turya_home_dir()).join("catalog")
}

fn config_path() -> String {
    format!("{}/config.toml", turya_home_dir())
}

/// Register built-in provider plugins (host-side; core stays vendor-blind).
fn build_provider_registry() -> turya_core::ProviderRegistry {
    let registry = turya_core::ProviderRegistry::new();
    registry.register(Arc::new(turya_provider_anthropic::AnthropicPlugin));
    registry.register(Arc::new(turya_provider_gemini::GeminiPlugin));
    // A local server the user already runs: no credential, model list read
    // off the running daemon rather than a static table.
    registry.register(Arc::new(turya_provider_ollama::OllamaPlugin));
    registry
}

/// Resolve the active provider: `--provider` > `TURYA_PROVIDER` > `anthropic`.
/// Falls back to the deterministic mock when sim mode is forced or no
/// credential resolves (offline stays usable; reason goes to stderr).
async fn select_provider(
    registry: &turya_core::ProviderRegistry,
    provider_flag: Option<&str>,
    store: &dyn turya_auth::CredentialStore,
) -> std::sync::Arc<dyn LlmProvider> {
    let mock = || {
        std::sync::Arc::new(MockProvider {
            responses: vec![
                ProviderStep::Token("Welcome to Turya. Analyzing your repository... ".to_string()),
                ProviderStep::CallTool(ToolCall {
                    call_id: "init_call".to_string(),
                    tool_name: "view_file".to_string(),
                    parameters: serde_json::json!({ "path": "Cargo.toml" }),
                    signature: None,
                }),
                ProviderStep::Token("\nRepository read complete. Ready for tasks.".to_string()),
                ProviderStep::Finish,
            ],
        }) as std::sync::Arc<dyn LlmProvider>
    };
    if std::env::var("TURYA_SIM_MODE").ok().as_deref() == Some("1") {
        return mock();
    }
    let id = provider_flag
        .map(|s| s.to_string())
        .or_else(|| std::env::var("TURYA_PROVIDER").ok())
        .unwrap_or_else(|| "anthropic".to_string());
    let plugin = match registry.get(&id) {
        Some(p) => p,
        None => {
            eprintln!("Turya: unknown provider '{id}', using mock (run: turya auth status)");
            return mock();
        }
    };
    // OAuth needs the user's client id for refresh; keychain holds it post-login.
    let oauth_cfg = || {
        let client_id = std::env::var("TURYA_OAUTH_CLIENT_ID")
            .ok()
            .or_else(|| store.get(&turya_auth::oauth_client_id_account(&id)))?;
        Some(turya_auth::OAuthConfig::google(&client_id))
    };
    let creds = match turya_auth::resolver::resolve(&id, None, oauth_cfg().as_ref(), store).await {
        Ok(creds) => creds,
        Err(e) => {
            eprintln!("Turya: {e}; using mock (offline/sim mode)");
            return mock();
        }
    };

    // The model is chosen AFTER credentials, because a provider may have no
    // static list at all: a local server is the authority on which models
    // exist, and asking it is the only way to learn a default.
    //
    // A configured model is validated only when there is a list to validate
    // against. Rejecting it against an empty list is what made a
    // discovery-backed provider boot as `model ''` and then fail every turn.
    let statics = plugin.models();
    let model = match std::env::var("TURYA_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
    {
        Some(m) if statics.iter().any(|known| known.id == m) => Some(m),
        Some(m) if statics.is_empty() => Some(m),
        Some(m) => {
            eprintln!("Turya: unknown model '{m}' for {id}, using default");
            None
        }
        None => None,
    };
    let model = match model {
        Some(m) => m,
        None => match statics.first() {
            Some(m) => m.id.clone(),
            // No static list: ask the server, and say so rather than booting
            // with an empty model that fails on the first request.
            None => match plugin.list_models(&creds).await.first() {
                Some(m) => {
                    eprintln!("Turya: discovered {id} model '{m}'");
                    m.clone()
                }
                None => {
                    eprintln!("Turya: no models available from {id} (is the server running?)");
                    String::new()
                }
            },
        },
    };
    match plugin.connect(creds, &model) {
        Ok(p) => {
            eprintln!("Turya: using {id} model '{model}'");
            p
        }
        Err(e) => {
            eprintln!("Turya: cannot connect {id} ({e}), using mock");
            mock()
        }
    }
}
