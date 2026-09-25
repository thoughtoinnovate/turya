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
}
