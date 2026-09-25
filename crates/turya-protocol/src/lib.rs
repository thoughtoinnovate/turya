use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentMode {
    Plan,
    Build,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Open,
    ReviewForMe,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Moderate,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticItem {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
    pub severity: String,
}

/// Commands sent from any UI/Client to the Turya Core Engine
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum TuryaCommand {
    SubmitPrompt {
        prompt: String,
        mode: AgentMode,
    },
    ResolvePermission {
        request_id: String,
        decision: PermissionDecision,
    },
    AbortTurn,
    UpdateConfig {
        permission_mode: Option<PermissionMode>,
        provider: Option<String>,
        model: Option<String>,
    },
    /// List registered providers and their models (drives `/models`).
    ListProviders,
    /// Query dual-slot auth state for one provider (drives `/auth` badges).
    GetAuthStatus {
        provider: String,
    },
    /// Start an auth flow; the engine answers with `AuthFlowStarted`.
    BeginAuthFlow {
        provider: String,
        method: String,
    },
    /// Deliver one user input (key text, pasted code) to a running flow.
    SubmitAuthInput {
        flow_id: String,
        payload: String,
    },
    /// Abandon a running auth flow.
    CancelAuthFlow {
        flow_id: String,
    },
}

/// Events broadcast by the Turya Core Engine to all connected UIs/Clients
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum TuryaEvent {
    TurnStarted {
        turn_id: String,
        mode: AgentMode,
    },
    TokenDelta {
        chunk: String,
    },
    ToolCallInitiated(ToolCall),
    ToolCallCompleted(ToolResult),
    PermissionRequested {
        request_id: String,
        action: String,
        risk_level: RiskLevel,
        details: String,
    },
    DiagnosticsReceived {
        diagnostics: Vec<DiagnosticItem>,
    },
    TurnCompleted {
        turn_id: String,
        success: bool,
    },
    Error {
        message: String,
    },
    /// Registry snapshot answering `ListProviders`.
    ProvidersListed {
        providers: Vec<ProviderSummary>,
    },
    /// Dual-slot auth state answering `GetAuthStatus` (also pushed on change).
    AuthStatusChanged {
        provider: String,
        api_key: String,
        oauth: String,
    },
    /// An auth flow needs a user action; token exchange stays server-side.
    AuthFlowStarted {
        flow_id: String,
        action: AuthAction,
    },
    AuthFlowCompleted {
        flow_id: String,
        provider: String,
        method: String,
    },
    AuthFlowFailed {
        flow_id: String,
        reason: String,
    },
    /// Model catalog changed for a provider (refresh finished).
    CatalogUpdated {
        provider: String,
    },
}

/// One provider + its models, as shown in the `/models` browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub models: Vec<ModelSummary>,
    /// Slot badges: `"env" | "stored" | "connected" | "missing" | "unsupported"`.
    pub api_key: String,
    pub oauth: String,
}

/// One model in the `/models` browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub id: String,
    pub display_name: String,
    /// `live | cached | snapshot | static`.
    pub source: String,
}

/// User action required to advance an auth flow (rendered by the client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum AuthAction {
    /// Open this URL in a browser; the callback is captured server-side.
    OpenBrowser { url: String },
    /// Prompt for masked text input (API key or pasted auth code).
    PromptMasked { prompt: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialization_roundtrip() {
        let cmd = TuryaCommand::SubmitPrompt {
            prompt: "Refactor auth".to_string(),
            mode: AgentMode::Build,
        };
        let serialized = serde_json::to_string(&cmd).unwrap();
        assert!(serialized.contains("SubmitPrompt"));
    }

    #[test]
    fn test_provider_auth_messages_roundtrip() {
        // New variants must serialize with stable tags (additive-only schema).
        let cmds = vec![
            TuryaCommand::ListProviders,
            TuryaCommand::GetAuthStatus {
                provider: "gemini".to_string(),
            },
            TuryaCommand::BeginAuthFlow {
                provider: "gemini".to_string(),
                method: "api-key".to_string(),
            },
            TuryaCommand::UpdateConfig {
                permission_mode: None,
                provider: Some("gemini".to_string()),
                model: Some("gemini-2.5-pro".to_string()),
            },
        ];
        for cmd in cmds {
            let s = serde_json::to_string(&cmd).unwrap();
            let back: TuryaCommand = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), s);
        }
        let evt = TuryaEvent::AuthFlowStarted {
            flow_id: "f1".to_string(),
            action: AuthAction::OpenBrowser {
                url: "https://example.test/auth".to_string(),
            },
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains("AuthFlowStarted") && s.contains("OpenBrowser"));
        let listed = TuryaEvent::ProvidersListed {
            providers: vec![ProviderSummary {
                id: "gemini".to_string(),
                display_name: "Gemini".to_string(),
                models: vec![ModelSummary {
                    id: "gemini-2.5-pro".to_string(),
                    display_name: "Gemini 2.5 Pro".to_string(),
                    source: "static".to_string(),
                }],
                api_key: "missing".to_string(),
                oauth: "missing".to_string(),
            }],
        };
        let s = serde_json::to_string(&listed).unwrap();
        let back: TuryaEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);
    }
}
