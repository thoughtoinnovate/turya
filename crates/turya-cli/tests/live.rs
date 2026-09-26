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

/// Run one turn, retrying transient provider capacity failures.
///
/// A shared API returns 503/UNAVAILABLE under load; that is a property of the
/// network, not of the build. Without this the suite is flaky for reasons
/// that have nothing to do with the code under test. Any *other* error returns
/// immediately, so a real regression still fails fast.
async fn run_turn(engine: Arc<TuryaEngine>, prompt: &str, app: &mut TuiApp) -> (String, bool) {
    let mut last = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5 * attempt as u64)).await;
        }
        let (text, success) = run_turn_once(engine.clone(), prompt, app).await;
        if success {
            return (text, true);
        }
        let transient = text.contains("503")
            || text.contains("UNAVAILABLE")
            || text.contains("high demand")
            || text.contains("RESOURCE_EXHAUSTED")
            || text.contains("429");
        last = text;
        if !transient {
            break;
        }
        eprintln!("live: transient provider error, retry {attempt}/3");
    }
    (last, false)
}

/// Drain events into a TUI app (the real render path) and collect the text.
async fn run_turn_once(engine: Arc<TuryaEngine>, prompt: &str, app: &mut TuiApp) -> (String, bool) {
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
async fn live_remembers_an_earlier_turn() {
    require_live!();
    let creds = match live_credentials().await {
        Some(c) => c,
        None => {
            eprintln!("skip: no live credential available");
            return;
        }
    };
    // Turn 1 plants a fact, turn 2 asks for it. Only a real model call can
    // prove the stored log is actually being replayed to the provider.
    let store = Arc::new(turya_memory::MemoryStore::open_in_memory().unwrap());
    let session = "live-memory";
    turya_core::MemoryHook::begin_session(store.as_ref(), session, "/repo", "memory probe")
        .await
        .unwrap();
    let engine = Arc::new(
        TuryaEngine::new(
            LiveProvider::build(&creds),
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_memory_hook(store.clone())
        .with_session_id(session),
    );

    let mut app = TuiApp::new();
    let (first, ok1) = run_turn(
        engine.clone(),
        "Remember this codeword exactly: PELICAN-7742. Reply only with OK.",
        &mut app,
    )
    .await;
    assert!(ok1, "turn 1 failed:\n{first}");
    let (second, ok2) = run_turn(
        engine.clone(),
        "What codeword did I ask you to remember? Reply with just the codeword.",
        &mut app,
    )
    .await;
    assert!(ok2, "turn 2 failed:\n{second}");
    assert!(
        second.to_uppercase().contains("PELICAN-7742"),
        "the model must recall the earlier turn's fact:\n{second}"
    );

    // And the log holds both turns for a later resume. A retried turn adds
    // one, so assert the floor rather than an exact count.
    let meta = store.session_meta(session).unwrap().unwrap();
    assert!(meta.seq >= 2, "both turns committed: {meta:?}");
    let turns = store.load_transcript(session).unwrap();
    assert!(turns.len() >= 2, "replayable: {} turns", turns.len());
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

#[tokio::test]
async fn live_compaction_preserves_a_fact_from_earlier() {
    require_live!();
    let creds = match live_credentials().await {
        Some(c) => c,
        None => {
            eprintln!("skip: no live credential available");
            return;
        }
    };
    // The real eval gate: plant a fact, bury it, compact with a real model,
    // then ask. Only a genuine summary can answer this.
    let store = Arc::new(turya_memory::MemoryStore::open_in_memory().unwrap());
    let session = "live-compact";
    turya_core::MemoryHook::begin_session(store.as_ref(), session, "/repo", "compact probe")
        .await
        .unwrap();
    let engine = Arc::new(
        TuryaEngine::new(
            LiveProvider::build(&creds),
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_memory_hook(store.clone())
        .with_session_id(session),
    );
    let mut app = TuiApp::new();

    // Enough turns that compaction is meaningful.
    for i in 1..=4 {
        let (text, ok) = run_turn(
            engine.clone(),
            &format!(
                "Turn {i}: say the single word ACK{i} and nothing else. \
                 The gateway token is ZEBRAFISH-9931."
            ),
            &mut app,
        )
        .await;
        assert!(ok, "turn {i} failed:\n{text}");
    }

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    let (compacted_view, marker) = engine
        .compact(Some("preserve identifiers exactly"), &event_tx)
        .await
        .expect("compaction runs against a live model");
    eprintln!("compaction marker: {marker}");
    // Drop the sender before draining: recv() would otherwise never return
    // None while this test still holds it.
    drop(event_tx);
    while event_rx.recv().await.is_some() {}
    assert!(
        compacted_view.turns.len() < 5,
        "the compacted view must be smaller: {}",
        compacted_view.turns.len()
    );
    let summary = compacted_view.turns[0]
        .parts
        .iter()
        .filter_map(|p| match p {
            turya_protocol::Part::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<String>();
    eprintln!("--- summary ---\n{summary}\n---");
    assert!(
        summary.contains("## Objective"),
        "the summary must follow the requested shape:\n{summary}"
    );

    // Now the question that matters.
    let (answer, ok) = run_turn(
        engine.clone(),
        "What was the gateway token? Reply with just the token.",
        &mut app,
    )
    .await;
    assert!(ok, "post-compaction turn failed:\n{answer}");
    assert!(
        answer.to_uppercase().contains("ZEBRAFISH-9931"),
        "the token must survive a real compaction:\n{answer}\n(summary was: {summary})"
    );
}

#[tokio::test]
async fn live_advertises_a_skill_and_the_model_loads_it() {
    require_live!();
    let creds = match live_credentials().await {
        Some(c) => c,
        None => {
            eprintln!("skip: no live credential available");
            return;
        }
    };
    // A real skill on disk, discovered and advertised; the model must read
    // the body itself and follow an instruction that is only in the file.
    let root = std::env::temp_dir().join("turya-live-skill");
    let dir = root.join(".agents/skills/release-notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: release-notes\ndescription: Draft release notes from git commits\n---\n\n         Draft release notes. The first line of your answer must be exactly\n         SKILL-WAS-LOADED, on its own. Then write one short line.\n",
    )
    .unwrap();

    let provider = LiveProvider::build(&creds);
    let engine = Arc::new(
        TuryaEngine::new(
            provider,
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_skills_hook(Arc::new(turya_skills::FileSkillProvider::new(&[root
            .to_string_lossy()
            .to_string()])))
        .with_session_id("live-skills"),
    );
    let mut app = TuiApp::new();
    let (text, ok) = run_turn(
        engine,
        "Please write release notes for the last commit. Use the release-notes skill.",
        &mut app,
    )
    .await;
    assert!(ok, "turn failed:\n{text}");
    assert!(
        text.contains("SKILL-WAS-LOADED"),
        "the model must have read SKILL.md and followed it:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
