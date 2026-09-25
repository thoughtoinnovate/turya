use std::sync::Arc;
use tokio::sync::mpsc;
use turya_core::{TaskId, TaskKind, TaskRegistry, TuryaEngine};
use turya_protocol::{AgentMode, PermissionDecision, TuryaCommand, TuryaEvent};

pub struct TuryaSession {
    engine: Arc<TuryaEngine>,
    cmd_rx: mpsc::Receiver<TuryaCommand>,
    event_tx: mpsc::Sender<TuryaEvent>,
    active_perm_tx: Option<mpsc::Sender<(String, PermissionDecision)>>,
    /// Every live turn/subagent task. Replaces a single `active_turn` slot:
    /// aborting one id now tears down its whole tree, so a subagent can
    /// never outlive the turn that started it.
    tasks: TaskRegistry,
    active_turn: Option<TaskId>,
    turn_counter: usize,
    /// Prompts submitted while a turn is running. Replayed one at a time when
    /// the session goes idle, so follow-ups run in the order they were typed
    /// and never interleave with the turn that is already working.
    queue: std::collections::VecDeque<TuryaCommand>,
    /// Where a drained queue item goes: back into ourselves, so a queued
    /// prompt takes the identical path a typed one does.
    reinject: Option<mpsc::Sender<TuryaCommand>>,
    /// Signalled by the spawned turn task when `run_turn` returns. The engine
    /// reports completion to the client as an event, but the session loop must
    /// learn about it too: it owns the slot, and the queue drains on idle.
    finished: Option<mpsc::Sender<String>>,
}

impl TuryaSession {
    pub fn new(
        engine: Arc<TuryaEngine>,
        cmd_rx: mpsc::Receiver<TuryaCommand>,
        event_tx: mpsc::Sender<TuryaEvent>,
    ) -> Self {
        Self {
            engine,
            cmd_rx,
            event_tx,
            active_perm_tx: None,
            tasks: TaskRegistry::new(),
            active_turn: None,
            turn_counter: 0,
            queue: std::collections::VecDeque::new(),
            reinject: None,
            finished: None,
        }
    }

    /// Kill the running turn and anything it spawned. Returns its id when
    /// something died.
    fn kill_active_turn(&mut self) -> Option<String> {
        self.active_perm_tx = None;
        let id = self.active_turn.take()?;
        self.tasks.abort_tree(&id);
        Some(id.0)
    }

    /// Number of prompts waiting for the session to go idle.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Tell the client the queue changed, so a count can be rendered.
    async fn announce_queue(&self) {
        let _ = self
            .event_tx
            .send(TuryaEvent::QueueChanged {
                pending: self.queue.len(),
            })
            .await;
    }

    /// Run the next queued prompt if the session is idle. One at a time: a
    /// turn that drains the whole queue at once would hide the user's
    /// follow-ups behind a long batch they cannot interrupt individually.
    /// Run the next queued prompt if the session is idle. One at a time: a
    /// turn that drained the whole queue at once would hide the user's
    /// follow-ups behind a batch they cannot watch.
    async fn drain_one(&mut self) {
        if self.active_turn.is_some() {
            return;
        }
        let Some(next) = self.queue.pop_front() else {
            return;
        };
        self.announce_queue().await;
        if let Some(tx) = self.reinject.clone() {
            let _ = tx.send(next).await;
        }
    }

    pub async fn run_loop(mut self) {
        // Commands we bounce back to ourselves when a turn goes idle.
        let (reinject_tx, mut reinject_rx) = mpsc::channel::<TuryaCommand>(32);
        self.reinject = Some(reinject_tx.clone());
        // `reinject_tx` is held here for the lifetime of the loop.
        let (done_tx, mut done_rx) = mpsc::channel::<String>(16);
        self.finished = Some(done_tx);
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    self.dispatch(cmd).await;
                }
                Some(cmd) = reinject_rx.recv() => {
                    self.dispatch(cmd).await;
                }
                Some(turn_id) = done_rx.recv() => {
                    // The turn returned: free the slot and run the next
                    // queued prompt, if any. One at a time on purpose.
                    if self
                        .active_turn
                        .as_ref()
                        .is_some_and(|id| id.0 == turn_id)
                    {
                        self.active_turn = None;
                    }
                    self.tasks.reap();
                    self.drain_one().await;
                }
            }
        }
        // Nothing may outlive the loop.
        self.tasks.drain_all().await;
    }

    /// Handle one command. Queued prompts arrive here too, so a follow-up
    /// takes exactly the same path a typed one does.
    async fn dispatch(&mut self, cmd: TuryaCommand) {
        {
            match cmd {
                TuryaCommand::SubmitPrompt {
                    prompt,
                    mode,
                    attachments,
                } => {
                    let engine = self.engine.clone();
                    let event_tx = self.event_tx.clone();

                    // One active turn at a time: a stale task would interleave
                    // events into the new turn, so kill it first (silent —
                    // the new TurnStarted explains what happened).
                    if let Some(stale) = self.active_turn.take() {
                        self.tasks.abort_tree(&stale);
                    }
                    self.tasks.reap();

                    // Route permission decisions specifically for this active turn
                    let (turn_perm_tx, turn_perm_rx) = mpsc::channel(16);
                    self.active_perm_tx = Some(turn_perm_tx);

                    let turn_id = format!("turn_{}", self.turn_counter);
                    self.turn_counter += 1;

                    let task_turn_id = turn_id.clone();
                    let done_tx = self.finished.clone();
                    let handle = tokio::spawn(async move {
                        engine
                            .run_turn(
                                &task_turn_id,
                                &prompt,
                                mode,
                                &attachments,
                                event_tx,
                                turn_perm_rx,
                            )
                            .await;
                        if let Some(tx) = done_tx {
                            let _ = tx.send(task_turn_id).await;
                        }
                    });
                    let id = self.tasks.insert(&turn_id, TaskKind::Root, None, handle);
                    self.active_turn = Some(id);
                }
                TuryaCommand::ResolvePermission {
                    request_id,
                    decision,
                } => {
                    // Forward permission decision directly to the active turn
                    if let Some(ref tx) = self.active_perm_tx {
                        let _ = tx.send((request_id, decision)).await;
                    }
                }
                TuryaCommand::AbortTurn => {
                    // Idle Esc is a silent no-op; a live kill is acknowledged
                    // so the client can render it.
                    if let Some(turn_id) = self.kill_active_turn() {
                        // No queue announcement here: an abort does not change
                        // the queue, and inserting a QueueChanged between
                        // TurnCompleted and Error would reorder what the client
                        // sees on abort.
                        let _ = self
                            .event_tx
                            .send(TuryaEvent::TurnCompleted {
                                turn_id,
                                success: false,
                            })
                            .await;
                        let _ = self
                            .event_tx
                            .send(TuryaEvent::Error {
                                message: "Turn aborted by user".to_string(),
                            })
                            .await;
                    }
                }
                TuryaCommand::QueuePrompt { prompt } => {
                    // Busy means "queue"; idle means "just run it". A user who
                    // wants to change a running turn's mind aborts it and
                    // retypes, which is the honest way to say so.
                    if self.active_turn.is_some() {
                        self.queue.push_back(TuryaCommand::QueuePrompt { prompt });
                        self.announce_queue().await;
                    } else if let Some(tx) = self.reinject.clone() {
                        let _ = tx
                            .send(TuryaCommand::SubmitPrompt {
                                prompt,
                                mode: AgentMode::Build,
                                attachments: Vec::new(),
                            })
                            .await;
                    }
                }
                TuryaCommand::ClearQueue => {
                    self.queue.clear();
                    self.announce_queue().await;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Notify;
    use turya_core::{LlmProvider, ProviderStep};
    use turya_protocol::AgentMode;

    /// Provider that signals entry then parks until `gate` fires (or abort).
    struct GateProvider {
        entered: Arc<Notify>,
        gate: Arc<Notify>,
    }

    #[async_trait]
    impl LlmProvider for GateProvider {
        async fn generate_turn(
            &self,
            _transcript: &turya_protocol::Transcript,
            _tx: mpsc::Sender<ProviderStep>,
        ) -> Result<(), String> {
            self.entered.notify_one();
            self.gate.notified().await;
            Ok(())
        }
    }

    fn harness() -> (
        mpsc::Sender<TuryaCommand>,
        mpsc::Receiver<TuryaEvent>,
        Arc<Notify>,
        Arc<Notify>,
    ) {
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let provider: Arc<dyn LlmProvider> = Arc::new(GateProvider {
            entered: entered.clone(),
            gate: gate.clone(),
        });
        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = Arc::new(TuryaEngine::new(
            provider,
            tools,
            turya_protocol::PermissionMode::Open,
        ));
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(64);
        tokio::spawn(TuryaSession::new(engine, cmd_rx, event_tx).run_loop());
        (cmd_tx, event_rx, entered, gate)
    }

    async fn recv_timeout(event_rx: &mut mpsc::Receiver<TuryaEvent>) -> Option<TuryaEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn abort_kills_live_turn_and_acks() {
        let (cmd_tx, mut event_rx, entered, _gate) = harness();
        cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt: "long task".to_string(),
                mode: AgentMode::Build,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        // Wait until the provider is actually inside the turn.
        assert!(matches!(
            recv_timeout(&mut event_rx).await,
            Some(TuryaEvent::TurnStarted { .. })
        ));
        entered.notified().await;

        cmd_tx.send(TuryaCommand::AbortTurn).await.unwrap();

        // Kill is acknowledged with failed completion + toast message…
        assert!(matches!(
            recv_timeout(&mut event_rx).await,
            Some(TuryaEvent::TurnCompleted { success: false, .. })
        ));
        assert!(matches!(
            recv_timeout(&mut event_rx).await,
            Some(TuryaEvent::Error { .. })
        ));
        // …and the orphaned task emits nothing further.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn idle_abort_is_silent() {
        let (cmd_tx, mut event_rx, _entered, _gate) = harness();
        cmd_tx.send(TuryaCommand::AbortTurn).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn second_prompt_replaces_stale_turn() {
        let (cmd_tx, mut event_rx, entered, gate) = harness();
        cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt: "first".to_string(),
                mode: AgentMode::Build,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            recv_timeout(&mut event_rx).await,
            Some(TuryaEvent::TurnStarted { .. })
        ));
        entered.notified().await;

        // A new prompt kills the stale task silently (new TurnStarted explains).
        cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt: "second".to_string(),
                mode: AgentMode::Build,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            recv_timeout(&mut event_rx).await,
            Some(TuryaEvent::TurnStarted {
                turn_id,
                ..
            }) if turn_id == "turn_1"
        ));
        // Release the second turn so it can finish on its own.
        gate.notify_one();
        // Draining: at most the second turn's completion may arrive; the
        // first turn must never complete (it was killed).
        let mut saw_turn_0_done = false;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), event_rx.recv()).await
            {
                Ok(Some(TuryaEvent::TurnCompleted { turn_id, .. })) if turn_id == "turn_0" => {
                    saw_turn_0_done = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(!saw_turn_0_done);
    }
    #[tokio::test]
    async fn a_prompt_sent_while_busy_waits_for_the_turn() {
        // The queue contract: follow-ups run in order, after the current work,
        // and never interleave with it.
        let (cmd_tx, mut event_rx, entered, gate) = harness();
        cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt: "first".to_string(),
                mode: AgentMode::Build,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            recv_timeout(&mut event_rx).await,
            Some(TuryaEvent::TurnStarted { .. })
        ));
        entered.notified().await;

        // Busy: this is queued, not run.
        cmd_tx
            .send(TuryaCommand::QueuePrompt {
                prompt: "second".to_string(),
            })
            .await
            .unwrap();
        // The client learns the queue is non-empty.
        let mut saw_queue = false;
        for _ in 0..3 {
            match recv_timeout(&mut event_rx).await {
                Some(TuryaEvent::QueueChanged { pending: 1 }) => {
                    saw_queue = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(saw_queue, "the client is told what is queued");
        assert!(
            !matches!(
                recv_timeout(&mut event_rx).await,
                Some(TuryaEvent::TurnStarted { .. })
            ),
            "a queued prompt must not start while a turn is running"
        );

        // Finish the first turn: the queued prompt runs next.
        gate.notify_one();
        let mut started_second = false;
        for _ in 0..6 {
            match recv_timeout(&mut event_rx).await {
                Some(TuryaEvent::TurnStarted { turn_id, .. }) if turn_id.contains("1") => {
                    started_second = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(started_second, "the queued prompt runs once the turn ends");
    }

    #[tokio::test]
    async fn queueing_while_idle_runs_immediately() {
        let (cmd_tx, mut event_rx, _entered, _gate) = harness();
        cmd_tx
            .send(TuryaCommand::QueuePrompt {
                prompt: "now".to_string(),
            })
            .await
            .unwrap();
        assert!(
            matches!(
                recv_timeout(&mut event_rx).await,
                Some(TuryaEvent::TurnStarted { .. })
            ),
            "nothing to wait for, so it runs"
        );
    }

    #[tokio::test]
    async fn clearing_the_queue_drops_pending_prompts() {
        let (cmd_tx, mut event_rx, entered, gate) = harness();
        cmd_tx
            .send(TuryaCommand::SubmitPrompt {
                prompt: "first".to_string(),
                mode: AgentMode::Build,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let _ = recv_timeout(&mut event_rx).await;
        entered.notified().await;

        cmd_tx
            .send(TuryaCommand::QueuePrompt {
                prompt: "dropped".to_string(),
            })
            .await
            .unwrap();
        cmd_tx.send(TuryaCommand::ClearQueue).await.unwrap();

        gate.notify_one();
        // Give the loop room to (incorrectly) start the cleared prompt.
        let mut started = 0;
        for _ in 0..6 {
            match recv_timeout(&mut event_rx).await {
                Some(TuryaEvent::TurnStarted { .. }) => started += 1,
                Some(_) => continue,
                None => break,
            }
        }
        assert_eq!(started, 0, "a cleared prompt never runs");
    }
}
