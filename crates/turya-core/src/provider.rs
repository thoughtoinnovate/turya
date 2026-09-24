use async_trait::async_trait;
use turya_protocol::ToolCall;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum ProviderStep {
    Token(String),
    CallTool(ToolCall),
    Finish,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn generate_turn(
        &self,
        prompt: &str,
        history: &[String],
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
        _prompt: &str,
        _history: &[String],
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        for step in &self.responses {
            let _ = tx.send(step.clone()).await;
        }
        Ok(())
    }
}
