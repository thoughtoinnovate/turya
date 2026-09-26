//! Subagent delegation: does a child turn actually run, does the parent get
//! its conclusion, and are the edges that would cost real money closed?
//!
//! The provider is scripted off the transcript's content rather than a fixed
//! response list, because parent and child share one provider here: only a
//! transcript-aware mock can tell which side of the delegation it is
//! answering for.

use std::sync::Arc;

use tokio::sync::mpsc;
use turya_core::{LlmProvider, ProviderStep, TuryaEngine, SPAWN_AGENT_TOOL};
use turya_protocol::{AgentMode, Part, PermissionMode, ToolCall, Transcript, TuryaEvent};
use turya_tools::ToolRegistry;

/// Answers by reading the transcript, so one provider can play both the
/// delegating parent and the delegated child.
struct Scripted {
    /// The parent delegates once, then wraps up with the child's answer.
    delegate: bool,
    /// Every transcript this provider was handed, so a test can assert on
    /// what the model was actually told rather than on what we hoped it saw.
    seen: Arc<std::sync::Mutex<Vec<Transcript>>>,
}

#[async_trait::async_trait]
impl LlmProvider for Scripted {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let text = || {
            transcript
                .turns
                .iter()
                .flat_map(|t| t.parts.iter())
                .filter_map(|p| match p {
                    Part::UserText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.seen.lock().unwrap().push(transcript.clone());
        let all = text();

        // A child turn is told it is a subagent by the engine's framing.
        if all.contains("You are the subagent") {
            let _ = tx.send(ProviderStep::Token("CHILD-ANSWER".into())).await;
            return Ok(());
        }

        // The child must not be able to delegate again; if it tries, the
        // engine refuses. Replaying a delegation here would loop.
        let already = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .any(|p| matches!(p, Part::ToolResult { .. }));
        if self.delegate && !already {
            let _ = tx
                .send(ProviderStep::CallTool(ToolCall {
                    call_id: "s1".into(),
                    tool_name: SPAWN_AGENT_TOOL.into(),
                    parameters: serde_json::json!({ "name": "scribe", "task": "do the side work" }),
                    signature: None,
                }))
                .await;
        } else {
            let _ = tx.send(ProviderStep::Token("parent is done".into())).await;
        }
        Ok(())
    }
}

async fn run(engine: TuryaEngine) -> Vec<TuryaEvent> {
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (_perm_tx, perm_rx) = mpsc::channel(1);
    let collect = tokio::spawn(async move {
        engine
            .run_turn(
                "t1",
                "do the thing",
                AgentMode::Build,
                &[],
                event_tx,
                perm_rx,
            )
            .await;
    });
    let mut events = Vec::new();
    while let Some(ev) = event_rx.recv().await {
        let stop = matches!(
            ev,
            TuryaEvent::TurnCompleted { .. } | TuryaEvent::Error { .. }
        );
        events.push(ev);
        if stop {
            break;
        }
    }
    drop(event_rx);
    let _ = collect.await;
    events
}

fn engine(scripted: Scripted) -> TuryaEngine {
    TuryaEngine::new(
        Arc::new(scripted),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    )
}

fn scripted(delegate: bool) -> (Scripted, Arc<std::sync::Mutex<Vec<Transcript>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        Scripted {
            delegate,
            seen: seen.clone(),
        },
        seen,
    )
}

#[tokio::test]
async fn a_delegated_child_answers_and_the_parent_sees_only_the_summary() {
    let (provider, _) = scripted(true);
    let events = run(engine(provider)).await;

    let started = events.iter().find_map(|e| match e {
        TuryaEvent::SubagentStarted { name, task, .. } => Some((name.clone(), task.clone())),
        _ => None,
    });
    assert_eq!(
        started,
        Some(("scribe".to_string(), "do the side work".to_string()))
    );

    let finished = events.iter().find_map(|e| match e {
        TuryaEvent::SubagentFinished { name, summary, .. } => Some((name.clone(), summary.clone())),
        _ => None,
    });
    assert_eq!(
        finished,
        Some(("scribe".to_string(), "CHILD-ANSWER".to_string())),
        "the parent must receive the child's conclusion verbatim"
    );
}

#[tokio::test]
async fn the_child_innards_do_not_leak_into_the_users_transcript() {
    let (provider, _) = scripted(true);
    let events = run(engine(provider)).await;
    // The child ran its own turn, so TurnStarted fired twice: the parent's
    // and the child's. If the child's events were forwarded the user would
    // see a second turn header for work they only asked for once.
    let starts = events
        .iter()
        .filter(|e| matches!(e, TuryaEvent::TurnStarted { .. }))
        .count();
    assert_eq!(starts, 1, "only the parent's turn is shown to the user");
}

#[tokio::test]
async fn the_tool_catalog_advertises_delegation_to_the_parent() {
    let (provider, seen) = scripted(false);
    run(engine(provider)).await;
    let seen = seen.lock().unwrap();
    let catalog = seen[0]
        .turns
        .iter()
        .flat_map(|t| t.parts.iter())
        .filter_map(|p| match p {
            Part::Instruction { text } => Some(text.as_str()),
            _ => None,
        })
        .find(|t| t.contains(SPAWN_AGENT_TOOL))
        .unwrap_or_else(|| panic!("the model was never told it can delegate: {:?}", seen[0]));
    assert!(
        catalog.contains("name: string") && catalog.contains("task: string"),
        "the delegation tool must document its arguments: {catalog}"
    );
}

/// Delegates, but the child returns without saying anything.
struct MuteChild {
    seen: Arc<std::sync::Mutex<Vec<Transcript>>>,
}

#[async_trait::async_trait]
impl LlmProvider for MuteChild {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(transcript.clone());
        let is_child = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .filter_map(|p| match p {
                Part::UserText { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|t| t.contains("You are the subagent"));
        if is_child {
            // Says nothing at all: the worst case a summary has to handle.
            let _ = tx.send(ProviderStep::Finish).await;
            return Ok(());
        }
        let already = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .any(|p| matches!(p, Part::ToolResult { .. }));
        if !already {
            let _ = tx
                .send(ProviderStep::CallTool(ToolCall {
                    call_id: "q1".into(),
                    tool_name: SPAWN_AGENT_TOOL.into(),
                    parameters: serde_json::json!({ "name": "mute", "task": "say nothing" }),
                    signature: None,
                }))
                .await;
        } else {
            let _ = tx.send(ProviderStep::Token("parent done".into())).await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_child_that_says_nothing_is_a_failure_not_an_empty_success() {
    // An empty summary handed to the parent as a success would be read as
    // "the work is done", which is the most expensive kind of wrong answer.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let events = run(TuryaEngine::new(
        Arc::new(MuteChild { seen: seen.clone() }),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    ))
    .await;

    let refusal = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|t| {
            t.turns
                .iter()
                .flat_map(|x| x.parts.iter())
                .filter_map(|p| match p {
                    Part::UserText { text } => Some(text.as_str()),
                    _ => None,
                })
                .any(|x| !x.contains("You are the subagent"))
        })
        .flat_map(|t| t.turns.iter().flat_map(|x| x.parts.iter()))
        .find_map(|p| match p {
            Part::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("the parent must have been told the delegation failed");
    assert!(
        refusal.contains("returned nothing") || refusal.contains("unfulfilled"),
        "the parent must be told the task was unfulfilled, got: {refusal}"
    );
    assert!(!events.is_empty());
}

/// A provider that delegates from the child too, to prove the depth cap is
/// enforced by the engine and not merely by the catalog going quiet.
struct Recursive {
    depth: std::sync::atomic::AtomicUsize,
    refusals: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl LlmProvider for Recursive {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let is_child = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .filter_map(|p| match p {
                Part::UserText { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|t| t.contains("You are the subagent"));
        let already = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .any(|p| matches!(p, Part::ToolResult { .. }));
        if is_child {
            if let Some(Part::ToolResult { output, .. }) = transcript
                .turns
                .iter()
                .flat_map(|t| t.parts.iter())
                .find(|p| matches!(p, Part::ToolResult { .. }))
            {
                // The engine refused; record what it said so the test can
                // assert the child was told *why*, not just blocked.
                self.refusals.lock().unwrap().push(output.clone());
                let _ = tx.send(ProviderStep::Token("child done".into())).await;
            } else {
                // First child pass: actually try to delegate, so the cap is
                // exercised rather than merely never reached.
                let _ = tx
                    .send(ProviderStep::CallTool(ToolCall {
                        call_id: "r2".into(),
                        tool_name: SPAWN_AGENT_TOOL.into(),
                        parameters: serde_json::json!({ "name": "deeper", "task": "one more level" }),
                        signature: None,
                    }))
                    .await;
            }
            return Ok(());
        }
        if !already {
            self.depth.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = tx
                .send(ProviderStep::CallTool(ToolCall {
                    call_id: "r1".into(),
                    tool_name: SPAWN_AGENT_TOOL.into(),
                    parameters: serde_json::json!({ "name": "recursive", "task": "go again" }),
                    signature: None,
                }))
                .await;
        } else {
            let _ = tx.send(ProviderStep::Token("done".into())).await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn the_child_cannot_delegate_further_and_is_told_why() {
    // Depth is a hard cap. A subagent that could spawn subagents could spawn
    // a fork bomb, and no amount of budget tuning fixes that.
    let refusals = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = TuryaEngine::new(
        Arc::new(Recursive {
            depth: std::sync::atomic::AtomicUsize::new(0),
            refusals: refusals.clone(),
        }),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    );
    run(engine).await;

    let refusals = refusals.lock().unwrap();
    assert_eq!(
        refusals.len(),
        1,
        "the child must have attempted one delegation and been refused once: {refusals:?}"
    );
    assert!(
        refusals[0].contains("nesting is capped"),
        "the child must be told nesting is capped, got: {}",
        refusals[0]
    );
}

/// A child that emits far more events than its channel can hold.
struct Chatty {
    /// Tokens the child will produce, each one a `TokenDelta` event.
    tokens: usize,
    seen: Arc<std::sync::Mutex<Vec<Transcript>>>,
}

#[async_trait::async_trait]
impl LlmProvider for Chatty {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(transcript.clone());
        let is_child = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .filter_map(|p| match p {
                Part::UserText { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|t| t.contains("You are the subagent"));
        if is_child {
            for i in 0..self.tokens {
                let _ = tx
                    .send(ProviderStep::Token(format!("child-token-{i} ")))
                    .await;
            }
            let _ = tx.send(ProviderStep::Finish).await;
            return Ok(());
        }
        let already = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .any(|p| matches!(p, Part::ToolResult { .. }));
        if !already {
            let _ = tx
                .send(ProviderStep::CallTool(ToolCall {
                    call_id: "c1".into(),
                    tool_name: SPAWN_AGENT_TOOL.into(),
                    parameters: serde_json::json!({ "name": "chatty", "task": "say a lot" }),
                    signature: None,
                }))
                .await;
        } else {
            let _ = tx.send(ProviderStep::Token("parent done".into())).await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_child_that_outfills_the_event_channel_does_not_deadlock() {
    // The channel between a subagent and its parent holds 256 events. Draining
    // it only after the child returned meant a chatty child blocked forever on
    // a send nobody was reading - a hang with no error, which is the worst
    // failure mode a test suite can have. This is the regression guard.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = TuryaEngine::new(
        Arc::new(Chatty {
            tokens: 2000,
            seen: seen.clone(),
        }),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    );

    // Bounded: a deadlock shows up as this timing out, not as a hung CI job.
    let events = tokio::time::timeout(std::time::Duration::from_secs(20), run(engine))
        .await
        .expect("the subagent turn deadlocked on its own event channel");

    let summary = events
        .iter()
        .find_map(|e| match e {
            TuryaEvent::SubagentFinished { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("the child must have finished");
    assert!(
        summary.contains("child-token-1999"),
        "every token must reach the parent, not just the first {}: {}",
        summary.len(),
        &summary[..summary.len().min(120)]
    );
}

/// A subagent that needs permission the user has not granted yet.
struct NeedyChild {
    seen: Arc<std::sync::Mutex<Vec<Transcript>>>,
}

#[async_trait::async_trait]
impl LlmProvider for NeedyChild {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        self.seen.lock().unwrap().push(transcript.clone());
        let is_child = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .filter_map(|p| match p {
                Part::UserText { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|t| t.contains("You are the subagent"));
        if is_child {
            let answered = transcript
                .turns
                .iter()
                .flat_map(|t| t.parts.iter())
                .any(|p| matches!(p, Part::ToolResult { .. }));
            if !answered {
                // run_bash is High risk, so Manual mode asks the user first.
                // This is the call that used to deadlock.
                let _ = tx
                    .send(ProviderStep::CallTool(ToolCall {
                        call_id: "need-1".into(),
                        tool_name: "run_bash".into(),
                        parameters: serde_json::json!({ "command": "echo hi" }),
                        signature: None,
                    }))
                    .await;
            } else {
                let _ = tx.send(ProviderStep::Token("child finished".into())).await;
            }
            return Ok(());
        }
        let done = transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .any(|p| matches!(p, Part::ToolResult { .. }));
        if !done {
            let _ = tx
                .send(ProviderStep::CallTool(ToolCall {
                    call_id: "n1".into(),
                    tool_name: SPAWN_AGENT_TOOL.into(),
                    parameters: serde_json::json!({ "name": "needy", "task": "run a command" }),
                    signature: None,
                }))
                .await;
        } else {
            let _ = tx.send(ProviderStep::Token("parent done".into())).await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_subagents_permission_request_reaches_the_user_instead_of_deadlocking() {
    // Manual mode plus a subagent that runs a High-risk tool used to hang
    // forever: the request was dropped on the floor, the child waited for an
    // answer that could not come, and the parent waited for the child. Bounded
    // by a timeout so a regression fails the suite instead of stalling CI.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = TuryaEngine::new(
        Arc::new(NeedyChild { seen: seen.clone() }),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Manual,
    );

    let (event_tx, mut event_rx) = mpsc::channel(256);
    // The permission channel the host normally owns. Nothing answers on it,
    // which is exactly the case that used to hang.
    let (perm_tx, perm_rx) = mpsc::channel(4);
    drop(perm_tx);

    let turn = {
        let engine = Arc::new(engine);
        tokio::spawn(async move {
            engine
                .run_turn(
                    "perm-1",
                    "have a subagent run a command",
                    AgentMode::Build,
                    &[],
                    event_tx,
                    perm_rx,
                )
                .await;
        })
    };

    let mut asked: Option<String> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, event_rx.recv()).await {
            Ok(Some(TuryaEvent::PermissionRequested { action, .. })) => {
                asked = Some(action);
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    let _ = turn.await;

    assert_eq!(
        asked.as_deref(),
        Some("run_bash"),
        "the user must be asked before a subagent's High-risk tool runs"
    );
    let _ = seen;
}

/// Calls one tool with one set of arguments, forever.
struct Looper;

#[async_trait::async_trait]
impl LlmProvider for Looper {
    async fn generate_turn(
        &self,
        _transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let _ = tx
            .send(ProviderStep::CallTool(ToolCall {
                call_id: "loop".into(),
                tool_name: "view_file".into(),
                parameters: serde_json::json!({ "path": "Cargo.toml" }),
                signature: None,
            }))
            .await;
        let _ = tx.send(ProviderStep::Token("done".into())).await;
        Ok(())
    }
}

#[tokio::test]
async fn a_model_that_repeats_the_same_call_is_stopped_instead_of_burning_the_budget() {
    // The exact shape seen in a real session: the same tool, the same
    // arguments, over and over, each answered with identical filler. Only the
    // step budget stopped it, so the user watched a frozen screen for the
    // length of the budget.
    let engine = TuryaEngine::new(
        Arc::new(Looper),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    );
    let events = run(engine).await;

    // Counted from the event stream, which is the ground truth: one entry per
    // completed call, in order.
    let mut executed = 0usize;
    let mut refused = 0usize;
    for e in &events {
        if let TuryaEvent::ToolCallCompleted(r) = e {
            if r.success {
                executed += 1;
            } else if r
                .error
                .as_deref()
                .is_some_and(|m| m.contains("just called with these exact arguments"))
            {
                refused += 1;
            }
        }
    }
    assert_eq!(
        executed, 1,
        "the file is read exactly once however often the model retries \
         (executed={executed} refused={refused})"
    );
    assert!(
        refused > 1,
        "every repeat must be refused rather than alternated (refused={refused})"
    );
}

#[tokio::test]
async fn a_legitimate_repeat_after_a_different_call_still_runs() {
    // The guard must not break real work: re-reading a file after changing
    // something else is exactly what an agent does.
    struct Rereader {
        results: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for Rereader {
        async fn generate_turn(
            &self,
            transcript: &Transcript,
            tx: mpsc::Sender<ProviderStep>,
        ) -> Result<(), String> {
            let n = transcript
                .turns
                .iter()
                .flat_map(|t| t.parts.iter())
                .filter(|p| matches!(p, Part::ToolResult { .. }))
                .count();
            for r in transcript
                .turns
                .last()
                .into_iter()
                .flat_map(|t| t.parts.iter())
            {
                if let Part::ToolResult { output, .. } = r {
                    self.results.lock().unwrap().push(output.clone());
                }
            }
            // read, write, read again - the same read, not consecutive.
            let call = match n {
                0 => ToolCall {
                    call_id: "a".into(),
                    tool_name: "view_file".into(),
                    parameters: serde_json::json!({ "path": "Cargo.toml" }),
                    signature: None,
                },
                1 => ToolCall {
                    call_id: "b".into(),
                    tool_name: "write_file".into(),
                    parameters: serde_json::json!({ "path": "turya-repeat-probe.txt", "content": "x" }),
                    signature: None,
                },
                2 => ToolCall {
                    call_id: "c".into(),
                    tool_name: "view_file".into(),
                    parameters: serde_json::json!({ "path": "Cargo.toml" }),
                    signature: None,
                },
                _ => {
                    let _ = tx.send(ProviderStep::Finish).await;
                    return Ok(());
                }
            };
            let _ = tx.send(ProviderStep::CallTool(call)).await;
            Ok(())
        }
    }

    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = TuryaEngine::new(
        Arc::new(Rereader {
            results: results.clone(),
        }),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    );
    run(engine).await;

    let results = results.lock().unwrap();
    assert!(
        !results
            .iter()
            .any(|r| r.contains("just called with these exact arguments")),
        "a non-consecutive repeat is legitimate and must run: {results:?}"
    );
    let _ = std::fs::remove_file("turya-repeat-probe.txt");
}

/// Fails a tool and records what the model was actually told.
struct FailingTool {
    results: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl LlmProvider for FailingTool {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        if let Some(t) = transcript.turns.last() {
            for p in &t.parts {
                if let Part::ToolResult { output, .. } = p {
                    self.results.lock().unwrap().push(output.clone());
                }
            }
        }
        if transcript
            .turns
            .iter()
            .flat_map(|t| t.parts.iter())
            .any(|p| matches!(p, Part::ToolResult { .. }))
        {
            let _ = tx.send(ProviderStep::Finish).await;
            return Ok(());
        }
        // `exit 3` fails, and its stderr is the diagnostic worth seeing.
        let _ = tx
            .send(ProviderStep::CallTool(ToolCall {
                call_id: "f1".into(),
                tool_name: "run_bash".into(),
                parameters: serde_json::json!({
                    "command": "echo 'error[E0308]: mismatched types' 1>&2; exit 3"
                }),
                signature: None,
            }))
            .await;
        Ok(())
    }
}

#[tokio::test]
async fn a_failed_command_still_shows_the_model_its_output() {
    // "Exited with code: 3" on its own is useless. The diagnostic is the whole
    // point of a failing shell command, and it used to be thrown away - which
    // is how an agent ends up guessing why a build broke.
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = TuryaEngine::new(
        Arc::new(FailingTool {
            results: results.clone(),
        }),
        Arc::new(ToolRegistry::standard()),
        PermissionMode::Open,
    );
    run(engine).await;

    let results = results.lock().unwrap();
    let told = results
        .iter()
        .find(|r| r.contains("run_bash"))
        .expect("the model must be told the command failed");
    assert!(
        told.contains("error[E0308]: mismatched types"),
        "the diagnostic must survive: {told}"
    );
    assert!(
        told.contains("Exited with code"),
        "and so must the reason it failed: {told}"
    );
}
