//! End-to-end session persistence: engine turn -> stored log -> reload.
//!
//! This is the A1 contract. A turn must survive the process, a second turn
//! must *see* the first, and a crash-interrupted tool call must replay as a
//! valid request rather than a provider 400.

use std::sync::{Arc, Mutex};

use turya_core::{LlmProvider, ProviderStep, TuryaEngine};
use turya_memory::MemoryStore;
use turya_protocol::{AgentMode, Attachment, Part, PermissionMode, Transcript, TuryaEvent};
use turya_tools::ToolRegistry;

/// Records every transcript it is handed, so a test can assert what the
/// model would actually have seen on the second turn.
struct RecordingProvider {
    seen: Mutex<Vec<Transcript>>,
    scripts: Mutex<Vec<Vec<ProviderStep>>>,
}

impl RecordingProvider {
    fn new(scripts: Vec<Vec<ProviderStep>>) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            scripts: Mutex::new(scripts),
        })
    }
}

#[async_trait::async_trait]
impl LlmProvider for RecordingProvider {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: tokio::sync::mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(transcript.clone());
        let script = self.scripts.lock().unwrap().remove(0);
        for step in script {
            let _ = tx.send(step).await;
        }
        Ok(())
    }
}

fn text_step(s: &str) -> ProviderStep {
    ProviderStep::Token(s.to_string())
}

async fn run_turn(engine: &Arc<TuryaEngine>, session: &str, prompt: &str) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let (_ptx, prx) = tokio::sync::mpsc::channel(4);
    engine
        .run_turn(
            &format!("t-{}", prompt.len()),
            prompt,
            AgentMode::Build,
            &[] as &[Attachment],
            tx,
            prx,
        )
        .await;
    while rx.recv().await.is_some() {}
    let _ = session;
}

fn engine_with(
    provider: Arc<dyn LlmProvider>,
    store: Arc<MemoryStore>,
    session: &str,
) -> Arc<TuryaEngine> {
    Arc::new(
        TuryaEngine::new(
            provider,
            Arc::new(ToolRegistry::standard()),
            PermissionMode::Open,
        )
        .with_memory_hook(store)
        .with_session_id(session),
    )
}

#[tokio::test]
async fn a_turn_is_persisted_and_reloaded() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "first").unwrap();
    let provider =
        RecordingProvider::new(vec![vec![text_step("hello back"), ProviderStep::Finish]]);
    let engine = engine_with(provider.clone(), store.clone(), "s1");

    run_turn(&engine, "s1", "say hi").await;

    let meta = store.session_meta("s1").unwrap().unwrap();
    assert_eq!(meta.seq, 1, "one turn committed: {meta:?}");
    let turns = store.load_transcript("s1").unwrap();
    assert_eq!(turns.len(), 1);
    let text: String = turns[0]
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::UserText { text } | Part::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(text.contains("say hi"), "user turn stored: {text}");
    assert!(text.contains("hello back"), "assistant turn stored: {text}");
}

#[tokio::test]
async fn the_second_turn_sees_the_first() {
    // The whole point of the log: cross-turn memory. Before A1 every turn
    // started from an empty history.
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "").unwrap();
    let provider = RecordingProvider::new(vec![
        vec![text_step("the secret word is wombat"), ProviderStep::Finish],
        vec![text_step("understood"), ProviderStep::Finish],
    ]);
    let engine = engine_with(provider.clone(), store.clone(), "s1");

    run_turn(&engine, "s1", "remember: the secret word is wombat").await;
    run_turn(&engine, "s1", "what was the word?").await;

    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let second: String = seen[1]
        .texts()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        second.contains("wombat"),
        "turn 2 must carry turn 1's history: {second}"
    );
    assert!(second.contains("what was the word?"), "and its own prompt");
}

#[tokio::test]
async fn an_interrupted_turn_replays_as_a_valid_request() {
    // Simulate a crash: the log holds a tool call with no result. Replaying
    // it raw would be rejected by the provider, so the store closes the call.
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "crashed").unwrap();
    store
        .append_turn(
            "s1",
            &turya_memory::turn_with_id(
                "t-1",
                vec![
                    Part::UserText {
                        text: "run the build".to_string(),
                    },
                    Part::ToolCall {
                        call_id: "c1".to_string(),
                        tool_name: "run_bash".to_string(),
                        arguments: serde_json::json!({}),
                        signature: Some("sig".to_string()),
                    },
                ],
            ),
        )
        .unwrap();

    // A new turn loads the log and must build a request every call has an
    // answer for.
    let provider = RecordingProvider::new(vec![vec![text_step("recovered"), ProviderStep::Finish]]);
    let engine = engine_with(provider.clone(), store.clone(), "s1");
    run_turn(&engine, "s1", "carry on").await;

    let seen = provider.seen.lock().unwrap();
    let transcript: Transcript = seen[0].clone();
    let calls: Vec<&str> = transcript
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .filter_map(|p| match p {
            Part::ToolCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let results: Vec<&str> = transcript
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .filter_map(|p| match p {
            Part::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec!["c1"], "the interrupted call survives");
    assert_eq!(results, vec!["c1"], "and is closed so the request is valid");
    assert!(
        store.session_meta("s1").unwrap().unwrap().repaired,
        "the session is flagged as repaired"
    );
}

#[tokio::test]
async fn search_finds_content_after_the_fact() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "").unwrap();
    let provider = RecordingProvider::new(vec![
        vec![
            text_step("the ratelimiter lives in src/limit.rs"),
            ProviderStep::Finish,
        ],
        vec![text_step("sure"), ProviderStep::Finish],
    ]);
    let engine = engine_with(provider.clone(), store.clone(), "s1");
    run_turn(&engine, "s1", "where is the ratelimiter?").await;
    run_turn(&engine, "s1", "unrelated follow-up").await;

    let hits = store.search("s1", "ratelimiter", 5).unwrap();
    assert!(!hits.is_empty(), "post-hoc retrieval finds the detail");
    assert!(
        hits.iter().any(|h| h.contains("limit.rs")),
        "the answer is retrievable after the fact: {hits:?}"
    );
}

#[tokio::test]
async fn turn_completion_is_reported_and_stored() {
    let store = Arc::new(MemoryStore::open_in_memory().unwrap());
    store.begin_session("s1", "/repo", "").unwrap();
    let provider = RecordingProvider::new(vec![vec![text_step("done"), ProviderStep::Finish]]);
    let engine = engine_with(provider, store.clone(), "s1");
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let (_ptx, prx) = tokio::sync::mpsc::channel(4);
    engine
        .run_turn("t-x", "hi", AgentMode::Build, &[], tx, prx)
        .await;
    let mut completed = false;
    while let Some(evt) = rx.recv().await {
        if let TuryaEvent::TurnCompleted { success, .. } = evt {
            completed = success;
        }
    }
    assert!(completed, "a clean turn reports success");
    assert_eq!(store.session_meta("s1").unwrap().unwrap().seq, 1);
}
