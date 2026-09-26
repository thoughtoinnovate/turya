//! Ollama provider plugin (internal, native).
//!
//! Rule 3.2: this crate owns the Ollama wire dialect — `POST /api/chat` with
//! its **NDJSON** response, `GET /api/tags` for the installed model list,
//! `POST /api/show` for per-model capabilities and context length, and the
//! `OLLAMA_HOST` / `OLLAMA_API_KEY` knowledge behind them. The microkernel only
//! ever sees `LlmProvider` steps; nothing Ollama-shaped reaches `turya-core`.
//!
//! Two things here are unlike the hosted providers and are the reason this
//! crate earns its place:
//!
//! - **The stream is NDJSON, not SSE.** Bare JSON objects, one per line, no
//!   `data:` prefix. An SSE reader skips every line that does not start with
//!   `data:` and therefore returns an empty turn, not an error.
//! - **`/api/show` reports what a model can actually do** (`tools`, `vision`,
//!   `thinking`) and its real context length, so the model picker is built
//!   from the server's own answers instead of a hardcoded table of names.

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use turya_core::{
    AuthMethodKind, LlmProvider, ModelInfo, ProviderPlugin, ProviderStep, ResolvedCreds,
};
use turya_protocol::{MessagePart, Part, ToolCall, ToolSpec, Transcript};

/// Where a stock `ollama serve` listens.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// How long the server keeps the model resident between calls.
///
/// The server default is 5m, which a multi-pass agent turn can exceed — the
/// turn is generate → tool → generate, with a permission pause in the middle —
/// and the model unloads mid-turn.
pub const DEFAULT_KEEP_ALIVE: &str = "30m";

/// `options.num_ctx` sent on every request.
///
/// A CPU-only server commonly defaults to 4096, which silently truncates long
/// turns while the catalog happily reports a much larger window. Pinning it
/// makes the number the catalog shows the number actually in force.
pub const DEFAULT_NUM_CTX: u32 = 32_768;

/// Upper bound on `/api/show` probes per listing. Probing every pulled model
/// is not worth it past this: the listing is for a picker, not a benchmark.
/// Models past the cap are still listed, without capability data.
pub const MAX_PROBED_MODELS: usize = 24;

/// Per-probe ceiling. Shorter than the client timeout on purpose: a hung
/// probe must cost seconds, not the whole listing.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Ollama's base URL, from `OLLAMA_HOST` or the stock default.
///
/// Read at call time, not cached, so a server the user starts later is found
/// without a restart.
pub fn default_base_url() -> String {
    normalize_base(&std::env::var("OLLAMA_HOST").unwrap_or_default())
}

/// Turn a configured host into a base URL.
///
/// `OLLAMA_HOST` is commonly `0.0.0.0:11434` (an *interface*, which is what the
/// server binds) or a `https://` tunnel to a machine elsewhere, so a missing
/// scheme means http and a trailing slash is dropped rather than doubled.
pub fn normalize_base(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    if host.is_empty() {
        return DEFAULT_BASE_URL.to_string();
    }
    if host.starts_with("http://") || host.starts_with("https://") {
        host.to_string()
    } else {
        format!("http://{host}")
    }
}

/// Join a base URL and an endpoint path without doubling the slash.
fn url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Bearer header for a token, or `None` when there is nothing to send.
///
/// A local server has no credential, so an empty or blank token is the normal
/// case and must not become an error or an empty `Authorization` header.
fn bearer_header(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(format!("Bearer {token}"))
    }
}

/// The reasoning level to put in `think`, or `None` to omit the field.
///
/// Ollama accepts `low` / `medium` / `high` / `max` (or a bool). An unrecognized
/// level omits the field entirely so the server's own default applies, which is
/// what the other providers do — a guessed level is a wrong answer, not a
/// lenient one. `think: false` is deliberately not reachable here: turya's
/// effort levels are the four named ones.
pub fn think_level(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "max" => Some("max"),
        _ => None,
    }
}

/// What the local server says one model can do.
///
/// All-false with no window means *the server did not say* — a model whose
/// `/api/show` probe failed is still listed, just not described. Absent data is
/// not a claim of absence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// `"tools"` — the model can call functions.
    pub tools: bool,
    /// `"vision"` — the model accepts image input.
    pub vision: bool,
    /// `"thinking"` — the model emits reasoning (`message.thinking`).
    pub thinking: bool,
    /// The model's maximum context length, in tokens.
    pub context_window: Option<u32>,
    /// False when the server said nothing at all, as opposed to saying none
    /// of the above. Decides whether a `/api/show` probe is worth a round
    /// trip, and keeps "unknown" from being shown to the user as "cannot".
    pub known: bool,
}

impl ModelCapabilities {
    /// Keep what is already known, take only what the other side learned.
    ///
    /// A probe that answers with less than the listing did must not erase it.
    pub fn merged_over(self, fallback: ModelCapabilities) -> ModelCapabilities {
        ModelCapabilities {
            tools: self.tools || fallback.tools,
            vision: self.vision || fallback.vision,
            thinking: self.thinking || fallback.thinking,
            context_window: self.context_window.or(fallback.context_window),
            known: self.known || fallback.known,
        }
    }
}

/// One model found on the local server, with the label and capabilities the
/// server itself reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// The name `/api/chat` takes, verbatim (`qwen3.5:0.8b`).
    pub id: String,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
}

/// A human label for a model id: `qwen3.5:0.8b (0.8B, Q4_K_M)`.
///
/// Built from the `details` block, not from a table: the id alone is already
/// what `/api/tags` returned, and the size/quantization is what tells two
/// locally installed builds of the same family apart.
pub fn display_name_for(id: &str, details: Option<&Value>) -> String {
    let field = |name: &str| {
        details
            .and_then(|d| d.get(name))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut extra = Vec::new();
    for name in ["parameter_size", "quantization_level"] {
        let value = field(name);
        if !value.is_empty() {
            extra.push(value);
        }
    }
    if extra.is_empty() {
        id.to_string()
    } else {
        format!("{id} ({})", extra.join(", "))
    }
}

/// The model's maximum context length, from an `/api/show` `model_info` block.
///
/// The key is `<arch>.context_length` and the architecture prefix changes with
/// the model family — `llama.context_length`, `qwen3.context_length`,
/// `qwen2.context_length`, … — so any key with that suffix counts and none is
/// hardcoded. The widest positive value wins because the block is an unordered
/// map that can carry more than one architecture's key.
///
/// No such key means no number. A guessed window is worse than an absent one:
/// it decides when the session compacts, and a wrong guess truncates turns.
pub fn context_length(model_info: &Value) -> Option<u32> {
    let fields = model_info.as_object()?;
    fields
        .iter()
        // Accepts `qwen35.context_length` (from `/api/show` `model_info`) and
        // the bare `context_length` (from `/api/tags` `details`). Both shapes
        // are real: confirmed against a 0.34.4 server, which sends one in
        // each place for the same model.
        .filter(|(key, _)| key.ends_with(".context_length") || key.as_str() == "context_length")
        .filter_map(|(_, value)| value.as_u64())
        .filter(|len| *len > 0)
        .max()
        .and_then(|len| u32::try_from(len).ok())
}

/// Read the capability flags and the context window out of an `/api/show`
/// payload.
///
/// `capabilities` is the server's own answer, which is why the flags are not a
/// static table: `["completion","tools","vision","thinking"]` on one model,
/// `["completion"]` on another, and the picker should show that difference.
/// `completion` is the baseline every listed model has, so it carries no flag.
pub fn capabilities_from_show(show: &Value) -> ModelCapabilities {
    let caps = show
        .get("capabilities")
        .and_then(|c| c.as_array())
        .map(|c| c.as_slice())
        .unwrap_or(&[]);
    let has = |name: &str| caps.iter().any(|c| c.as_str() == Some(name));
    let window = context_length(&show["model_info"]);
    ModelCapabilities {
        tools: has("tools"),
        vision: has("vision"),
        thinking: has("thinking"),
        context_window: window,
        // A payload with neither capabilities nor a window told us nothing,
        // which is not the same as being told "none of the above".
        known: show.get("capabilities").is_some() || window.is_some(),
    }
}

/// Read capabilities and a context window out of one `/api/tags` entry.
///
/// A current server answers both here, so a typical install needs no
/// `/api/show` probe at all. `known: false` when it stayed silent, which is
/// what makes the probe worth spending.
pub fn capabilities_from_tags(entry: &Value) -> ModelCapabilities {
    let caps = entry
        .get("capabilities")
        .and_then(|c| c.as_array())
        .map(|c| c.as_slice())
        .unwrap_or(&[]);
    let has = |name: &str| caps.iter().any(|c| c.as_str() == Some(name));
    let window = entry
        .get("details")
        .and_then(|d| d.get("context_length"))
        .and_then(|v| v.as_u64())
        .and_then(|len| u32::try_from(len).ok());
    ModelCapabilities {
        tools: has("tools"),
        vision: has("vision"),
        thinking: has("thinking"),
        context_window: window,
        known: entry.get("capabilities").is_some() || window.is_some(),
    }
}

/// Emit a held-back run of tool results, in call order.
fn flush_results(pending: &mut Vec<(usize, Value)>, out: &mut Vec<Value>) {
    pending.sort_by_key(|(pos, _)| *pos);
    out.extend(pending.drain(..).map(|(_, message)| message));
}

/// Ollama's `messages` array for a transcript.
///
/// Two things this must get right, both of which the server answers with a 400
/// or a visibly confused model:
///
/// 1. **A tool result is keyed by `tool_name`, not by call id.** Ollama's
///    response carries no call ids at all, and `MessagePart::ToolResult` has
///    only a `call_id`, so the name is recovered from the `ToolCall` part the
///    result answers. Results are emitted in call order. If the model calls the
///    same tool twice in one pass, the history here holds two `role:"tool"`
///    messages with the same `tool_name` and Ollama cannot tell them apart:
///    the protocol has no id to disambiguate with, so no change on this side
///    fixes it and inventing one would be a lie the model then acts on. The
///    ambiguity is Ollama's wire shape, carried faithfully.
/// 2. **The request must not open with an assistant message carrying tool
///    calls.** Both APIs reject that with a 400, and reordering history to
///    avoid it would corrupt the conversation, so a leading assistant turn with
///    `tool_calls` is dropped. A leading `role:"tool"` message cannot happen:
///    a transcript always opens with the user's turn, which is what
///    `Transcript::to_messages` exists to guarantee.
fn wire_messages(transcript: &Transcript) -> Vec<Value> {
    // call_id -> (position in call order, tool name). The position is what
    // orders the results below.
    let mut position: HashMap<String, usize> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut order = 0usize;
    for turn in &transcript.turns {
        for part in &turn.parts {
            if let Part::ToolCall {
                call_id, tool_name, ..
            } = part
            {
                if position.insert(call_id.clone(), order).is_none() {
                    order += 1;
                }
                names.insert(call_id.clone(), tool_name.clone());
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();
    // A run of consecutive tool results, held back so it can be emitted in
    // call order. Results stay where the transcript put them relative to the
    // assistant turn that asked for them — hoisting them to the end would put
    // a later assistant turn ahead of its own results, which is a 400.
    let mut pending: Vec<(usize, Value)> = Vec::new();

    for message in transcript.to_messages() {
        let is_result = !message.content.is_empty()
            && message
                .content
                .iter()
                .all(|part| matches!(part, MessagePart::ToolResult { .. }));
        if is_result {
            for part in &message.content {
                if let MessagePart::ToolResult {
                    call_id, content, ..
                } = part
                {
                    let pos = position.get(call_id).copied().unwrap_or(order);
                    pending.push((
                        pos,
                        json!({
                            "role": "tool",
                            "content": content,
                            // Ollama matches results by name alone. See the
                            // note on this function: a missing name means the
                            // transcript held a result with no call, and
                            // there is nothing better to send.
                            "tool_name": names.get(call_id).cloned().unwrap_or_default(),
                        }),
                    ));
                }
            }
            continue;
        }
        flush_results(&mut pending, &mut out);

        let role = match message.role {
            turya_protocol::Role::User => "user",
            turya_protocol::Role::Assistant => "assistant",
        };
        let mut content = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        for part in &message.content {
            match part {
                // Reasoning goes back as content: the request-side message
                // has no separate thinking field, and re-sending it as text
                // is what the model actually said.
                MessagePart::Text { text } | MessagePart::Reasoning { text } => {
                    content.push_str(text)
                }
                MessagePart::ToolUse {
                    name, arguments, ..
                } => tool_calls.push(json!({
                    "function": { "name": name, "arguments": arguments }
                })),
                // An image cannot be sent here: Ollama takes inline base64 in
                // `images` and the neutral part carries a *path*, which the
                // server would reject as a malformed image. Dropping the part
                // beats sending something the model cannot read; the engine's
                // file tools remain the way a model sees an image.
                MessagePart::File { .. } => {}
                MessagePart::ToolResult { .. } => unreachable!("handled above"),
            }
        }
        if content.is_empty() && tool_calls.is_empty() {
            continue;
        }
        let mut wire = json!({ "role": role, "content": content });
        if !tool_calls.is_empty() {
            wire["tool_calls"] = Value::Array(tool_calls);
        }
        out.push(wire);
    }
    flush_results(&mut pending, &mut out);

    if out
        .first()
        .is_some_and(|m| m.get("tool_calls").is_some() && m["role"] == "assistant")
    {
        out.remove(0);
    }
    out
}

/// Streaming provider for a locally running Ollama server.
///
/// No credential is required: `creds.token` is empty for a local server and is
/// sent as a bearer token only when the server is remote or proxied (an SSH
/// tunnel, `ollama serve` behind auth), which `via` labels rather than the
/// provider guessing. `OLLAMA_API_KEY` is read by `turya-auth` — it owns
/// credential resolution — and arrives here as that token; the provider reads
/// exactly one environment variable, `OLLAMA_HOST`, which is an address and
/// not a secret.
pub struct OllamaProvider {
    /// Base URL of the server, without a trailing slash.
    pub base_url: String,
    /// The model name exactly as the server knows it (`qwen3.5:0.8b`).
    pub model: String,
    /// `options.num_ctx` pinned on every request; see [`DEFAULT_NUM_CTX`].
    pub num_ctx: u32,
    /// `keep_alive` pinned on every request; see [`DEFAULT_KEEP_ALIVE`].
    pub keep_alive: String,
    /// Empty for a local server; a bearer token for a remote or proxied one.
    token: String,
    /// Reasoning effort level, set mid-session through the optional
    /// `LlmProvider::set_effort` capability. Behind a lock because the
    /// provider is shared as an `Arc` and swapped while a session runs.
    effort: RwLock<Option<String>>,
}

impl OllamaProvider {
    /// A provider for a local server, with the pinned defaults.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            base_url: default_base_url(),
            model: model.into(),
            num_ctx: DEFAULT_NUM_CTX,
            keep_alive: DEFAULT_KEEP_ALIVE.to_string(),
            token: String::new(),
            effort: RwLock::new(None),
        }
    }

    /// A provider for host-resolved credentials. An empty token is normal.
    pub fn connect_with(creds: &ResolvedCreds, model: &str) -> Self {
        Self {
            token: creds.token.clone(),
            ..Self::new(model)
        }
    }

    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = normalize_base(base_url);
        self
    }

    /// Pin the context window. The catalog's discovered maximum is what a host
    /// should pass here, so the window it advertises is the one in force.
    pub fn with_num_ctx(mut self, num_ctx: u32) -> Self {
        self.num_ctx = num_ctx;
        self
    }

    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        self.keep_alive = keep_alive.into();
        self
    }

    /// Bearer header for this provider, or `None` for a credential-less server.
    fn bearer(&self) -> Option<String> {
        bearer_header(&self.token)
    }

    /// Pure request target (`{base}/api/chat`).
    fn request_target(&self) -> String {
        url(&self.base_url, "/api/chat")
    }

    /// Render the kernel's tool list into Ollama's wire shape.
    ///
    /// Nothing here is hardcoded: a tool the engine did not offer — an MCP
    /// server's, or the kernel's own `spawn_agent` — reaches the model as a
    /// real declared function. The JSON Schema key is `parameters`.
    fn tool_entries(tools: &[ToolSpec]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect()
    }

    /// The `/api/chat` request body (pure: unit-tested).
    fn request_body(&self, transcript: &Transcript, tools: &[ToolSpec]) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": wire_messages(transcript),
            // An empty list stays an empty list: the engine offered nothing,
            // and an empty array is a valid body.
            "tools": Self::tool_entries(tools),
            "stream": true,
            "keep_alive": self.keep_alive,
            // Pinned, not left to the server: a 4096-token default would
            // truncate a turn while the catalog advertises 32k.
            "options": { "num_ctx": self.num_ctx },
        });
        let effort = self.effort.read().unwrap().clone();
        if let Some(level) = effort.as_deref().and_then(think_level) {
            body["think"] = json!(level);
        }
        body
    }

    /// Feed bytes in and take every complete NDJSON frame out (pure:
    /// unit-tested).
    ///
    /// Ollama streams **newline-delimited JSON**: bare objects, one per line,
    /// with no `data:` prefix. A reader written for SSE — the shape Gemini and
    /// Anthropic use — skips every line that does not start with `data:` and
    /// silently swallows the whole response, which looks like an empty turn
    /// rather than a bug. A line that does not parse is skipped, not fatal; a
    /// trailing partial line stays buffered for the next chunk.
    ///
    /// Bytes are buffered rather than decoded per chunk, so a multi-byte
    /// character split across two chunks still decodes: a line boundary is
    /// never inside one.
    fn drain_ndjson(buf: &mut Vec<u8>, chunk: &[u8]) -> Vec<Value> {
        buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                frames.push(value);
            }
        }
        frames
    }

    /// Translate one decoded frame into steps (pure: unit-tested).
    /// `call_seq` numbers the synthetic call ids (`ocall_<n>`).
    ///
    /// `Result` rather than a bare `Vec` because Ollama reports failures
    /// *inside* the stream: a frame carrying `{"error": "..."}` holds an
    /// actionable message (`model "x" not found, try pulling it first`), and
    /// rendering that as a token would make a real failure look like a model
    /// that had nothing to say.
    fn steps_from_payload(
        payload: &Value,
        call_seq: &mut usize,
    ) -> Result<Vec<ProviderStep>, String> {
        if let Some(error) = payload.get("error").and_then(|e| e.as_str()) {
            return Err(format!("ollama: {error}"));
        }
        let mut steps = Vec::new();
        let message = match payload.get("message") {
            Some(m) => m,
            None => return Ok(steps),
        };
        // Reasoning streams as a token: there is no reasoning step, and a
        // thinking model that thinks silently looks like one that stalled.
        for key in ["thinking", "content"] {
            let text = message.get(key).and_then(|t| t.as_str()).unwrap_or("");
            // Empty text frames are the common case between tokens; emitting
            // them floods the transcript with nothing.
            if !text.is_empty() {
                steps.push(ProviderStep::Token(text.to_string()));
            }
        }
        let calls = message
            .get("tool_calls")
            .and_then(|c| c.as_array())
            .map(|c| c.as_slice())
            .unwrap_or(&[]);
        for call in calls {
            let name = call
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let arguments = call
                .pointer("/function/arguments")
                .cloned()
                .unwrap_or(json!({}));
            *call_seq += 1;
            steps.push(ProviderStep::CallTool(ToolCall {
                // Ollama's response carries no call id, so one is minted here
                // — the engine needs an id to route the result back.
                call_id: format!("ocall_{}", *call_seq),
                tool_name: name.to_string(),
                parameters: arguments,
                // Ollama neither signs calls nor requires a signature on
                // replay, so there is nothing to carry.
                signature: None,
            }));
        }
        Ok(steps)
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn set_effort(&self, level: Option<String>) {
        // Interior mutability: the provider is shared as an Arc and swapped
        // mid-session, so effort is set through a lock rather than a field.
        *self.effort.write().unwrap() = level;
    }

    async fn generate_turn(
        &self,
        transcript: &Transcript,
        tools: &[ToolSpec],
        tx: mpsc::Sender<ProviderStep>,
    ) -> Result<(), String> {
        let client = reqwest::Client::new();
        let mut req = client
            .post(self.request_target())
            .header("content-type", "application/json");
        if let Some(bearer) = self.bearer() {
            req = req.header("authorization", bearer);
        }
        // No client timeout on the turn: a long generation on a local CPU box
        // must not be cut off mid-answer.
        let resp = req
            .json(&self.request_body(transcript, tools))
            .send()
            .await
            .map_err(|e| format!("ollama request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let short: String = text.chars().take(300).collect();
            return Err(format!("ollama {status}: {short}"));
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut call_seq = 0usize;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("ollama stream read failed: {e}"))?;
            for frame in Self::drain_ndjson(&mut buf, &chunk) {
                // A mid-stream error frame ends the turn with its message.
                for step in Self::steps_from_payload(&frame, &mut call_seq)? {
                    let _ = tx.send(step).await;
                }
            }
        }
        let _ = tx.send(ProviderStep::Finish).await;
        Ok(())
    }
}

/// Registry plugin: Ollama, the server the user runs themselves.
pub struct OllamaPlugin;

impl OllamaPlugin {
    /// Everything the local server has: ids, labels, and per-model capability
    /// data from `/api/show`.
    ///
    /// Empty on any failure to *list* — the trait's contract is that a listing
    /// problem is never an error, so callers fall through to cached or static
    /// models. A probe that fails for one model is not a listing failure: that
    /// model stays in the list without capabilities, because dropping a model
    /// the user pulled over one unanswered question is the worse trade. Probes
    /// run concurrently under a short timeout, so a library of fifty models
    /// costs seconds rather than a stalled picker.
    pub async fn discover(base_url: &str, token: &str) -> Vec<DiscoveredModel> {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("turya-provider-ollama")
            .build()
        {
            Ok(client) => client,
            Err(_) => return vec![],
        };
        let mut req = client.get(url(base_url, "/api/tags"));
        if let Some(bearer) = bearer_header(token) {
            req = req.header("authorization", bearer);
        }
        let body: Value = match req.send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(body) => body,
                Err(_) => return vec![],
            },
            _ => return vec![],
        };
        let entries = match body.get("models").and_then(|m| m.as_array()) {
            Some(entries) => entries,
            None => return vec![],
        };
        let mut found: Vec<DiscoveredModel> = Vec::new();
        for entry in entries {
            let id = match entry.get("name").and_then(|n| n.as_str()) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            found.push(DiscoveredModel {
                display_name: display_name_for(&id, entry.get("details")),
                id,
                // `/api/tags` already answers capabilities and the context
                // window on a current server, so most installs need no probe
                // at all. Only what the listing stayed silent about is
                // worth a round trip.
                capabilities: capabilities_from_tags(entry),
            });
        }
        // Every model the server lists is offered; the probe is what enriches
        // an entry, not what gates it. So a model past the cap is still
        // selectable, just without capability data.
        let needs_probe: Vec<usize> = found
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.capabilities.known)
            .take(MAX_PROBED_MODELS)
            .map(|(i, _)| i)
            .collect();
        let shows = futures::future::join_all(
            needs_probe
                .iter()
                .map(|i| Self::probe(&client, base_url, token, &found[*i].id)),
        )
        .await;
        for (slot, show) in needs_probe.iter().zip(shows) {
            if let Some(show) = show {
                let from_show = capabilities_from_show(&show);
                // A probe must never erase what the listing already told us.
                found[*slot].capabilities = from_show.merged_over(found[*slot].capabilities);
            }
        }
        found
    }

    /// One `/api/show` probe. `None` on any failure, under a short timeout.
    async fn probe(
        client: &reqwest::Client,
        base_url: &str,
        token: &str,
        model: &str,
    ) -> Option<Value> {
        let request = async {
            let mut req = client
                .post(url(base_url, "/api/show"))
                .json(&json!({ "model": model }));
            if let Some(bearer) = bearer_header(token) {
                req = req.header("authorization", bearer);
            }
            let resp = req.send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.json::<Value>().await.ok()
        };
        tokio::time::timeout(PROBE_TIMEOUT, request)
            .await
            .ok()
            .flatten()
    }
}

#[async_trait]
impl ProviderPlugin for OllamaPlugin {
    fn id(&self) -> &str {
        "ollama"
    }

    fn display_name(&self) -> &str {
        "Ollama"
    }

    fn models(&self) -> Vec<ModelInfo> {
        // No static list on purpose: a hardcoded table of model names goes
        // stale against whatever the user has actually pulled, and a name that
        // is not installed fails at request time. Discovery is the feature —
        // `list_models` and `OllamaPlugin::discover` are the real source.
        vec![]
    }

    fn auth_methods(&self) -> Vec<AuthMethodKind> {
        vec![
            // A local server needs nothing; a proxied or remote one may, and
            // `turya-auth` resolves this variable into the token.
            AuthMethodKind::ApiKey {
                env_var: "OLLAMA_API_KEY",
            },
            // Declaring this is what makes an unauthenticated server
            // selectable at all: without it, "no credential" reads as an error.
            AuthMethodKind::None,
        ]
    }

    fn connect(&self, creds: ResolvedCreds, model: &str) -> Result<Arc<dyn LlmProvider>, String> {
        // An empty token is the normal case for a server the user runs, so
        // there is nothing here that can fail.
        Ok(Arc::new(OllamaProvider::connect_with(&creds, model)))
    }

    async fn list_models(&self, creds: &ResolvedCreds) -> Vec<String> {
        let base_url = default_base_url();
        Self::discover(&base_url, &creds.token)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript_with_user_text(text: &str) -> Transcript {
        let mut t = Transcript::new("s");
        t.push(Part::UserText {
            text: text.to_string(),
        });
        t
    }

    fn frames(chunk: &[u8]) -> Vec<Value> {
        let mut buf: Vec<u8> = Vec::new();
        OllamaProvider::drain_ndjson(&mut buf, chunk)
    }

    /// Captured verbatim from a real `qwen3.5:0.8b` on Ollama 0.34.4.
    ///
    /// This shape is why the listing is trusted first: the live server sends
    /// capabilities and a context window in `/api/tags`, which the published
    /// docs do not show. A probe-per-model design would make N pointless
    /// round trips on every `/models` open.
    const LIVE_TAGS_ENTRY: &str = r#"{
        "name": "qwen3.5:0.8b",
        "model": "qwen3.5:0.8b",
        "size": 1036046583,
        "details": {
            "family": "qwen35",
            "families": ["qwen35"],
            "parameter_size": "873.44M",
            "quantization_level": "Q8_0",
            "context_length": 262144,
            "embedding_length": 1024
        },
        "capabilities": ["completion", "vision", "tools", "thinking"]
    }"#;

    #[test]
    fn the_tags_listing_alone_can_describe_a_model() {
        let entry: Value = serde_json::from_str(LIVE_TAGS_ENTRY).unwrap();
        let caps = capabilities_from_tags(&entry);
        assert!(caps.known, "the listing answered, so no probe is needed");
        assert!(caps.tools);
        assert!(caps.vision);
        assert!(caps.thinking);
        assert_eq!(caps.context_window, Some(262_144));
    }

    #[test]
    fn a_silent_listing_is_distinguished_from_a_negative_one() {
        // An older server that answers neither is not evidence of absence, and
        // must still earn itself a probe.
        let bare: Value = serde_json::from_str(r#"{"name":"mystery:1b","details":{}}"#).unwrap();
        let caps = capabilities_from_tags(&bare);
        assert!(!caps.known);
        assert!(!caps.tools && !caps.vision && !caps.thinking);
        assert_eq!(caps.context_window, None);
    }

    #[test]
    fn a_probe_never_erases_what_the_listing_already_knew() {
        let from_tags = ModelCapabilities {
            tools: true,
            vision: true,
            thinking: true,
            context_window: Some(262_144),
            known: true,
        };
        // A thin /api/show answer must not downgrade a model to "cannot".
        let thin = ModelCapabilities {
            known: true,
            ..Default::default()
        };
        let merged = thin.merged_over(from_tags);
        assert!(merged.tools && merged.vision && merged.thinking);
        assert_eq!(merged.context_window, Some(262_144));
    }

    #[test]
    fn a_bare_context_length_key_is_read_as_well_as_a_dotted_one() {
        // `/api/tags` sends `details.context_length` with no architecture
        // prefix; `/api/show` sends `qwen35.context_length`. Both are real and
        // the scan must not insist on the dot.
        let bare = json!({ "context_length": 262144 });
        let dotted = json!({ "qwen35.context_length": 262144 });
        assert_eq!(context_length(&bare), Some(262_144));
        assert_eq!(context_length(&dotted), Some(262_144));
    }

    #[test]
    fn plugin_declaration_is_stable() {
        let p = OllamaPlugin;
        assert_eq!(p.id(), "ollama");
        assert_eq!(p.display_name(), "Ollama");
        assert_eq!(
            p.auth_methods(),
            vec![
                AuthMethodKind::ApiKey {
                    env_var: "OLLAMA_API_KEY"
                },
                // A server the user runs needs no credential; without this the
                // provider could never be selected.
                AuthMethodKind::None,
            ]
        );
        // No static models: a stale hardcoded name is worse than none.
        assert!(p.models().is_empty());
    }

    #[test]
    fn ndjson_frames_decode_text_thinking_tool_calls_and_done() {
        let mut seq = 0;
        let text = json!({
            "model": "qwen3.5:0.8b", "created_at": "2026-01-01T00:00:00Z", "done": false,
            "message": { "role": "assistant", "content": "The" }
        });
        let steps = OllamaProvider::steps_from_payload(&text, &mut seq).unwrap();
        assert!(matches!(&steps[..], [ProviderStep::Token(t)] if t == "The"));

        // Reasoning is a token as well: there is no reasoning step, and a
        // thinking model that thinks silently looks stalled.
        let thinking = json!({
            "done": false,
            "message": { "role": "assistant", "content": "", "thinking": "let me check" }
        });
        let steps = OllamaProvider::steps_from_payload(&thinking, &mut seq).unwrap();
        assert!(
            matches!(&steps[..], [ProviderStep::Token(t)] if t == "let me check"),
            "empty content must not add a second, empty token: {steps:?}"
        );

        // A tool call arrives whole, in one frame, with no id and no signature.
        let call = json!({
            "done": false,
            "message": {
                "role": "assistant", "content": "",
                "tool_calls": [{ "function": { "name": "view_file", "arguments": { "path": "a.rs" } } }]
            }
        });
        let steps = OllamaProvider::steps_from_payload(&call, &mut seq).unwrap();
        assert_eq!(steps.len(), 1, "{steps:?}");
        match &steps[0] {
            ProviderStep::CallTool(c) => {
                assert_eq!(
                    c.call_id, "ocall_1",
                    "the id is minted here, not by the server"
                );
                assert_eq!(c.tool_name, "view_file");
                assert_eq!(c.parameters["path"], "a.rs");
                assert_eq!(
                    c.signature, None,
                    "ollama neither signs calls nor replays a signature"
                );
            }
            other => panic!("expected a tool call, got {other:?}"),
        }

        // Two calls in one frame keep counting.
        let two = json!({
            "message": { "tool_calls": [
                { "function": { "name": "a", "arguments": {} } },
                { "function": { "name": "", "arguments": {} } },
                { "function": { "name": "b", "arguments": {} } }
            ]}
        });
        let steps = OllamaProvider::steps_from_payload(&two, &mut seq).unwrap();
        let ids: Vec<&str> = steps
            .iter()
            .map(|s| match s {
                ProviderStep::CallTool(c) => c.call_id.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            ids,
            vec!["ocall_2", "ocall_3"],
            "a nameless call is skipped"
        );

        // The final frame carries bookkeeping, not content.
        let done = json!({
            "done": true, "done_reason": "stop",
            "total_duration": 1234, "prompt_eval_count": 40, "eval_count": 9,
            "message": { "role": "assistant", "content": "" }
        });
        assert!(OllamaProvider::steps_from_payload(&done, &mut seq)
            .unwrap()
            .is_empty());

        // Unknown, empty and non-object payloads yield nothing and never panic.
        for payload in [
            json!({}),
            json!({"message": {}}),
            json!({"message": {"content": 7}}),
        ] {
            assert!(OllamaProvider::steps_from_payload(&payload, &mut seq)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn an_error_frame_is_a_real_error_not_a_token() {
        // Ollama's errors are actionable, so they must abort the turn rather
        // than render as something the model "said".
        let payload = json!({
            "error": "model \"ghost:7b\" not found, try pulling it first"
        });
        let mut seq = 0;
        let err = OllamaProvider::steps_from_payload(&payload, &mut seq)
            .expect_err("an error frame is an error");
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("ghost:7b"), "{err}");
    }

    #[test]
    fn the_stream_is_ndjson_not_sse() {
        // The regression that matters most in this crate: an SSE reader skips
        // any line that does not start with `data:`, so an Ollama response
        // parsed as SSE yields nothing at all — an empty turn, not an error.
        let body = concat!(
            "{\"model\":\"qwen3.5:0.8b\",\"done\":false,\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"}}\n",
            "\n",
            "{\"done\":false,\"message\":{\"role\":\"assistant\",\"content\":\"lo\"}}\n",
        );
        let decoded = frames(body.as_bytes());
        assert_eq!(decoded.len(), 2, "bare JSON lines, blank line skipped");
        let mut seq = 0;
        let steps: Vec<ProviderStep> = decoded
            .iter()
            .flat_map(|f| OllamaProvider::steps_from_payload(f, &mut seq).unwrap())
            .collect();
        let text: String = steps
            .iter()
            .map(|s| match s {
                ProviderStep::Token(t) => t.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(text, "Hello", "a bare JSON line must produce tokens");

        // An unparseable line is skipped, not fatal: the turn continues.
        assert_eq!(
            frames(b"not json at all\n{\"message\":{\"content\":\"ok\"}}\n").len(),
            1
        );
    }

    #[test]
    fn ndjson_survives_a_frame_split_across_chunks() {
        // Chunk boundaries fall mid-frame and mid-character; a partial line is
        // held until the rest arrives.
        let mut buf: Vec<u8> = Vec::new();
        let full = "{\"message\":{\"role\":\"assistant\",\"content\":\"héllo\"}}\n";
        let (head, tail) = full.split_at(9);
        assert!(
            OllamaProvider::drain_ndjson(&mut buf, head.as_bytes()).is_empty(),
            "a partial line is not a frame yet"
        );
        let decoded = OllamaProvider::drain_ndjson(&mut buf, tail.as_bytes());
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0]["message"]["content"], "héllo");
    }

    #[test]
    fn a_request_never_opens_with_an_assistant_tool_call() {
        // Both APIs reject a request whose first message is an assistant turn
        // carrying tool calls. Reordering history to dodge it would corrupt the
        // conversation, so the leading turn is dropped.
        let mut t = Transcript::new("s");
        t.push(Part::ToolCall {
            call_id: "c1".to_string(),
            tool_name: "run_bash".to_string(),
            arguments: json!({"command": "ls"}),
            signature: None,
        });
        t.push(Part::UserText {
            text: "hello".to_string(),
        });
        let messages = wire_messages(&t);
        assert_eq!(messages[0]["role"], "user", "{messages:?}");

        // The ordinary case is untouched: user turn, then the assistant turn
        // that carries the calls.
        let mut t = Transcript::new("s");
        t.start_turn("t1");
        t.push(Part::UserText {
            text: "list the files".to_string(),
        });
        t.push(Part::Text {
            text: "looking".to_string(),
        });
        t.push(Part::ToolCall {
            call_id: "c1".to_string(),
            tool_name: "run_bash".to_string(),
            arguments: json!({"command": "ls"}),
            signature: None,
        });
        t.push(Part::ToolResult {
            call_id: "c1".to_string(),
            output: "ok".to_string(),
            truncated: false,
        });
        let messages = wire_messages(&t);
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "list the files");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "looking");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "run_bash");
        assert_eq!(messages[2]["role"], "tool");
    }

    #[test]
    fn tool_results_are_keyed_by_name_and_ordered_by_call() {
        // Ollama matches a result to a call by name alone, so the name has to
        // be recovered from the call the result answers.
        let mut t = Transcript::new("s");
        t.push(Part::UserText {
            text: "read both".to_string(),
        });
        t.push(Part::ToolCall {
            call_id: "ocall_1".to_string(),
            tool_name: "view_file".to_string(),
            arguments: json!({"path": "a.rs"}),
            signature: None,
        });
        t.push(Part::ToolCall {
            call_id: "ocall_2".to_string(),
            tool_name: "view_file".to_string(),
            arguments: json!({"path": "b.rs"}),
            signature: None,
        });
        // The same tool twice, in one pass: two `role:"tool"` messages with the
        // same `tool_name`. Ollama cannot tell them apart — it sends no ids —
        // and no provider-side change can invent one. The results still travel
        // in call order, which is the most the wire shape allows.
        t.push(Part::ToolResult {
            call_id: "ocall_1".to_string(),
            output: "contents of a".to_string(),
            truncated: false,
        });
        t.push(Part::ToolResult {
            call_id: "ocall_2".to_string(),
            output: "contents of b".to_string(),
            truncated: false,
        });
        let messages = wire_messages(&t);
        let results: Vec<&Value> = messages.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(results.len(), 2, "{messages:?}");
        assert_eq!(results[0]["content"], "contents of a");
        assert_eq!(results[1]["content"], "contents of b");
        for r in &results {
            assert_eq!(r["tool_name"], "view_file");
        }
    }

    #[test]
    fn request_body_maps_tools_to_ollama_function_entries() {
        let t = transcript_with_user_text("hi");
        let provider = OllamaProvider::new("qwen3.5:0.8b");
        // Whatever the kernel offers is what is declared - the old version of
        // this test asserted a hardcoded list of three, which is the bug.
        let offered = vec![
            turya_core::spawn_agent_spec(),
            ToolSpec::new(
                "mcp_lookup",
                "an MCP tool the kernel discovered at runtime",
                json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            ),
        ];
        let body = provider.request_body(&t, &offered);
        assert_eq!(body["model"], "qwen3.5:0.8b");
        assert_eq!(body["stream"], true);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "declared exactly what the kernel offered");
        assert_eq!(tools[0]["type"], "function");
        let spawn = tools
            .iter()
            .find(|t| t["function"]["name"] == "spawn_agent")
            .expect("spawn_agent declared");
        // The schema must survive the translation, not just the name, and the
        // key is `parameters` - not Anthropic's `input_schema`.
        assert_eq!(spawn["function"]["parameters"]["required"][0], "name");
        assert!(spawn["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Delegate"));
        assert!(tools.iter().any(|t| t["function"]["name"] == "mcp_lookup"));

        // And an empty tool list is still a valid body, not a malformed one.
        let none = provider.request_body(&t, &[]);
        assert_eq!(none["tools"].as_array().unwrap().len(), 0);
        assert_eq!(none["messages"][0]["role"], "user");
    }

    #[test]
    fn request_body_pins_num_ctx_and_keep_alive() {
        let t = transcript_with_user_text("hi");
        let provider = OllamaProvider::new("qwen3.5:0.8b");
        let body = provider.request_body(&t, &[]);
        // The stock 5m unloads the model during a permission pause, and the
        // stock 4096-token window truncates a long turn.
        assert_eq!(body["keep_alive"], "30m");
        assert_eq!(body["options"]["num_ctx"], 32768);

        // Both are settable, so a host can pin the window it discovered.
        let pinned = provider
            .with_num_ctx(40960)
            .with_keep_alive("2h")
            .with_base_url("http://gpu-box.lan:11434/");
        let body = pinned.request_body(&t, &[]);
        assert_eq!(body["options"]["num_ctx"], 40960);
        assert_eq!(body["keep_alive"], "2h");
        assert_eq!(pinned.request_target(), "http://gpu-box.lan:11434/api/chat");
    }

    #[test]
    fn effort_maps_to_think_and_an_unknown_level_omits_it() {
        let t = transcript_with_user_text("hi");
        let provider = OllamaProvider::new("qwen3.5:0.8b");
        assert!(
            provider.request_body(&t, &[]).get("think").is_none(),
            "no effort means the server default"
        );

        for level in ["low", "high", "MAX", "medium"] {
            provider.set_effort(Some(level.to_string()));
            let level = level.to_ascii_lowercase();
            assert_eq!(provider.request_body(&t, &[])["think"], level);
        }

        // An unknown level omits the field entirely: the server default
        // applies, which is better than a guess the model cannot interpret.
        provider.set_effort(Some("bogus".to_string()));
        assert!(provider.request_body(&t, &[]).get("think").is_none());
        assert_eq!(think_level("low"), Some("low"));
        assert_eq!(think_level("extreme"), None);

        // The change is visible on a real instance through the lock, which is
        // the only way a shared provider takes it mid-session.
        let shared = Arc::new(OllamaProvider::new("m"));
        let switched = Arc::clone(&shared);
        let handle = std::thread::spawn(move || {
            switched.set_effort(Some("high".to_string()));
        });
        handle.join().unwrap();
        assert_eq!(shared.request_body(&t, &[])["think"], "high");
        shared.set_effort(None);
        assert!(shared.request_body(&t, &[]).get("think").is_none());
    }

    #[test]
    fn connect_accepts_an_empty_token_for_a_local_server() {
        let t = transcript_with_user_text("hi");
        let local = ResolvedCreds {
            token: String::new(),
            expires_at: None,
            via: "local",
        };
        // No credential is not a failure: the server is the user's own.
        let connected = OllamaPlugin.connect(local.clone(), "qwen3.5:0.8b");
        assert!(connected.is_ok(), "a local server needs no key");

        let provider = OllamaProvider::connect_with(&local, "qwen3.5:0.8b");
        assert_eq!(provider.bearer(), None, "no empty Authorization header");
        assert_eq!(provider.request_body(&t, &[])["model"], "qwen3.5:0.8b");

        // Proxied or remote: the token is sent, and the base can be moved.
        let remote = ResolvedCreds {
            token: "  tunnel-token  ".to_string(),
            expires_at: None,
            via: "env",
        };
        let provider = OllamaProvider::connect_with(&remote, "qwen3.5:0.8b")
            .with_base_url("https://ollama.example:443/");
        assert_eq!(provider.bearer().as_deref(), Some("Bearer tunnel-token"));
        assert_eq!(
            provider.request_target(),
            "https://ollama.example:443/api/chat"
        );
    }

    #[test]
    fn capabilities_and_context_length_come_from_api_show() {
        // The server's own answers, in the shape it really sends.
        let show = json!({
            "details": {
                "family": "qwen3",
                "families": ["qwen3"],
                "parameter_size": "0.8B",
                "quantization_level": "Q4_K_M",
                "format": "gguf"
            },
            "model_info": {
                "general.architecture": "qwen3",
                "qwen3.context_length": 40960,
                "qwen3.block_count": 28,
                "qwen3.embedding_length": 1024
            },
            "capabilities": ["completion", "tools", "vision", "thinking"]
        });
        let caps = capabilities_from_show(&show);
        assert_eq!(
            caps,
            ModelCapabilities {
                tools: true,
                vision: true,
                thinking: true,
                context_window: Some(40960),
                known: true,
            }
        );
        assert_eq!(
            display_name_for("qwen3.5:0.8b", show.get("details")),
            "qwen3.5:0.8b (0.8B, Q4_K_M)"
        );

        // A plain completion model says so, and says nothing about a window.
        let plain = json!({
            "details": { "family": "gemma" },
            "model_info": { "gemma.context_length": 8192 },
            "capabilities": ["completion"]
        });
        let caps = capabilities_from_show(&plain);
        assert_eq!(
            caps,
            ModelCapabilities {
                tools: false,
                vision: false,
                thinking: false,
                context_window: Some(8192),
                known: true,
            }
        );
        assert_eq!(
            display_name_for("gemma2:2b", plain.get("details")),
            "gemma2:2b"
        );
    }

    #[test]
    fn context_length_is_prefix_agnostic_and_never_guessed() {
        // The architecture prefix changes with the model family, so no one
        // family's key may be hardcoded.
        for (key, len) in [
            ("llama.context_length", 131072),
            ("qwen3.context_length", 40960),
            ("gemma2.context_length", 8192),
            ("deepseek2.context_length", 163840),
        ] {
            let info = json!({ "general.architecture": "x", key: len });
            assert_eq!(context_length(&info), Some(len), "key {key}");
        }
        // Unordered and multi-key: the widest advertised window wins.
        let both = json!({ "llama.context_length": 4096, "clip.vision.embedding_length": 1280 });
        assert_eq!(context_length(&both), Some(4096));

        // Nothing to read means no number. A guessed window decides when the
        // session compacts, and being wrong truncates turns silently.
        for info in [
            json!({}),
            json!({ "qwen3.block_count": 28 }),
            json!({ "qwen3.context_length": 0 }),
            json!({ "qwen3.context_length": "40960" }),
            json!([1, 2, 3]),
        ] {
            assert_eq!(context_length(&info), None, "{info}");
        }
        // And a payload with no `model_info` at all is unknown, not zero.
        assert_eq!(
            capabilities_from_show(&json!({})),
            ModelCapabilities::default()
        );
    }

    #[test]
    fn base_url_normalization_covers_what_ollama_host_holds() {
        assert_eq!(normalize_base(""), DEFAULT_BASE_URL);
        assert_eq!(normalize_base("   "), DEFAULT_BASE_URL);
        // OLLAMA_HOST is usually the bind interface, with no scheme.
        assert_eq!(normalize_base("0.0.0.0:11434"), "http://0.0.0.0:11434");
        assert_eq!(normalize_base("http://box:11434/"), "http://box:11434");
        assert_eq!(
            normalize_base("https://tunnel.example"),
            "https://tunnel.example"
        );
        assert_eq!(
            url(&normalize_base("box:11434/"), "/api/tags"),
            "http://box:11434/api/tags"
        );
    }
}
