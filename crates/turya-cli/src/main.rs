use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::mpsc;
use turya_cli::{auth_cmd, config, host_services, update};
use turya_core::{LlmProvider, MockProvider, ProviderStep, TuryaEngine};
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

    let tools = Arc::new(ToolRegistry::standard());
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
        config_path,
        format!("{}/turya.db", turya_home_dir()),
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
    let model = std::env::var("TURYA_MODEL").ok().and_then(|m| {
        if plugin.models().iter().any(|known| known.id == m) {
            Some(m)
        } else {
            eprintln!("Turya: unknown model '{m}' for {id}, using default");
            None
        }
    });
    let model = model.unwrap_or_else(|| {
        plugin
            .models()
            .first()
            .map(|m| m.id.clone())
            .unwrap_or_default()
    });
    // OAuth needs the user's client id for refresh; keychain holds it post-login.
    let oauth_cfg = || {
        let client_id = std::env::var("TURYA_OAUTH_CLIENT_ID")
            .ok()
            .or_else(|| store.get(&turya_auth::oauth_client_id_account(&id)))?;
        Some(turya_auth::OAuthConfig::google(&client_id))
    };
    match turya_auth::resolver::resolve(&id, None, oauth_cfg().as_ref(), store).await {
        Ok(creds) => match plugin.connect(creds, &model) {
            Ok(p) => {
                eprintln!("Turya: using {id} model '{model}'");
                p
            }
            Err(e) => {
                eprintln!("Turya: cannot connect {id} ({e}), using mock");
                mock()
            }
        },
        Err(e) => {
            eprintln!("Turya: {e}; using mock (offline/sim mode)");
            mock()
        }
    }
}
