use async_trait::async_trait;
use tokio::sync::mpsc;
use turya_protocol::{ToolCall, Transcript};

#[derive(Debug, Clone)]
pub enum ProviderStep {
    Token(String),
    CallTool(ToolCall),
    Finish,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// One model call over the current conversation.
    ///
    /// The transcript is the whole conversation — including the user's turn,
    /// which the engine records before calling. There is deliberately no
    /// separate `prompt` argument: passing it alongside the transcript is how
    /// a user turn gets duplicated and the request ends up starting with an
    /// assistant tool-call, which the APIs reject. The provider owns its wire
    /// format; see `Transcript::to_messages` for the neutral projection.
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String>;
}

/// Deterministic mock provider for tests and bootstrap verification
pub struct MockProvider {
    pub responses: Vec<ProviderStep>,
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn generate_turn(
        &self,
        _transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        for step in &self.responses {
            let _ = tx.send(step.clone()).await;
        }
        Ok(())
    }
}
