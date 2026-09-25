use std::sync::Arc;
use tokio::sync::mpsc;
use turya_core::TuryaEngine;
use turya_protocol::{PermissionDecision, TuryaCommand, TuryaEvent};

pub struct TuryaSession {
    engine: Arc<TuryaEngine>,
    cmd_rx: mpsc::Receiver<TuryaCommand>,
    event_tx: mpsc::Sender<TuryaEvent>,
    active_perm_tx: Option<mpsc::Sender<(String, PermissionDecision)>>,
    active_turn: Option<(String, tokio::task::JoinHandle<()>)>,
    turn_counter: usize,
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
            active_turn: None,
            turn_counter: 0,
        }
    }

    /// Kill the running turn, if any. Returns its id when something died.
    fn kill_active_turn(&mut self) -> Option<String> {
        self.active_perm_tx = None;
        self.active_turn.take().map(|(id, handle)| {
            handle.abort();
            id
        })
    }

    pub async fn run_loop(mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                TuryaCommand::SubmitPrompt { prompt, mode } => {
                    let engine = self.engine.clone();
                    let event_tx = self.event_tx.clone();

                    // One active turn at a time: a stale task would interleave
                    // events into the new turn, so kill it first (silent —
                    // the new TurnStarted explains what happened).
                    if let Some((_, stale)) = self.active_turn.take() {
                        stale.abort();
                    }

                    // Route permission decisions specifically for this active turn
                    let (turn_perm_tx, turn_perm_rx) = mpsc::channel(16);
                    self.active_perm_tx = Some(turn_perm_tx);

                    let turn_id = format!("turn_{}", self.turn_counter);
                    self.turn_counter += 1;

                    let task_turn_id = turn_id.clone();
                    let handle = tokio::spawn(async move {
                        engine
                            .run_turn(&task_turn_id, &prompt, mode, event_tx, turn_perm_rx)
                            .await;
                    });
                    self.active_turn = Some((turn_id, handle));
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
            _prompt: &str,
            _history: &[String],
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
}
