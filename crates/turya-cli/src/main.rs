use clap::Parser;
use turya_core::{AnthropicProvider, LlmProvider, MockProvider, ProviderStep, TuryaEngine};
use turya_protocol::{PermissionMode, ToolCall};
use turya_tools::ToolRegistry;
use turya_server::TuryaSession;
use turya_tui::TuiApp;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "turya", about = "Fast, modular agentic coding harness")]
struct Args {
    #[arg(short, long, default_value = "review-for-me")]
    permission_mode: String,
    #[arg(long)]
    model: Option<String>,
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
    if let Some(model) = args.model {
        // Export for AnthropicProvider::from_env; CLI flag wins over env.
        std::env::set_var("TURYA_MODEL", model);
    }
    let permission_mode = parse_permission_mode(&args.permission_mode);

    // Deterministic mock when TURYA_SIM_MODE=1 or no API key is configured.
    let provider: std::sync::Arc<dyn LlmProvider> =
        match AnthropicProvider::from_env() {
            Some(real) => {
                eprintln!("Turya: using Anthropic model '{}'", real.model);
                std::sync::Arc::new(real)
            }
            None => std::sync::Arc::new(MockProvider {
                responses: vec![
                    ProviderStep::Token(
                        "Welcome to Turya. Analyzing your repository... ".to_string(),
                    ),
                    ProviderStep::CallTool(ToolCall {
                        call_id: "init_call".to_string(),
                        tool_name: "view_file".to_string(),
                        parameters: serde_json::json!({ "path": "Cargo.toml" }),
                    }),
                    ProviderStep::Token(
                        "\nRepository read complete. Ready for tasks.".to_string(),
                    ),
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
