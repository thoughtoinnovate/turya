//! Live-model integration tests. NOT run by `make test`.
//!
//! These hit the real provider with a real credential and assert on real
//! files on disk. They exist because the unit tests cannot prove that a
//! provider still *accepts* the wire format we build — only a live call can.
//!
//! Run with:
//!   TURYA_LIVE=1 cargo test -p turya-cli --test live -- --nocapture
//!
//! Credential resolution matches the real binary: `TURYA_GEMINI_API_KEY`,
//! else the stored store. Tests are skipped (not failed) when no credential
//! is present, so a clean machine can still run the suite green.

use std::sync::Arc;

use turya_cli::live::{live_credentials, LiveProvider};
use turya_core::TuryaEngine;
use turya_protocol::{AgentMode, PermissionMode, TuryaEvent};
use turya_tools::ToolRegistry;
use turya_tui::TuiApp;

/// The gate every test checks first.
macro_rules! require_live {
    () => {
        if std::env::var("TURYA_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skip: set TURYA_LIVE=1 to run live model tests");
            return;
        }
    };
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("turya-live-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn engine_for(provider: Arc<dyn turya_core::LlmProvider>) -> Arc<TuryaEngine> {
    Arc::new(TuryaEngine::new(
        provider,
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    ))
}

/// Drain events into a TUI app (the real render path) and collect the text.
async fn run_turn(engine: Arc<TuryaEngine>, prompt: &str, app: &mut TuiApp) -> (String, bool) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(256);
    let (_perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let turn = engine.clone();
    let owned = prompt.to_string();
    let task = tokio::spawn(async move {
        turn.run_turn("live0", &owned, AgentMode::Build, &[], event_tx, perm_rx)
            .await;
    });
    let mut success = false;
    while let Some(evt) = event_rx.recv().await {
        if let TuryaEvent::TurnCompleted { success: s, .. } = evt {
            success = s;
        }
        app.feed_flow_event(&evt);
    }
    task.await.unwrap();
    (app.transcript_text(), success)
}

#[tokio::test]
async fn live_streams_text_and_renders_in_the_tui() {
    require_live!();
    let creds = match live_credentials().await {
        Some(c) => c,
        None => {
            eprintln!("skip: no live credential available");
            return;
        }
    };
    let provider = LiveProvider::build(&creds);
    let engine = engine_for(provider);
    let mut app = TuiApp::new();
    let (text, success) = run_turn(
        engine,
        "Reply with exactly the word: turya-live-ok",
        &mut app,
    )
    .await;

    assert!(
        success,
        "live turn must complete cleanly; rendered:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("turya-live-ok"),
        "model response missing from the TUI transcript:\n{text}"
    );
    // The top bar and status line are live chrome, not transcript text.
    assert!(!app.transcript_text().is_empty());
}

#[tokio::test]
async fn live_reads_a_real_file_and_writes_one_back() {
    require_live!();
    let creds = match live_credentials().await {
        Some(c) => c,
        None => {
            eprintln!("skip: no live credential available");
            return;
        }
    };
    let dir = temp_dir("files");
    let src = dir.join("input.txt");
    let dst = dir.join("output.txt");
    std::fs::write(&src, "MAGIC_TOKEN_4821").unwrap();

    let prompt = format!(
        "Read {} and write its exact contents to {}. Use the tools. \
         Then tell me the token you wrote.",
        src.display(),
        dst.display()
    );
    let provider = LiveProvider::build(&creds);
    let engine = engine_for(provider);
    let mut app = TuiApp::new();
    let (text, success) = run_turn(engine, &prompt, &mut app).await;

    assert!(
        success,
        "tool turn must complete cleanly; rendered:\n{text}"
    );
    let written = std::fs::read_to_string(&dst)
        .unwrap_or_else(|e| panic!("model must write {}: {e}", dst.display()));
    assert!(
        written.contains("MAGIC_TOKEN_4821"),
        "written file must carry the token, got: {written}"
    );
    assert!(
        text.contains("MAGIC_TOKEN_4821"),
        "the model must report the token it read/wrote:\n{text}"
    );
    // Tool rows are part of what the user sees.
    assert!(
        text.contains("view_file") || text.contains("write_file"),
        "tool calls must render in the transcript:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
