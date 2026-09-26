//! End-to-end against a real Ollama daemon.
//!
//! Skipped unless `TURYA_LIVE_OLLAMA=1`. This is the only test that can
//! catch a wire-format assumption that the published docs get wrong - and it
//! already did: a 0.34.4 server sends capabilities and a context window in
//! `/api/tags` that the docs do not mention.

use std::sync::Arc;

use turya_core::{LlmProvider, ProviderPlugin, ProviderStep};
use turya_protocol::{Part, ToolSpec, Transcript};
use turya_provider_ollama::{OllamaPlugin, OllamaProvider};

fn live() -> bool {
    std::env::var("TURYA_LIVE_OLLAMA").ok().as_deref() == Some("1")
}

/// A model the caller has actually pulled. Without one there is nothing to say.
fn model() -> Option<String> {
    std::env::var("TURYA_OLLAMA_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
}

#[tokio::test]
async fn the_server_is_reachable_and_lists_a_model() {
    if !live() {
        eprintln!("skip: set TURYA_LIVE_OLLAMA=1");
        return;
    }
    let plugin = OllamaPlugin;
    let creds = turya_core::ResolvedCreds {
        token: String::new(),
        expires_at: None,
        via: "local",
    };
    let ids = plugin.list_models(&creds).await;
    assert!(
        !ids.is_empty(),
        "discovery returned nothing — is the daemon up and a model pulled?"
    );
    assert!(
        ids.iter().any(|id| Some(id) == model().as_ref()),
        "the requested model {:?} is not installed; found {ids:?}",
        model()
    );
}

#[tokio::test]
async fn discovery_reports_capabilities_and_a_context_window() {
    if !live() {
        eprintln!("skip: set TURYA_LIVE_OLLAMA=1");
        return;
    }
    let found = OllamaPlugin::discover("http://127.0.0.1:11434", "").await;
    let m = match model() {
        Some(want) => found.into_iter().find(|m| m.id == want),
        None => found.into_iter().next(),
    };
    let m = m.expect("a discovered model");
    assert!(
        m.capabilities.known,
        "{}: the server said nothing about it",
        m.id
    );
    assert!(
        m.capabilities.context_window.is_some(),
        "{}: no context window reported",
        m.id
    );
    // A local model is worthless to an agent without tool calling, so if the
    // server says it has tools, we must have read that.
    println!(
        "{} -> tools={} vision={} thinking={} ctx={:?}",
        m.id,
        m.capabilities.tools,
        m.capabilities.vision,
        m.capabilities.thinking,
        m.capabilities.context_window
    );
}

#[tokio::test]
async fn a_real_turn_streams_text() {
    if !live() {
        eprintln!("skip: set TURYA_LIVE_OLLAMA=1");
        return;
    }
    let Some(id) = model() else {
        eprintln!("skip: no TURYA_OLLAMA_MODEL");
        return;
    };
    let provider = OllamaProvider::new(id);
    let mut t = Transcript::new("live");
    t.push(Part::UserText {
        text: "Reply with exactly the word PONG and nothing else.".to_string(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let handle = {
        let provider: Arc<dyn LlmProvider> = Arc::new(provider);
        tokio::spawn(async move { provider.generate_turn(&t, &[], tx).await })
    };
    let mut text = String::new();
    let mut finished = false;
    while let Some(step) = rx.recv().await {
        match step {
            ProviderStep::Token(t) => text.push_str(&t),
            ProviderStep::Finish => {
                finished = true;
                break;
            }
            ProviderStep::CallTool(_) => {}
        }
    }
    handle
        .await
        .expect("provider task")
        .expect("turn must not error");
    assert!(finished, "the provider must always send Finish");
    assert!(
        text.to_lowercase().contains("pong"),
        "expected PONG in the streamed text, got: {text:?}"
    );
}

#[tokio::test]
async fn a_tool_is_declared_and_can_be_called_back() {
    // The two halves of the tool path against a real server: the model sees
    // the declaration, and the history we send back is accepted. A provider
    // that mangles `role:"tool"` gets a 400 here.
    if !live() {
        eprintln!("skip: set TURYA_LIVE_OLLAMA=1");
        return;
    }
    let Some(id) = model() else {
        eprintln!("skip: no TURYA_OLLAMA_MODEL");
        return;
    };
    if !OllamaPlugin::discover("http://127.0.0.1:11434", "")
        .await
        .iter()
        .any(|m| m.id == id && m.capabilities.tools)
    {
        eprintln!("skip: {id} does not support tools");
        return;
    }
    let tools = vec![ToolSpec::new(
        "get_fruit_colour",
        "Return the colour of a fruit. Use this instead of guessing.",
        serde_json::json!({
            "type": "object",
            "properties": { "fruit": { "type": "string", "description": "the fruit" } },
            "required": ["fruit"]
        }),
    )];
    let provider = OllamaProvider::new(id);
    let mut t = Transcript::new("live-tools");
    t.push(Part::UserText {
        text: "What colour is a banana? Use get_fruit_colour.".to_string(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let handle = {
        let provider: Arc<dyn LlmProvider> = Arc::new(provider);
        tokio::spawn(async move { provider.generate_turn(&t, &tools, tx).await })
    };
    let mut called = false;
    while let Some(step) = rx.recv().await {
        if let ProviderStep::CallTool(c) = step {
            called = true;
            assert_eq!(c.tool_name, "get_fruit_colour");
            assert_eq!(c.parameters["fruit"], "banana");
        }
    }
    handle
        .await
        .expect("provider task")
        .expect("turn must not error");
    assert!(called, "a tool-capable model should have called the tool");
}

#[tokio::test]
async fn a_tool_result_history_is_accepted_by_the_server() {
    // The failure this guards is silent and ugly: a malformed `role:"tool"`
    // message does not error cleanly, it just makes the model behave as
    // though the turn never happened.
    if !live() {
        eprintln!("skip: set TURYA_LIVE_OLLAMA=1");
        return;
    }
    let Some(id) = model() else {
        eprintln!("skip: no TURYA_OLLAMA_MODEL");
        return;
    };
    let mut t = Transcript::new("live-history");
    t.push(Part::UserText {
        text: "What colour is a banana?".to_string(),
    });
    t.push(Part::Text {
        text: String::new(),
    });
    t.push(Part::ToolCall {
        call_id: "ocall_1".to_string(),
        tool_name: "get_fruit_colour".to_string(),
        arguments: serde_json::json!({ "fruit": "banana" }),
        signature: None,
    });
    t.push(Part::ToolResult {
        call_id: "ocall_1".to_string(),
        output: "BANANAS-ARE-YELLOW".to_string(),
        truncated: false,
    });
    t.push(Part::UserText {
        text: "Now say only: GOT-IT".to_string(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let handle = {
        let provider: Arc<dyn LlmProvider> = Arc::new(OllamaProvider::new(id));
        tokio::spawn(async move { provider.generate_turn(&t, &[], tx).await })
    };
    let mut text = String::new();
    while let Some(step) = rx.recv().await {
        if let ProviderStep::Token(t) = step {
            text.push_str(&t);
        }
    }
    let result = handle.await.expect("provider task");
    assert!(
        result.is_ok(),
        "a tool-result history must be accepted: {result:?}"
    );
    assert!(
        !text.is_empty(),
        "the model must say something after a tool result"
    );
}
