use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use super::provider::{LlmProvider, ProviderStep};
use turya_protocol::ToolCall;

/// Streaming Anthropic Messages provider (SSE).
///
/// Reads credentials strictly from the environment (`ANTHROPIC_API_KEY`);
/// never accepts keys as CLI args so they cannot leak into shell history.
pub struct AnthropicProvider {
    api_key: String,
    pub model: String,
    pub max_tokens: u32,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            max_tokens: 4096,
        }
    }

    /// Returns `None` when sim mode is forced or no key is configured.
    pub fn from_env() -> Option<Self> {
        if std::env::var("TURYA_SIM_MODE").ok().as_deref() == Some("1") {
            return None;
        }
        let key = std::env::var("ANTHROPIC_API_KEY").ok()?;
        if key.trim().is_empty() {
            return None;
        }
        let model =
            std::env::var("TURYA_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".to_string());
        Some(Self::new(key, model))
    }

    fn tool_schemas() -> serde_json::Value {
        json!([
            {
                "name": "view_file",
                "description": "Read file content from the filesystem",
                "input_schema": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            },
            {
                "name": "write_file",
                "description": "Write or overwrite file content",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "run_bash",
                "description": "Execute a bash shell command",
                "input_schema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }
            }
        ])
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn generate_turn(
        &self,
        prompt: &str,
        history: &[String],
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let mut messages: Vec<serde_json::Value> = history
            .iter()
            .map(|h| json!({"role": "user", "content": h}))
            .collect();
        messages.push(json!({"role": "user", "content": prompt}));

        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "tools": Self::tool_schemas(),
            "messages": messages,
        });

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("anthropic request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("anthropic {}: {}", status, text));
        }

        // Assemble streaming tool-input JSON per content-block index.
        let mut tool_names: std::collections::HashMap<usize, String> = Default::default();
        let mut tool_ids: std::collections::HashMap<usize, String> = Default::default();
        let mut tool_json: std::collections::HashMap<usize, String> = Default::default();
        let mut buf = String::new();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("sse read failed: {}", e))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim();
                if line.is_empty() || !line.starts_with("data:") {
                    continue;
                }
                let payload = line.trim_start_matches("data:").trim();
                if payload == "[DONE]" {
                    continue;
                }
                let evt: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let evt_type = evt.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match evt_type {
                    "content_block_delta" => {
                        let idx = evt.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let delta = evt.get("delta").cloned().unwrap_or(json!({}));
                        let d_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if d_type == "text_delta" {
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                let _ = tx.send(ProviderStep::Token(text.to_string())).await;
                            }
                        } else if d_type == "input_json_delta" {
                            if let Some(part) = delta.get("partial_json").and_then(|p| p.as_str()) {
                                tool_json.entry(idx).or_default().push_str(part);
                            }
                        }
                    }
                    "content_block_start" => {
                        let idx = evt.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let block = evt.get("content_block").cloned().unwrap_or(json!({}));
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                                tool_names.insert(idx, name.to_string());
                            }
                            if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                                tool_ids.insert(idx, id.to_string());
                            }
                        }
                    }
                    "content_block_stop" => {
                        let idx = evt.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        if let Some(name) = tool_names.remove(&idx) {
                            let raw = tool_json.remove(&idx).unwrap_or_else(|| "{}".to_string());
                            let params: serde_json::Value =
                                serde_json::from_str(&raw).unwrap_or(json!({}));
                            let call_id = tool_ids
                                .remove(&idx)
                                .unwrap_or_else(|| format!("call_{}", idx));
                            let _ = tx
                                .send(ProviderStep::CallTool(ToolCall {
                                    call_id,
                                    tool_name: name,
                                    parameters: params,
                                }))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
        }

        let _ = tx.send(ProviderStep::Finish).await;
        Ok(())
    }
}
