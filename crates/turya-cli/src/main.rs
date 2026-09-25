use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::mpsc;
use turya_core::{AnthropicProvider, LlmProvider, MockProvider, ProviderStep, TuryaEngine};
use turya_protocol::{PermissionMode, ToolCall};
use turya_server::TuryaSession;
use turya_tools::ToolRegistry;
use turya_tui::TuiApp;

mod update;

#[derive(Parser, Debug)]
#[command(
    name = "turya",
    version,
    about = "Fast, modular agentic coding harness"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(short, long, default_value = "review-for-me")]
    permission_mode: String,
    #[arg(long)]
    model: Option<String>,
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
}

fn parse_permission_mode(raw: &str) -> PermissionMode {
    match raw.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
        "open" => PermissionMode::Open,
        "manual" => PermissionMode::Manual,
        _ => PermissionMode::ReviewForMe,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Self-management subcommands never touch the agent engine.
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
        None => {}
    }

    if let Some(model) = args.model {
        // Export for AnthropicProvider::from_env; CLI flag wins over env.
        std::env::set_var("TURYA_MODEL", model);
    }
    let permission_mode = parse_permission_mode(&args.permission_mode);

    // Deterministic mock when TURYA_SIM_MODE=1 or no API key is configured.
    let provider: std::sync::Arc<dyn LlmProvider> = match AnthropicProvider::from_env() {
        Some(real) => {
            eprintln!("Turya: using Anthropic model '{}'", real.model);
            std::sync::Arc::new(real)
        }
        None => std::sync::Arc::new(MockProvider {
            responses: vec![
                ProviderStep::Token("Welcome to Turya. Analyzing your repository... ".to_string()),
                ProviderStep::CallTool(ToolCall {
                    call_id: "init_call".to_string(),
                    tool_name: "view_file".to_string(),
                    parameters: serde_json::json!({ "path": "Cargo.toml" }),
                }),
                ProviderStep::Token("\nRepository read complete. Ready for tasks.".to_string()),
                ProviderStep::Finish,
            ],
        }),
    };

    let tools = Arc::new(ToolRegistry::standard());
    let mut engine = TuryaEngine::new(provider, tools, permission_mode);

    // Step 8: episodic memory at $TURYA_HOME or ~/.turya/turya.db (best-effort).
    let turya_home = std::env::var("TURYA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{}/.turya", home)
    });
    let db_path = format!("{}/turya.db", turya_home);
    if let Err(e) = std::fs::create_dir_all(&turya_home) {
        eprintln!("Turya: cannot create {}: {}", turya_home, e);
    } else {
        match turya_memory::MemoryStore::open(&db_path) {
            Ok(store) => {
                engine = engine
                    .with_memory(Arc::new(std::sync::Mutex::new(store)))
                    .with_session_id("local");
            }
            Err(e) => eprintln!("Turya: memory disabled ({}: {})", db_path, e),
        }
    }

    // Step 10: live diagnostics when a language server is on PATH.
    engine = engine.with_lsp(Arc::new(turya_lsp::LspBridge::rust_analyzer()));

    let engine = Arc::new(engine);

    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::channel(64);

    let session = TuryaSession::new(engine, cmd_rx, event_tx);
    tokio::spawn(async move {
        session.run_loop().await;
    });

    let app = TuiApp::new();
    app.run(cmd_tx, event_rx).await?;

    Ok(())
}
