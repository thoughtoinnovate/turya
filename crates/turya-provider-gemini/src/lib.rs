//! Gemini provider plugin (internal, native).
//!
//! Rule 3.2: this crate owns the Gemini wire dialect (Generative Language
//! `streamGenerateContent` SSE, `functionDeclarations` mapping, `/v1beta/models`
//! listing). The microkernel only sees `LlmProvider` steps.

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use turya_core::{
    AuthMethodKind, LlmProvider, ModelInfo, ProviderPlugin, ProviderStep, ResolvedCreds,
};
use turya_protocol::{MessagePart, ToolCall, Transcript};

/// Streaming Gemini provider (Generative Language SSE).
///
/// Credential transport depends on provenance: API keys travel as `?key=`,
/// OAuth access tokens travel as `Authorization: Bearer` (Google rejects
/// OAuth tokens in the `key` param). The flag derives from `creds.via`,
/// so callers never branch on auth method themselves.
pub struct GeminiProvider {
    credential: String,
    bearer: bool,
    pub model: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            credential: api_key,
            bearer: false,
            model,
        }
    }

    pub fn connect_with(creds: &ResolvedCreds, model: &str) -> Self {
        Self {
            credential: creds.token.clone(),
            bearer: creds.via == "oauth",
            model: model.to_string(),
        }
    }

    /// Pure request target: `(url, bearer_token)`. Unit-tested, no network.
    fn request_target(&self) -> (String, Option<String>) {
        let base = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.model
        );
        if self.bearer {
            (base, Some(format!("Bearer {}", self.credential)))
        } else {
            (format!("{base}&key={}", self.credential), None)
        }
    }

    /// Request body for `streamGenerateContent` (pure: unit-tested).
    ///
    /// Wire mapping: assistant content carries `role: "model"`, tool results
    /// ride as `functionResponse` inside a `user` turn (the convention the
    /// Generative Language API requires for function results), and files
    /// become `inlineData`/`fileData` parts.
    fn request_body(transcript: &Transcript) -> serde_json::Value {
        let contents: Vec<serde_json::Value> = transcript
            .to_messages()
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    turya_protocol::Role::Assistant => "model",
                    turya_protocol::Role::User => "user",
                };
                let parts: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { text } | MessagePart::Reasoning { text } => {
                            json!({ "text": text })
                        }
                        MessagePart::ToolUse {
                            call_id,
                            name,
                            arguments,
                            signature,
                        } => {
                            // The signature is a sibling of `functionCall` on
                            // the part, not a field inside it — nesting it
                            // inside is rejected as an unknown name.
                            let mut part = json!({
                                "functionCall": { "name": name, "args": arguments, "id": call_id }
                            });
                            if let Some(sig) = signature {
                                part["thoughtSignature"] = json!(sig);
                            }
                            part
                        }
                        MessagePart::ToolResult {
                            call_id, content, ..
                        } => json!({
                            "functionResponse": {
                                "name": "tool_result",
                                "response": { "result": content, "call_id": call_id }
                            }
                        }),
                        MessagePart::File { path, mime } => {
                            // Inline bytes are not read here: the engine owns
                            // file access, and a missing file is a visible
                            // error rather than a silent empty part.
                            json!({
                                "fileData": { "mimeType": mime, "fileUri": path.to_string_lossy() }
                            })
                        }
                    })
                    .collect();
                json!({ "role": role, "parts": parts })
            })
            .collect();
        json!({
            "contents": contents,
            "tools": [{"functionDeclarations": Self::function_declarations()}],
        })
    }

    fn function_declarations() -> serde_json::Value {
        json!([
            {
                "name": "view_file",
                "description": "Read file content from the filesystem",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            },
            {
                "name": "write_file",
                "description": "Write or overwrite file content",
                "parameters": {
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
                "parameters": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }
            }
        ])
    }

    /// Translate one decoded SSE JSON payload into steps (pure: unit-tested).
    /// `call_seq` numbers synthetic call ids (`gcall_<n>`).
    ///
    /// Two silence traps handled here so turns never look empty:
    /// - `thought: true` parts (thinking models): surfaced as text.
    /// - `promptFeedback.blockReason`: surfaced as a visible warning.
    fn steps_from_payload(payload: &serde_json::Value, call_seq: &mut usize) -> Vec<ProviderStep> {
        let mut steps = Vec::new();
        if let Some(reason) = payload
            .get("promptFeedback")
            .and_then(|f| f.get("blockReason"))
            .and_then(|r| r.as_str())
        {
            steps.push(ProviderStep::Token(format!(
                "⚠ response blocked by safety filter: {reason}"
            )));
        }
        let empty = vec![];
        let candidates = payload
            .get("candidates")
            .and_then(|c| c.as_array())
            .unwrap_or(&empty);
        for cand in candidates {
            if let Some(reason) = cand.get("finishReason").and_then(|r| r.as_str()) {
                if reason == "SAFETY" {
                    steps.push(ProviderStep::Token(
                        "⚠ response stopped by safety filter".to_string(),
                    ));
                }
            }
            let parts = cand
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array());
            let parts = match parts {
                Some(p) => p,
                None => continue,
            };
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    // Thought parts and answer text both stream visibly;
                    // styling them apart is a /thinking display follow-up.
                    steps.push(ProviderStep::Token(text.to_string()));
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let args = fc.get("args").cloned().unwrap_or(json!({}));
                    *call_seq += 1;
                    steps.push(ProviderStep::CallTool(ToolCall {
                        call_id: format!("gcall_{}", *call_seq),
                        tool_name: name.to_string(),
                        parameters: args,
                        // Required on replay: the API rejects a functionCall
                        // part without the signature the model emitted.
                        signature: part
                            .get("thoughtSignature")
                            .and_then(|s| s.as_str())
                            .map(str::to_string),
                    }));
                }
            }
        }
        steps
    }

    /// Live model listing (`GET /v1beta/models`), filtered to generation-capable
    /// models. Empty on ANY failure — callers fall through, never error.
    /// `bearer` selects the OAuth transport (see [`GeminiProvider`]).
    pub async fn fetch_live_models(api_key: &str) -> Vec<String> {
        Self::fetch_live_models_authed(api_key, false).await
    }

    pub async fn fetch_live_models_authed(token: &str, bearer: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..5 {
            let mut url = if bearer {
                "https://generativelanguage.googleapis.com/v1beta/models?pageSize=100".to_string()
            } else {
                format!(
                    "https://generativelanguage.googleapis.com/v1beta/models?key={token}&pageSize=100"
                )
            };
            if let Some(t) = &page_token {
                url.push_str(&format!("&pageToken={t}"));
            }
            let body: serde_json::Value = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("turya-provider")
                .build()
            {
                Ok(c) => {
                    let mut req = c.get(&url);
                    if bearer {
                        req = req.bearer_auth(token);
                    }
                    match req.send().await {
                        Ok(r) if r.status().is_success() => match r.json().await {
                            Ok(b) => b,
                            Err(_) => break,
                        },
                        _ => break,
                    }
                }
                Err(_) => break,
            };
            let empty = vec![];
            let models = body
                .get("models")
                .and_then(|m| m.as_array())
                .unwrap_or(&empty);
            if models.is_empty() {
                break;
            }
            for m in models {
                let name = m.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let short = name.strip_prefix("models/").unwrap_or(name);
                let methods = m
                    .get("supportedGenerationMethods")
                    .and_then(|v| v.as_array());
                let generatable = methods
                    .map(|arr| arr.iter().any(|v| v.as_str() == Some("generateContent")))
                    .unwrap_or(false);
                if generatable && !short.is_empty() {
                    out.push(short.to_string());
                }
            }
            page_token = body
                .get("nextPageToken")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            if page_token.is_none() {
                break;
            }
        }
        out
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let (url, bearer) = self.request_target();
        let body = Self::request_body(transcript);
        let client = reqwest::Client::new();
        let mut req = client.post(&url).header("content-type", "application/json");
        if let Some(token) = bearer {
            req = req.header("authorization", token);
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("gemini request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let short: String = text.chars().take(300).collect();
            return Err(format!("gemini {}: {}", status, short));
        }

        let mut buf = String::new();
        let mut call_seq = 0usize;
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
                let value: serde_json::Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for step in Self::steps_from_payload(&value, &mut call_seq) {
                    let _ = tx.send(step).await;
                }
            }
        }

        let _ = tx.send(ProviderStep::Finish).await;
        Ok(())
    }
}

/// Registry plugin: Gemini as a first-party native provider.
pub struct GeminiPlugin;

#[async_trait]
impl ProviderPlugin for GeminiPlugin {
    fn id(&self) -> &str {
        "gemini"
    }

    fn display_name(&self) -> &str {
        "Gemini"
    }

    fn models(&self) -> Vec<ModelInfo> {
        // Curated static fallback (live discovery unions over this in STEP 4).
        ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                display_name: id.to_string(),
            })
            .collect()
    }

    fn auth_methods(&self) -> Vec<AuthMethodKind> {
        vec![
            AuthMethodKind::ApiKey {
                env_var: "GEMINI_API_KEY",
            },
            AuthMethodKind::OAuth,
        ]
    }

    fn connect(&self, creds: ResolvedCreds, model: &str) -> Result<Arc<dyn LlmProvider>, String> {
        Ok(Arc::new(GeminiProvider::connect_with(&creds, model)))
    }

    async fn list_models(&self, creds: &ResolvedCreds) -> Vec<String> {
        let bearer = creds.via == "oauth";
        GeminiProvider::fetch_live_models_authed(&creds.token, bearer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_declaration_is_stable() {
        let p = GeminiPlugin;
        assert_eq!(p.id(), "gemini");
        assert_eq!(p.models().len(), 3);
        assert_eq!(
            p.auth_methods(),
            vec![
                AuthMethodKind::ApiKey {
                    env_var: "GEMINI_API_KEY"
                },
                AuthMethodKind::OAuth,
            ]
        );
    }

    #[test]
    fn request_body_maps_tools_to_function_declarations() {
        let mut t = Transcript::new("s");
        t.push(turya_protocol::Part::UserText {
            text: "hi".to_string(),
        });
        let body = GeminiProvider::request_body(&t);
        let decls = body
            .pointer("/tools/0/functionDeclarations")
            .and_then(|d| d.as_array())
            .unwrap();
        assert_eq!(decls.len(), 3);
        assert!(decls.iter().any(|d| d["name"] == "run_bash"));
        let contents = body.get("contents").and_then(|c| c.as_array()).unwrap();
        assert_eq!(contents.len(), 1, "one user turn is one turn");
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn request_body_uses_model_role_and_function_response_pairing() {
        let mut t = Transcript::new("s");
        t.start_turn("t1");
        t.push(turya_protocol::Part::UserText {
            text: "list the files".to_string(),
        });
        t.push(turya_protocol::Part::Text {
            text: "looking".to_string(),
        });
        t.push(turya_protocol::Part::ToolCall {
            call_id: "c1".to_string(),
            tool_name: "run_bash".to_string(),
            arguments: serde_json::json!({"command": "ls"}),
            signature: Some("sig-1".to_string()),
        });
        t.push(turya_protocol::Part::ToolResult {
            call_id: "c1".to_string(),
            output: "ok".to_string(),
            truncated: false,
        });

        let body = GeminiProvider::request_body(&t);
        let contents = body.get("contents").and_then(|c| c.as_array()).unwrap();
        // user turn, then the model turn (text + functionCall merged), then
        // the function response as a user turn.
        assert_eq!(contents.len(), 3, "got: {contents:?}");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "list the files");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "looking");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["name"], "run_bash");
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["call_id"],
            "c1"
        );
    }

    #[test]
    fn a_request_never_opens_with_an_assistant_function_call() {
        // Regression guard for the live 400: the API requires a function call
        // to follow a user turn, so the transcript's first turn must be the
        // user's, however the model replies.
        let mut t = Transcript::new("s");
        t.push(turya_protocol::Part::UserText {
            text: "hello".to_string(),
        });
        t.push(turya_protocol::Part::ToolCall {
            call_id: "c1".to_string(),
            tool_name: "run_bash".to_string(),
            arguments: serde_json::json!({}),
            signature: Some("sig-1".to_string()),
        });
        let body = GeminiProvider::request_body(&t);
        let contents = body.get("contents").and_then(|c| c.as_array()).unwrap();
        assert_eq!(contents[0]["role"], "user");
    }

    #[test]
    fn thought_signature_is_replayed_beside_the_function_call() {
        // Both directions matter: the signature must be *captured* from the
        // response and *replayed* on the part, not nested inside functionCall.
        let payload = json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "name": "run_bash", "args": {"command": "ls"} },
                "thoughtSignature": "SIG-123"
            }]}}]
        });
        let mut seq = 0;
        let steps = GeminiProvider::steps_from_payload(&payload, &mut seq);
        let call = steps
            .iter()
            .find_map(|s| match s {
                ProviderStep::CallTool(c) => Some(c.clone()),
                _ => None,
            })
            .expect("function call parsed");
        assert_eq!(call.signature.as_deref(), Some("SIG-123"));

        let mut t = Transcript::new("s");
        t.push(turya_protocol::Part::UserText {
            text: "go".to_string(),
        });
        t.push(turya_protocol::Part::ToolCall {
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            arguments: call.parameters.clone(),
            signature: call.signature.clone(),
        });
        let body = GeminiProvider::request_body(&t);
        let parts = body
            .pointer("/contents/1/parts/0")
            .expect("model turn part exists");
        assert_eq!(
            parts["thoughtSignature"], "SIG-123",
            "signature must sit beside functionCall: {parts}"
        );
        assert_eq!(parts["functionCall"]["name"], "run_bash");
    }

    #[test]
    fn a_call_without_a_signature_omits_the_field() {
        let mut t = Transcript::new("s");
        t.push(turya_protocol::Part::UserText {
            text: "go".to_string(),
        });
        t.push(turya_protocol::Part::ToolCall {
            call_id: "c1".to_string(),
            tool_name: "run_bash".to_string(),
            arguments: serde_json::json!({}),
            signature: None,
        });
        let body = GeminiProvider::request_body(&t);
        let part = &body["contents"][1]["parts"][0];
        assert!(part.get("thoughtSignature").is_none(), "{part}");
    }

    #[test]
    fn steps_from_text_and_function_call() {
        let payload = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "I'll read it. "},
                    {"functionCall": {"name": "view_file", "args": {"path": "a.rs"}}},
                    {"functionCall": {"name": "", "args": {}}},
                ]}
            }]
        });
        let mut seq = 0;
        let steps = GeminiProvider::steps_from_payload(&payload, &mut seq);
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            ProviderStep::Token(t) => assert_eq!(t, "I'll read it. "),
            other => panic!("expected token, got {:?}", std::mem::discriminant(other)),
        }
        match &steps[1] {
            ProviderStep::CallTool(c) => {
                assert_eq!(c.call_id, "gcall_1");
                assert_eq!(c.tool_name, "view_file");
                assert_eq!(c.parameters["path"], "a.rs");
            }
            _ => panic!("expected tool call"),
        }
        // Empty/unknown payloads yield nothing, never panic.
        assert!(GeminiProvider::steps_from_payload(&json!({}), &mut seq).is_empty());
        assert!(GeminiProvider::steps_from_payload(&json!({"candidates":[]}), &mut seq).is_empty());
    }

    #[test]
    fn thought_parts_stream_visibly() {
        // Thinking-model reasoning must not vanish into empty turns.
        let payload = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "Let me look at the repo layout. ", "thought": true},
                    {"text": "Here it is."},
                ]}
            }]
        });
        let mut seq = 0;
        let steps = GeminiProvider::steps_from_payload(&payload, &mut seq);
        assert_eq!(steps.len(), 2);
        match &steps[0] {
            ProviderStep::Token(t) => assert!(t.contains("repo layout")),
            _ => panic!("thought text must stream"),
        }
    }

    #[test]
    fn safety_blocks_surface_instead_of_empty_turns() {
        let payload = json!({
            "promptFeedback": {"blockReason": "SAFETY"},
            "candidates": []
        });
        let mut seq = 0;
        let steps = GeminiProvider::steps_from_payload(&payload, &mut seq);
        assert_eq!(steps.len(), 1);
        match &steps[0] {
            ProviderStep::Token(t) => assert!(t.contains("SAFETY")),
            _ => panic!("block reason must surface"),
        }
        let payload = json!({
            "candidates": [{"finishReason": "SAFETY"}]
        });
        let steps = GeminiProvider::steps_from_payload(&payload, &mut seq);
        assert!(matches!(steps[..], [ProviderStep::Token(_)]));
    }

    #[test]
    fn credential_transport_by_provenance() {
        // API key (env/stored/login): credential travels as ?key=, no header.
        let keyed = GeminiProvider::new("sk-test".to_string(), "gemini-2.5-flash".to_string());
        let (url, bearer) = keyed.request_target();
        assert!(url.contains("?alt=sse&key=sk-test"));
        assert!(bearer.is_none());

        // OAuth token: no key param anywhere, Bearer header instead.
        // (Google rejects OAuth tokens in `key=`.)
        let oauth = GeminiProvider::connect_with(
            &ResolvedCreds {
                token: "ya29.test".to_string(),
                expires_at: None,
                via: "oauth",
            },
            "gemini-2.5-flash",
        );
        let (url, bearer) = oauth.request_target();
        assert!(!url.contains("key="));
        assert!(!url.contains("ya29"));
        assert_eq!(bearer.as_deref(), Some("Bearer ya29.test"));
    }
}
