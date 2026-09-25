//! Anthropic provider plugin (internal, native).
//!
//! Rule 3.2: this crate owns the Anthropic wire dialect (Messages SSE,
//! tool-use assembly, `/v1/models` listing). The microkernel only sees
//! `LlmProvider` steps through the `ProviderPlugin` trait.

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use turya_core::{
    AuthMethodKind, LlmProvider, ModelInfo, ProviderPlugin, ProviderStep, ResolvedCreds,
};
use turya_protocol::{MessagePart, ToolCall, Transcript};

/// Streaming Anthropic Messages provider (SSE).
pub struct AnthropicProvider {
    api_key: String,
    pub model: String,
    pub max_tokens: u32,
    /// Reasoning effort level, behind a lock: the provider is shared and
    /// effort is set mid-session through `LlmProvider::set_effort`.
    effort: std::sync::RwLock<Option<String>>,
}

/// Thinking budgets for each effort level (see the Gemini provider for why
/// this scale exists).
const EFFORT_BUDGETS: [(&str, u32); 3] = [("low", 1024), ("medium", 8192), ("high", 24576)];

/// Map an effort level to a thinking-token budget.
pub fn thinking_budget(level: &str) -> Option<u32> {
    EFFORT_BUDGETS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(level))
        .map(|(_, budget)| *budget)
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            max_tokens: 4096,
            effort: std::sync::RwLock::new(None),
        }
    }

    /// Connect from host-resolved credentials (STEP 5 wiring calls this;
    /// `from_env` below preserves the current CLI bootstrap until then).
    pub fn connect_with(creds: &ResolvedCreds, model: &str) -> Self {
        Self::new(creds.token.clone(), model.to_string())
    }

    /// Returns `None` when sim mode is forced or no key is configured.
    /// Reads credentials strictly from the environment (`ANTHROPIC_API_KEY`);
    /// never accepts keys as CLI args so they cannot leak into shell history.
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

    /// Live model listing (`GET /v1/models`, newest first). Empty on ANY
    /// failure — callers fall through to cached/static lists, never an error.
    pub async fn fetch_live_models(api_key: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..5 {
            let mut url = "https://api.anthropic.com/v1/models?limit=100".to_string();
            if let Some(cursor) = &after {
                url.push_str(&format!("&after_id={cursor}"));
            }
            let resp = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("turya-provider")
                .build()
            {
                Ok(c) => c,
                Err(_) => break,
            }
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await;
            let body: serde_json::Value = match resp {
                Ok(r) if r.status().is_success() => match r.json().await {
                    Ok(b) => b,
                    Err(_) => break,
                },
                _ => break,
            };
            let empty = vec![];
            let data = body
                .get("data")
                .and_then(|d| d.as_array())
                .unwrap_or(&empty);
            if data.is_empty() {
                break;
            }
            for m in data {
                if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                    out.push(id.to_string());
                }
            }
            after = body
                .get("last_id")
                .and_then(|l| l.as_str())
                .map(|s| s.to_string());
            if !body
                .get("has_more")
                .and_then(|h| h.as_bool())
                .unwrap_or(false)
            {
                break;
            }
        }
        out
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
    fn set_effort(&self, level: Option<String>) {
        // Interior mutability: the provider is shared as an Arc and swapped
        // mid-session, so effort is set through a lock rather than a field.
        *self.effort.write().unwrap() = level;
    }
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let messages: Vec<serde_json::Value> = transcript
            .to_messages()
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    turya_protocol::Role::Assistant => "assistant",
                    turya_protocol::Role::User => "user",
                };
                let content: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { text } => json!({ "type": "text", "text": text }),
                        // Thinking blocks are re-sent as plain text: the API
                        // requires the signed original block, and a
                        // reconstructed one is rejected. Losing the signature
                        // costs cache reuse, not correctness.
                        MessagePart::Reasoning { text } => json!({ "type": "text", "text": text }),
                        MessagePart::ToolUse {
                            call_id,
                            name,
                            arguments,
                            ..
                        } => json!({
                            "type": "tool_use", "id": call_id, "name": name, "input": arguments
                        }),
                        MessagePart::ToolResult {
                            call_id,
                            content,
                            is_error,
                            truncated,
                        } => {
                            let mut block = json!({
                                "type": "tool_result", "tool_use_id": call_id, "content": content
                            });
                            if *is_error {
                                block["is_error"] = json!(true);
                            }
                            if *truncated {
                                block["_truncated"] = json!(true);
                            }
                            block
                        }
                        MessagePart::File { path, mime } => json!({
                            "type": "document",
                            "source": { "type": "file", "media_type": mime, "url": path.to_string_lossy() }
                        }),
                    })
                    .collect();
                json!({ "role": role, "content": content })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "tools": Self::tool_schemas(),
            "messages": messages,
        });
        let effort = self.effort.read().unwrap().clone();
        if let Some(budget) = effort.as_deref().and_then(thinking_budget) {
            // `thinking` requires a temperature of 1, which is the default;
            // sending both would be rejected.
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }

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
                                    // Anthropic signs thinking blocks, not calls.
                                    signature: None,
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

/// Registry plugin: Anthropic as a first-party native provider.
pub struct AnthropicPlugin;

#[async_trait]
impl ProviderPlugin for AnthropicPlugin {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn display_name(&self) -> &str {
        "Anthropic"
    }

    fn models(&self) -> Vec<ModelInfo> {
        // Curated static fallback (live discovery unions over this in STEP 4).
        ["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                display_name: id.to_string(),
            })
            .collect()
    }

    fn auth_methods(&self) -> Vec<AuthMethodKind> {
        vec![AuthMethodKind::ApiKey {
            env_var: "ANTHROPIC_API_KEY",
        }]
    }

    fn connect(&self, creds: ResolvedCreds, model: &str) -> Result<Arc<dyn LlmProvider>, String> {
        Ok(Arc::new(AnthropicProvider::new(
            creds.token,
            model.to_string(),
        )))
    }

    async fn list_models(&self, creds: &ResolvedCreds) -> Vec<String> {
        AnthropicProvider::fetch_live_models(&creds.token).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_declaration_is_stable() {
        let p = AnthropicPlugin;
        assert_eq!(p.id(), "anthropic");
        assert!(!p.models().is_empty());
        assert_eq!(
            p.auth_methods(),
            vec![AuthMethodKind::ApiKey {
                env_var: "ANTHROPIC_API_KEY"
            }]
        );
    }

    #[test]
    fn connect_uses_resolved_token_not_env() {
        let p = AnthropicPlugin;
        let creds = ResolvedCreds {
            token: "tok".to_string(),
            expires_at: None,
            via: "stored-key",
        };
        let prov = p.connect(creds, "m").unwrap();
        // Type-level: connect succeeds from creds alone (no env read).
        let _ = prov;
    }

    // NOTE: live `list_models` is covered by the ignored live test pattern
    // (STEP 8); unit tests never touch the network (deterministic suite).
    #[test]
    fn effort_levels_map_to_thinking_budgets() {
        assert_eq!(thinking_budget("low"), Some(1024));
        assert_eq!(thinking_budget("high"), Some(24576));
        assert_eq!(thinking_budget("extreme"), None);
    }
}
