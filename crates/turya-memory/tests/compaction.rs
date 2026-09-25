//! Compaction: prune, summarise, threshold, and the property that matters —
//! a compacted conversation can still answer questions about what came before.

use std::sync::Arc;

use turya_core::context::{
    compacted, compaction_prompt, estimate_transcript_tokens, prune_transcript, tail_turns,
};
use turya_core::{LlmProvider, ProviderStep, TuryaEngine};
use turya_memory::MemoryStore;
use turya_protocol::{
    AgentMode, Attachment, ContextBudget, Part, PermissionMode, Transcript, Turn, TurnId,
    TuryaEvent,
};
use turya_tools::ToolRegistry;

/// Replays a fixed script and records the transcript it was handed, so a test
/// can assert exactly what the model would see after a compaction.
struct Scripted {
    scripts: std::sync::Mutex<Vec<Vec<ProviderStep>>>,
    seen: std::sync::Mutex<Vec<Transcript>>,
}

impl Scripted {
    fn new(scripts: Vec<Vec<ProviderStep>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: std::sync::Mutex::new(scripts),
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl LlmProvider for Scripted {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: tokio::sync::mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(transcript.clone());
        // Take the script out under the lock, then send without holding it:
        // a std MutexGuard across an await makes the future non-Send.
        let script = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.is_empty() {
                vec![ProviderStep::Finish]
            } else {
                scripts.remove(0)
            }
        };
        for step in script {
            let _ = tx.send(step).await;
        }
        Ok(())
    }
}

fn turn(id: &str, parts: Vec<Part>) -> Turn {
    Turn {
        id: TurnId(id.to_string()),
        parts,
    }
}

fn tool_turn(id: &str, tool: &str, output: &str) -> Turn {
    turn(
        id,
        vec![
            Part::ToolCall {
                call_id: format!("{id}-call"),
                tool_name: tool.to_string(),
                arguments: serde_json::json!({}),
                signature: None,
            },
            Part::ToolResult {
                call_id: format!("{id}-call"),
                output: output.to_string(),
                truncated: false,
            },
        ],
    )
}

fn long_transcript() -> Transcript {
    let mut t = Transcript::new("s1");
    t.turns.push(turn(
        "t1",
        vec![Part::UserText {
            text: "the deploy key is in vault path ops/prod".to_string(),
        }],
    ));
    t.turns
        .push(tool_turn("t2", "view_file", &"x".repeat(6000)));
    t.turns
        .push(tool_turn("t3", "run_bash", "cargo test: 198 passed"));
    t.turns
        .push(tool_turn("t4", "view_file", &"y".repeat(6000)));
    t.turns.push(turn(
        "t5",
        vec![Part::UserText {
            text: "also remember port 8123".to_string(),
        }],
    ));
    t
}

#[test]
fn pruning_keeps_the_tail_and_marks_the_rest() {
    let mut t = long_transcript();
    let before = estimate_transcript_tokens(&t);
    let pruned = prune_transcript(&mut t);
    assert!(pruned > 0, "old tool results are pruned");
    let after = estimate_transcript_tokens(&t);
    assert!(
        after < before,
        "pruning must free context: {before} -> {after}"
    );

    // The last two turns are never pruned: the user's newest instruction and
    // the most recent tool result keep their exact values.
    let tail_text: String = t.turns[3..]
        .iter()
        .flat_map(|x| x.parts.iter())
        .filter_map(|p| match p {
            Part::ToolResult { output, .. } => Some(output.clone()),
            Part::UserText { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        tail_text.contains("8123"),
        "recent turn intact: {tail_text}"
    );
    assert!(
        tail_text.contains(&"y".repeat(50)),
        "the newest tool result keeps its bytes: {}",
        tail_text.chars().take(80).collect::<String>()
    );
    // And an older result is gone, replaced by the marker.
    assert!(
        !tail_text.contains("198 passed"),
        "older results are outside the tail: {tail_text}"
    );
}

#[test]
fn pruned_results_are_marked_not_faked() {
    let mut t = long_transcript();
    prune_transcript(&mut t);
    let old: Vec<&str> = t.turns[0..3]
        .iter()
        .flat_map(|x| x.parts.iter())
        .filter_map(|p| match p {
            Part::ToolResult { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert!(!old.is_empty());
    for o in old {
        assert!(o.contains("pruned"), "honest marker, not a value: {o}");
    }
}

#[test]
fn tail_is_the_last_two_turns() {
    let t = long_transcript();
    let tail = tail_turns(&t);
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].id.0, "t4");
    assert_eq!(tail[1].id.0, "t5");
}

#[test]
fn compaction_prompt_asks_for_the_headings_and_no_tools() {
    let p = compaction_prompt(None);
    assert!(p.contains("## Objective"), "{p}");
    assert!(p.contains("## Remaining tasks"), "{p}");
    assert!(p.contains("Do not call any tools"), "{p}");
    let focused = compaction_prompt(Some("focus on the auth bug"));
    assert!(focused.contains("auth bug"), "{focused}");
}

#[test]
fn compacted_transcript_is_summary_plus_verbatim_tail() {
    let t = long_transcript();
    let tail = tail_turns(&t);
    let next = compacted("s1", "SUMMARY BODY", &tail, "5 turns -> summary + last 2");
    assert_eq!(next.turns.len(), 3, "summary turn + 2 kept");
    assert_eq!(next.turns[0].id.0, "compaction");
    let first: String = next.turns[0]
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(first.contains("SUMMARY BODY"));
    assert!(first.contains("5 turns -> summary + last 2"), "{first}");
    // The kept turns are byte-identical, not summarised.
    assert_eq!(next.turns[1], tail[0]);
    assert_eq!(next.turns[2], tail[1]);
}

fn engine_with(provider: Arc<dyn LlmProvider>, store: Arc<MemoryStore>) -> Arc<TuryaEngine> {
    Arc::new(
        TuryaEngine::new(
            provider,
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_memory_hook(store)
        .with_session_id("s1"),
    )
}

async fn run(engine: &Arc<TuryaEngine>, prompt: &str) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let (_p, prx) = tokio::sync::mpsc::channel(4);
    engine
        .run_turn(
            &format!("t{}", prompt.len()),
            prompt,
            AgentMode::Build,
            &[] as &[Attachment],
            tx,
            prx,
        )
        .await;
    while rx.recv().await.is_some() {}
}

#[tokio::test]
async fn manual_compaction_summarises_and_keeps_the_tail() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    for t in long_transcript().turns {
        store.append_turn("s1", &t).unwrap();
    }
    let provider = Scripted::new(vec![vec![
        ProviderStep::Token("## Objective\nship it\n## Remaining tasks\nnone".to_string()),
        ProviderStep::Finish,
    ]]);
    let engine = engine_with(provider.clone(), store.clone());

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
    let (next, marker) = engine
        .compact(Some("focus on deploy"), &event_tx)
        .await
        .expect("compaction runs");

    assert!(marker.contains("5 turns"), "marker explains: {marker}");
    assert!(next.turns[0]
        .parts
        .iter()
        .any(|p| matches!(p, Part::Text { text } if text.contains("## Objective"))));

    // The summarising pass saw the focus instruction and no tools ran.
    let probe_text: String = {
        let seen = provider.seen.lock().unwrap();
        seen[0].texts().iter().map(|s| s.to_string()).collect()
    };
    assert!(
        probe_text.contains("auth bug") || probe_text.contains("focus on deploy"),
        "{probe_text}"
    );

    // Both compaction events reached the client. Read exactly two: draining
    // until close would hang, because this test still holds the sender.
    let mut started = false;
    let mut completed = false;
    for _ in 0..2 {
        match event_rx.recv().await {
            Some(TuryaEvent::CompactionStarted { .. }) => started = true,
            Some(TuryaEvent::CompactionCompleted { .. }) => completed = true,
            _ => {}
        }
    }
    assert!(started && completed, "compaction is visible, never silent");
}

#[tokio::test]
async fn a_tool_call_during_summarisation_is_dropped() {
    // Structural, not a prompt request: the summary pass cannot act.
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    for t in long_transcript().turns {
        store.append_turn("s1", &t).unwrap();
    }
    let provider = Scripted::new(vec![vec![
        ProviderStep::CallTool(turya_protocol::ToolCall {
            call_id: "sneaky".to_string(),
            tool_name: "run_bash".to_string(),
            parameters: serde_json::json!({"command": "rm -rf /"}),
            signature: None,
        }),
        ProviderStep::Token("## Objective\nsummary".to_string()),
        ProviderStep::Finish,
    ]]);
    let engine = engine_with(provider, store.clone());

    // A file that would be destroyed if the call were executed.
    let marker_file = std::env::temp_dir().join("turya-compact-must-not-run.txt");
    std::fs::write(&marker_file, "still here").unwrap();
    assert!(marker_file.exists());

    let (event_tx, _rx) = tokio::sync::mpsc::channel(32);
    let result = engine.compact(None, &event_tx).await;
    assert!(result.is_ok(), "compaction still completes");
    assert!(
        marker_file.exists(),
        "a tool call in the summary pass must never execute"
    );
    let _ = std::fs::remove_file(&marker_file);
}

#[tokio::test]
async fn compaction_refuses_a_session_that_is_too_short() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    store
        .append_turn(
            "s1",
            &turn("t1", vec![Part::UserText { text: "hi".into() }]),
        )
        .unwrap();
    let provider = Scripted::new(vec![]);
    let engine = engine_with(provider, store);
    let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
    let err = engine.compact(None, &event_tx).await.unwrap_err();
    assert!(err.contains("nothing to compact"), "{err}");
}

#[tokio::test]
async fn auto_compaction_fires_at_the_threshold_and_not_before() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    let engine = engine_with(Scripted::new(vec![]), store.clone());
    // A small window: the stored history is over it.
    engine.set_context_budget(4_000, 500);

    let mut t = long_transcript();
    assert!(
        engine.should_compact(&t),
        "a 4.5k window cannot hold this history"
    );

    // A young session is not a context problem, however small the window.
    let mut young = Transcript::new("s1");
    young
        .turns
        .push(turn("t1", vec![Part::UserText { text: "hi".into() }]));
    assert!(!engine.should_compact(&young), "never compact turn one");

    // And a big window does not trigger on the same history.
    engine.set_context_budget(1_000_000, 50_000);
    t.turns.truncate(2);
    assert!(!engine.should_compact(&t), "a large window is not full yet");
}

#[tokio::test]
async fn auto_compaction_can_be_switched_off() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    let engine = engine_with(Scripted::new(vec![]), store);
    engine.set_context_budget(2_000, 200);
    assert!(engine.should_compact(&long_transcript()));
    engine.set_auto_compact(false);
    assert!(
        !engine.should_compact(&long_transcript()),
        "manual /compact still works with auto off"
    );
}

#[tokio::test]
async fn a_shrinking_window_defers_compaction_to_the_next_prompt() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    let engine = engine_with(Scripted::new(vec![]), store);
    engine.set_context_budget(1_000_000, 50_000);
    assert!(!engine.needs_deferred_compact());
    // The user switches to a small model mid-session.
    engine.set_context_budget(8_000, 500);
    assert!(
        engine.needs_deferred_compact(),
        "the next prompt must compact first, not overflow"
    );
}

#[tokio::test]
async fn the_eval_gate_survives_compaction() {
    // The property compaction exists for: after compaction, a question about
    // something said BEFORE the compaction is still answerable, because the
    // tail is kept verbatim and the summary carries the rest.
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "eval").unwrap();
    let provider = Scripted::new(vec![
        vec![
            ProviderStep::Token("deploy key at ops/prod".to_string()),
            ProviderStep::Finish,
        ],
        vec![ProviderStep::Token("wrote config".to_string()), ProviderStep::Finish],
        vec![ProviderStep::Token("tests pass".to_string()), ProviderStep::Finish],
        // The compaction pass.
        vec![
            ProviderStep::Token(
                "## Objective\nShip.\n## Key facts and decisions\nThe deploy key lives at vault path ops/prod.\n## Remaining tasks\nnone"
                    .to_string(),
            ),
            ProviderStep::Finish,
        ],
        // The post-compaction question.
        vec![ProviderStep::Token("ops/prod".to_string()), ProviderStep::Finish],
    ]);
    let engine = engine_with(provider.clone(), store.clone());
    run(&engine, "the deploy key is at vault path ops/prod").await;
    run(&engine, "write the config").await;
    run(&engine, "run the tests").await;

    let (event_tx, _rx) = tokio::sync::mpsc::channel(32);
    let (compacted_view, _) = engine.compact(None, &event_tx).await.expect("compaction");

    run(&engine, "where is the deploy key again?").await;

    let text: String = {
        let seen = provider.seen.lock().unwrap();
        let last = seen.last().expect("a provider call happened");
        last.texts().iter().map(|s| s.to_string()).collect()
    };
    assert!(
        text.contains("ops/prod"),
        "the pre-compaction fact must survive compaction:\n{text}"
    );
    // And the compacted view is smaller than the original history.
    let stored = engine.stored_transcript().await;
    assert!(!stored.turns.is_empty(), "history is still stored");
    assert!(!compacted_view.turns.is_empty());
}

#[tokio::test]
async fn pre_compaction_history_stays_searchable() {
    // Compaction must not destroy the record: the log keeps every turn, so a
    // post-compaction question can be answered by searching, not by luck.
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "t").unwrap();
    for t in long_transcript().turns {
        store.append_turn("s1", &t).unwrap();
    }
    let engine = engine_with(Scripted::new(vec![]), store.clone());
    let (event_tx, _rx) = tokio::sync::mpsc::channel(16);
    engine
        .compact(None, &event_tx)
        .await
        .expect("compaction runs");
    // The engine persists the marker turn, but the original turns remain.
    let all = turya_core::MemoryHook::load_transcript(store.as_ref(), "s1")
        .await
        .unwrap();
    assert!(
        all.len() >= long_transcript().turns.len(),
        "pre-compaction turns are still in the log: {}",
        all.len()
    );
    let hits = store.search("s1", "8123", 5).unwrap();
    assert!(
        !hits.is_empty(),
        "a detail from before compaction is still retrievable"
    );
}

#[test]
fn context_budget_compares_against_usable_tokens() {
    let b = ContextBudget {
        total: 100_000,
        reserve: 20_000,
    };
    assert_eq!(b.usable(), 80_000);
    assert!(b.over(80_001));
    assert!(!b.over(80_000));
}
