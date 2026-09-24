use turya_core::TuryaEngine;
use turya_protocol::{TuryaCommand, TuryaEvent, PermissionDecision};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct TuryaSession {
    engine: Arc<TuryaEngine>,
    cmd_rx: mpsc::Receiver<TuryaCommand>,
    event_tx: mpsc::Sender<TuryaEvent>,
    active_perm_tx: Option<mpsc::Sender<(String, PermissionDecision)>>,
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
            turn_counter: 0,
        }
    }

    pub async fn run_loop(mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                TuryaCommand::SubmitPrompt { prompt, mode } => {
                    let engine = self.engine.clone();
                    let event_tx = self.event_tx.clone();

                    // Route permission decisions specifically for this active turn
                    let (turn_perm_tx, turn_perm_rx) = mpsc::channel(16);
                    self.active_perm_tx = Some(turn_perm_tx);

                    let turn_id = format!("turn_{}", self.turn_counter);
                    self.turn_counter += 1;

                    tokio::spawn(async move {
                        engine
                            .run_turn(&turn_id, &prompt, mode, event_tx, turn_perm_rx)
                            .await;
                    });
                }
                TuryaCommand::ResolvePermission { request_id, decision } => {
                    // Forward permission decision directly to the active turn
                    if let Some(ref tx) = self.active_perm_tx {
                        let _ = tx.send((request_id, decision)).await;
                    }
                }
                TuryaCommand::AbortTurn => {
                    self.active_perm_tx = None;
                    let _ = self
                        .event_tx
                        .send(TuryaEvent::Error {
                            message: "Turn aborted by user".to_string(),
                        })
                        .await;
                }
                _ => {}
            }
        }
    }
}
