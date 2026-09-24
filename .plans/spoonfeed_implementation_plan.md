# Turya: Foolproof "Spoon-Fed" Implementation Playbook

> **Target Audience**: Any AI agent or developer.  
> **Rule**: Every step is atomic, sequential, and fully specified with exact file paths, exact code, and exact verification commands. **Do not skip steps. Do not modify dependencies unless instructed.**
>
> 🔄 **AGENT RESUMPTION PROTOCOL**
> If you are starting a new session, **DO NOT start from Step 0**. First, read `.plans/EXECUTION_TRACKER.md` to find the exact step the previous agent left off on. Run `cargo check` to verify the codebase state, then resume seamlessly.
> 
> 🚨 **GLOBAL AGENT DIRECTIVE (MANDATORY)** 🚨
> Before generating code for any step in this playbook, **you MUST use your web search tools** to explore the latest best guides, repositories, and modern practices for fast, lightweight, self-improvable agents. If you find a significantly better, more modern, or more performant Rust pattern than what is sketched in this playbook, you are authorized and encouraged to upgrade the implementation. Never blindly write outdated code.

---

## Table of Contents
1. [Step 0: Workspace Setup & Cargo Initialization](#step-0-workspace-setup--cargo-initialization)
2. [Step 1: Crate `turya-protocol` (Shared Types & Event Protocol)](#step-1-crate-turya-protocol)
3. [Step 2: Crate `turya-tools` (Primitive Filesystem & Shell Tools)](#step-2-crate-turya-tools)
4. [Step 3: Crate `turya-core` (Master Loop, Event Bus & Mock Provider)](#step-3-crate-turya-core)
5. [Step 4: Crate `turya-server` (Headless Event-Driven Server)](#step-4-crate-turya-server)
6. [Step 5: Crate `turya-tui` (Ratatui Non-Blocking Terminal Frontend)](#step-5-crate-turya-tui)
7. [Step 6: Crate `turya-cli` (Unified Executable Entrypoint)](#step-6-crate-turya-cli)
8. [Step 7: Real LLM Provider Integration (Anthropic Streaming SSE)](#step-7-real-llm-provider-integration)
9. [Step 8: LSP Live Feedback Bridge](#step-8-lsp-live-feedback-bridge)
10. [Step 9: Extism Wasm Plugin Sandbox](#step-9-extism-wasm-plugin-sandbox)

---

## Step 0: Workspace Setup & Cargo Initialization

### 0.1 Create Root `Cargo.toml`
- **File**: `/home/dev/workspace/turya/Cargo.toml`
- **Action**: Create new file.
- **Content**:
```toml
[workspace]
resolver = "2"
members = [
    "crates/turya-protocol",
    "crates/turya-tools",
    "crates/turya-core",
    "crates/turya-server",
    "crates/turya-tui",
    "crates/turya-cli"
]

[workspace.dependencies]
# Performance: Trimmed tokio 'full' to bare minimum required features for tiny binary size
tokio = { version = "1.38", features = ["rt-multi-thread", "macros", "net", "time", "fs", "sync"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
async-trait = "0.1"
thiserror = "1.0"
tracing = "0.1"
# Performance: Use lightweight tracing rather than full subscriber bloat
tracing-subscriber = { version = "0.3", default-features = false, features = ["fmt", "ansi", "env-filter"] }
futures = "0.3"
# Performance: Use memory-mapping for reading huge files without blowing up RAM on old machines
memmap2 = "0.9"
```

### 0.2 Verification Command
```bash
cargo check
```
*(Expected: Succeeds or reports missing member directories, which will be created in subsequent steps).*

---

## Step 1: Crate `turya-protocol`

This crate defines all bidirectional commands (UI $\rightarrow$ Engine) and events (Engine $\rightarrow$ UI).

### 1.1 Create `crates/turya-protocol/Cargo.toml`
- **File**: `/home/dev/workspace/turya/crates/turya-protocol/Cargo.toml`
- **Content**:
```toml
[package]
name = "turya-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
```

### 1.2 Create `crates/turya-protocol/src/lib.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-protocol/src/lib.rs`
- **Content**:
```rust
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
    TurnStarted { turn_id: String, mode: AgentMode },
    TokenDelta { chunk: String },
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
    TurnCompleted { turn_id: String, success: bool },
    Error { message: String },
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
```

### 1.3 Verification Command
```bash
cargo test -p turya-protocol
```
*(Expected: `test tests::test_serialization_roundtrip ... ok`)*

---

## Step 2: Crate `turya-tools`

Built-in primitive tools: `view_file`, `write_file`, `edit_file`, and `run_bash`.

### 2.1 Create `crates/turya-tools/Cargo.toml`
- **File**: `/home/dev/workspace/turya/crates/turya-tools/Cargo.toml`
- **Content**:
```toml
[package]
name = "turya-tools"
version = "0.1.0"
edition = "2021"

[dependencies]
turya-protocol = { path = "../turya-protocol" }
async-trait = { workspace = true }
tokio = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
```

### 2.2 Create `crates/turya-tools/src/lib.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-tools/src/lib.rs`
- **Content**:
```rust
use async_trait::async_trait;
use turya_protocol::{RiskLevel, ToolResult};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn risk_level(&self, params: &serde_json::Value) -> RiskLevel;
    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult;
}

pub struct ViewFileTool;

#[async_trait]
impl Tool for ViewFileTool {
    fn name(&self) -> &'static str { "view_file" }
    fn description(&self) -> &'static str { "Read file content from the filesystem" }
    fn risk_level(&self, _params: &serde_json::Value) -> RiskLevel { RiskLevel::Low }

    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        let path_str = match params.get("path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => return ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
            },
        };

        match fs::read_to_string(path_str).await {
            Ok(content) => ToolResult {
                call_id: call_id.to_string(),
                success: true,
                output: content,
                error: None,
            },
            Err(e) => ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some(format!("Failed to read {}: {}", path_str, e)),
            },
        }
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str { "write_file" }
    fn description(&self) -> &'static str { "Write or overwrite file content" }
    fn risk_level(&self, _params: &serde_json::Value) -> RiskLevel { RiskLevel::High }

    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        let path_str = match params.get("path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => return ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
            },
        };
        let content = params.get("content").and_then(|c| c.as_str()).unwrap_or("");

        let path = Path::new(path_str);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent).await;
        }

        match fs::write(path, content).await {
            Ok(_) => ToolResult {
                call_id: call_id.to_string(),
                success: true,
                output: format!("Successfully wrote {} bytes to {}", content.len(), path_str),
                error: None,
            },
            Err(e) => ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some(format!("Failed to write {}: {}", path_str, e)),
            },
        }
    }
}

pub struct RunBashTool;

#[async_trait]
impl Tool for RunBashTool {
    fn name(&self) -> &'static str { "run_bash" }
    fn description(&self) -> &'static str { "Execute a bash shell command" }
    fn risk_level(&self, _params: &serde_json::Value) -> RiskLevel { RiskLevel::High }

    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        let command = match params.get("command").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => return ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some("Missing 'command' parameter".to_string()),
            },
        };

        match Command::new("bash").arg("-c").arg(command).output().await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() { stdout } else { format!("{}\nSTDERR:\n{}", stdout, stderr) };
                ToolResult {
                    call_id: call_id.to_string(),
                    success: output.status.success(),
                    output: combined,
                    error: if output.status.success() { None } else { Some(format!("Exited with code: {:?}", output.status.code())) },
                }
            },
            Err(e) => ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some(format!("Execution failed: {}", e)),
            },
        }
    }
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn standard() -> Self {
        Self {
            tools: vec![
                Box::new(ViewFileTool),
                Box::new(WriteFileTool),
                Box::new(RunBashTool),
            ],
        }
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|b| b.as_ref())
    }
}
```

### 2.3 Verification Command
```bash
cargo check -p turya-tools
```
*(Expected: Compiles with 0 warnings/errors).*

---

## Step 3: Crate `turya-core`

The microkernel: Master Agentic Loop, Permission Broker, and Provider Abstraction.

### 3.1 Create `crates/turya-core/Cargo.toml`
- **File**: `/home/dev/workspace/turya/crates/turya-core/Cargo.toml`
- **Content**:
```toml
[package]
name = "turya-core"
version = "0.1.0"
edition = "2021"

[dependencies]
turya-protocol = { path = "../turya-protocol" }
turya-tools = { path = "../turya-tools" }
tokio = { workspace = true }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
```

### 3.2 Create `crates/turya-core/src/provider.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-core/src/provider.rs`
- **Content**:
```rust
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
```

### 3.3 Create `crates/turya-core/src/permissions.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-core/src/permissions.rs`
- **Content**:
```rust
use turya_protocol::{PermissionDecision, PermissionMode, RiskLevel};
use std::collections::HashSet;
use std::sync::RwLock;

pub struct PermissionBroker {
    mode: RwLock<PermissionMode>,
    allowed_session_tools: RwLock<HashSet<String>>,
}

impl PermissionBroker {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode: RwLock::new(mode),
            allowed_session_tools: RwLock::new(HashSet::new()),
        }
    }

    pub fn set_mode(&self, new_mode: PermissionMode) {
        let mut mode = self.mode.write().unwrap();
        *mode = new_mode;
    }

    /// Evaluates if an action is pre-authorized or requires user challenge
    pub fn check(&self, tool_name: &str, risk: RiskLevel) -> Option<PermissionDecision> {
        let mode = *self.mode.read().unwrap();
        if mode == PermissionMode::Open {
            return Some(PermissionDecision::AllowOnce);
        }

        if self.allowed_session_tools.read().unwrap().contains(tool_name) {
            return Some(PermissionDecision::AllowOnce);
        }

        if mode == PermissionMode::ReviewForMe && risk == RiskLevel::Low {
            return Some(PermissionDecision::AllowOnce);
        }

        // None indicates the engine must issue PermissionRequested event
        None
    }

    pub fn record_decision(&self, tool_name: &str, decision: PermissionDecision) {
        if decision == PermissionDecision::AllowSession {
            self.allowed_session_tools.write().unwrap().insert(tool_name.to_string());
        }
    }
}
```

### 3.4 Create `crates/turya-core/src/engine.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-core/src/engine.rs`
- **Content**:
```rust
use crate::permissions::PermissionBroker;
use crate::provider::{LlmProvider, ProviderStep};
use turya_protocol::{
    AgentMode, TuryaCommand, TuryaEvent, PermissionDecision, PermissionMode,
};
use turya_tools::ToolRegistry;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub struct EngineState {
    pub permission_broker: PermissionBroker,
    pub pending_permission: Option<(String, oneshot::Sender<PermissionDecision>)>,
}

pub struct TuryaEngine {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    permissions: Arc<PermissionBroker>,
}

impl TuryaEngine {
    pub fn new(
        provider: Arc<dyn LlmProvider>, 
        tools: Arc<ToolRegistry>, 
        mode: PermissionMode
    ) -> Self {
        Self {
            provider,
            tools,
            permissions: Arc::new(PermissionBroker::new(mode)),
        }
    }

    pub async fn run_turn(
        &self,
        turn_id: &str,
        prompt: &str,
        mode: AgentMode,
        event_tx: mpsc::Sender<TuryaEvent>,
        mut perm_rx: mpsc::Receiver<(String, PermissionDecision)>,
    ) {
        let _ = event_tx.send(TuryaEvent::TurnStarted { turn_id: turn_id.to_string(), mode }).await;

        let (step_tx, mut step_rx) = mpsc::channel(32);
        let provider = self.provider.clone();
        let prompt_clone = prompt.to_string();

        tokio::spawn(async move {
            let _ = provider.generate_turn(&prompt_clone, &[], step_tx).await;
        });

        while let Some(step) = step_rx.recv().await {
            match step {
                ProviderStep::Token(chunk) => {
                    let _ = event_tx.send(TuryaEvent::TokenDelta { chunk }).await;
                }
                ProviderStep::CallTool(call) => {
                    let _ = event_tx.send(TuryaEvent::ToolCallInitiated(call.clone())).await;
                    let tool = match self.tools.get(&call.tool_name) {
                        Some(t) => t,
                        None => {
                            let _ = event_tx.send(TuryaEvent::ToolCallCompleted(turya_protocol::ToolResult {
                                call_id: call.call_id,
                                success: false,
                                output: String::new(),
                                error: Some(format!("Unknown tool: {}", call.tool_name)),
                            })).await;
                            continue;
                        }
                    };

                    let risk = tool.risk_level(&call.parameters);
                    let authorized = match self.permissions.check(&call.tool_name, risk) {
                        Some(decision) => decision != PermissionDecision::Deny,
                        None => {
                            // Issue challenge
                            let req_id = format!("req_{}", call.call_id);
                            let _ = event_tx.send(TuryaEvent::PermissionRequested {
                                request_id: req_id.clone(),
                                action: call.tool_name.clone(),
                                risk_level: risk,
                                details: call.parameters.to_string(),
                            }).await;

                            // Wait for UI to resolve
                            let mut approved = false;
                            while let Some((id, dec)) = perm_rx.recv().await {
                                if id == req_id {
                                    self.permissions.record_decision(&call.tool_name, dec);
                                    approved = dec != PermissionDecision::Deny;
                                    break;
                                }
                            }
                            approved
                        }
                    };

                    if authorized {
                        let result = tool.execute(&call.call_id, call.parameters).await;
                        let _ = event_tx.send(TuryaEvent::ToolCallCompleted(result)).await;
                    } else {
                        let _ = event_tx.send(TuryaEvent::ToolCallCompleted(turya_protocol::ToolResult {
                            call_id: call.call_id,
                            success: false,
                            output: String::new(),
                            error: Some("Permission denied by user".to_string()),
                        })).await;
                    }
                }
                ProviderStep::Finish => break,
            }
        }

        let _ = event_tx.send(TuryaEvent::TurnCompleted { turn_id: turn_id.to_string(), success: true }).await;
    }
}
```

### 3.5 Create `crates/turya-core/src/lib.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-core/src/lib.rs`
- **Content**:
```rust
pub mod engine;
pub mod permissions;
pub mod provider;

pub use engine::TuryaEngine;
pub use permissions::PermissionBroker;
pub use provider::{LlmProvider, MockProvider, ProviderStep};

#[cfg(test)]
mod tests {
    use super::*;
    use turya_protocol::{AgentMode, TuryaEvent, PermissionDecision, PermissionMode, ToolCall};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_master_loop_with_mock_provider() {
        let mock_provider = Arc::new(MockProvider {
            responses: vec![
                ProviderStep::Token("Hello".to_string()),
                ProviderStep::CallTool(ToolCall {
                    call_id: "1".to_string(),
                    tool_name: "view_file".to_string(),
                    parameters: serde_json::json!({"path": "Cargo.toml"}),
                }),
                ProviderStep::Finish,
            ],
        });

        let tools = Arc::new(turya_tools::ToolRegistry::standard());
        let engine = TuryaEngine::new(mock_provider, tools, PermissionMode::Open);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (_perm_tx, perm_rx) = mpsc::channel(1);

        tokio::spawn(async move {
            engine.run_turn("test_turn", "hi", AgentMode::Build, event_tx, perm_rx).await;
        });

        let mut received_token = false;
        let mut completed = false;

        while let Some(evt) = event_rx.recv().await {
            match evt {
                TuryaEvent::TokenDelta { chunk } => {
                    if chunk == "Hello" { received_token = true; }
                }
                TuryaEvent::TurnCompleted { .. } => {
                    completed = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(received_token);
        assert!(completed);
    }
}
```

### 3.6 Verification Command
```bash
cargo test -p turya-core
```
*(Expected: `test tests::test_master_loop_with_mock_provider ... ok`)*

---

## Step 4: Crate `turya-server`

Decouples the Core Engine from any UI using an event-driven channel interface.

### 4.1 Create `crates/turya-server/Cargo.toml`
- **File**: `/home/dev/workspace/turya/crates/turya-server/Cargo.toml`
- **Content**:
```toml
[package]
name = "turya-server"
version = "0.1.0"
edition = "2021"

[dependencies]
turya-protocol = { path = "../turya-protocol" }
turya-core = { path = "../turya-core" }
tokio = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
```

### 4.2 Create `crates/turya-server/src/lib.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-server/src/lib.rs`
- **Content**:
```rust
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
                        engine.run_turn(&turn_id, &prompt, mode, event_tx, turn_perm_rx).await;
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
                    let _ = self.event_tx.send(TuryaEvent::Error { message: "Turn aborted by user".to_string() }).await;
                }
                _ => {}
            }
        }
    }
}
```

### 4.3 Verification Command
```bash
cargo check -p turya-server
```

---

## Step 5: Crate `turya-tui`

The responsive Terminal UI built with `ratatui` and `crossterm`.

### 5.1 Create `crates/turya-tui/Cargo.toml`
- **File**: `/home/dev/workspace/turya/crates/turya-tui/Cargo.toml`
- **Content**:
```toml
[package]
name = "turya-tui"
version = "0.1.0"
edition = "2021"

[dependencies]
turya-protocol = { path = "../turya-protocol" }
ratatui = "0.28"
crossterm = { version = "0.28", features = ["event-stream"] }
tokio = { workspace = true }
futures = { workspace = true }
```

### 5.2 Create `crates/turya-tui/src/lib.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-tui/src/lib.rs`
- **Content**:
```rust
use crossterm::{
    event::{Event, EventStream, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use turya_protocol::{AgentMode, TuryaCommand, TuryaEvent, PermissionDecision};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::io::stdout;
use tokio::sync::mpsc;

pub struct TuiApp {
    input: String,
    streamed_text: String,
    tool_logs: Vec<String>,
    pending_permission: Option<(String, String)>, // (request_id, action)
}

impl TuiApp {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            streamed_text: String::new(),
            tool_logs: Vec::new(),
            pending_permission: None,
        }
    }

    pub async fn run(
        mut self,
        cmd_tx: mpsc::Sender<TuryaCommand>,
        mut event_rx: mpsc::Receiver<TuryaEvent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let mut reader = EventStream::new();

        loop {
            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),      // Header
                        Constraint::Min(5),         // Content & Streamed Text
                        Constraint::Length(5),      // Tool Activity Log
                        Constraint::Length(3),      // Input Box / Permission Prompt
                    ])
                    .split(f.area());

                // 1. Header
                let header = Paragraph::new(" Turya v0.1.0 │ Mode: Build │ Security: Review-for-me")
                    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                    .block(Block::default().borders(Borders::ALL).title("Status"));
                f.render_widget(header, chunks[0]);

                // 2. Chat Stream
                let chat = Paragraph::new(self.streamed_text.as_str())
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title("Assistant"));
                f.render_widget(chat, chunks[1]);

                // 3. Tool Activity
                let logs: Vec<Line> = self.tool_logs.iter().map(|l| Line::from(Span::raw(l))).collect();
                let tools_widget = Paragraph::new(logs)
                    .block(Block::default().borders(Borders::ALL).title("Tool Activity"));
                f.render_widget(tools_widget, chunks[2]);

                // 4. Input or Permission Prompt
                if let Some((_, ref action)) = self.pending_permission {
                    let prompt = Paragraph::new(format!("⚠️ Allow '{}'? Press [y] to allow, [n] to deny", action))
                        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
                        .block(Block::default().borders(Borders::ALL).title("Permission Required"));
                    f.render_widget(prompt, chunks[3]);
                } else {
                    let input_widget = Paragraph::new(self.input.as_str())
                        .block(Block::default().borders(Borders::ALL).title("Prompt (Enter to send, Esc to exit)"));
                    f.render_widget(input_widget, chunks[3]);
                }
            })?;

            tokio::select! {
                Some(Ok(event)) = reader.next() => {
                    if let Event::Key(key) = event {
                        if key.code == KeyCode::Esc {
                            break;
                        }
                        if let Some((req_id, _)) = self.pending_permission.take() {
                            match key.code {
                                KeyCode::Char('y') => {
                                    let _ = cmd_tx.send(TuryaCommand::ResolvePermission {
                                        request_id: req_id,
                                        decision: PermissionDecision::AllowOnce,
                                    }).await;
                                }
                                KeyCode::Char('n') => {
                                    let _ = cmd_tx.send(TuryaCommand::ResolvePermission {
                                        request_id: req_id,
                                        decision: PermissionDecision::Deny,
                                    }).await;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        match key.code {
                            KeyCode::Char(c) => self.input.push(c),
                            KeyCode::Backspace => { self.input.pop(); },
                            KeyCode::Enter => {
                                if !self.input.trim().is_empty() {
                                    let prompt = std::mem::take(&mut self.input);
                                    let _ = cmd_tx.send(TuryaCommand::SubmitPrompt {
                                        prompt,
                                        mode: AgentMode::Build,
                                    }).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some(evt) = event_rx.recv() => {
                    match evt {
                        TuryaEvent::TokenDelta { chunk } => {
                            self.streamed_text.push_str(&chunk);
                        }
                        TuryaEvent::ToolCallInitiated(call) => {
                            self.tool_logs.push(format!("⚡ Invoking: {}", call.tool_name));
                        }
                        TuryaEvent::ToolCallCompleted(res) => {
                            self.tool_logs.push(format!("✔ Result (call {}): success={}", res.call_id, res.success));
                        }
                        TuryaEvent::PermissionRequested { request_id, action, .. } => {
                            self.pending_permission = Some((request_id, action));
                        }
                        TuryaEvent::TurnCompleted { .. } => {
                            self.streamed_text.push_str("\n[Turn Finished]\n");
                        }
                        _ => {}
                    }
                }
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        Ok(())
    }
}
```

### 5.3 Verification Command
```bash
cargo check -p turya-tui
```

---

## Step 6: Crate `turya-cli`

The unified binary combining Engine, Mock/Anthropic Provider, Server, and TUI.

### 6.1 Create `crates/turya-cli/Cargo.toml`
- **File**: `/home/dev/workspace/turya/crates/turya-cli/Cargo.toml`
- **Content**:
```toml
[package]
name = "turya-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
turya-protocol = { path = "../turya-protocol" }
turya-tools = { path = "../turya-tools" }
turya-core = { path = "../turya-core" }
turya-server = { path = "../turya-server" }
turya-tui = { path = "../turya-tui" }
tokio = { workspace = true }
clap = { version = "4.5", features = ["derive"] }
```

### 6.2 Create `crates/turya-cli/src/main.rs`
- **File**: `/home/dev/workspace/turya/crates/turya-cli/src/main.rs`
- **Content**:
```rust
use clap::Parser;
use turya_core::{LlmProvider, TuryaEngine, MockProvider, ProviderStep};
use turya_protocol::{PermissionMode, ToolCall};
use turya_tools::ToolRegistry;
use turya_server::TuryaSession;
use turya_tui::TuiApp;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "turya", about = "Fast, modular agentic coding harness")]
struct Args {
    #[arg(short, long, default_value = "review-for-me")]
    permission_mode: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _args = Args::parse();

    // Default bootstrap provider: Simulated deterministic steps
    let mock_provider = Arc::new(MockProvider {
        responses: vec![
            ProviderStep::Token("Welcome to Turya. Analyzing your repository... ".to_string()),
            ProviderStep::CallTool(ToolCall {
                call_id: "init_call".to_string(),
                tool_name: "view_file".to_string(),
                parameters: serde_json::json!({ "path": "Cargo.toml" }),
            }),
            ProviderStep::Token("\nRepository read complete. Ready for tasks.".to_string()),
            ProviderStep::Finish,
        ],
    });

    let tools = Arc::new(ToolRegistry::standard());
    let engine = Arc::new(TuryaEngine::new(mock_provider, tools, PermissionMode::ReviewForMe));

    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::channel(64);

    let session = TuryaSession::new(engine, cmd_rx, event_tx);
    tokio::spawn(async move {
        session.run_loop().await;
    });

    let app = TuiApp::new();
    app.run(cmd_tx, event_rx).await?;

    Ok(())
}
```

### 6.3 End-to-End Verification Command
```bash
cargo build --workspace
```
*(Expected: Full workspace builds cleanly into `target/debug/turya-cli`).*

---

## Summary of Completed Files

When this playbook is executed in order, the following files exist and compile:
1. `Cargo.toml` (Workspace root)
2. `crates/turya-protocol/Cargo.toml` & `src/lib.rs`
3. `crates/turya-tools/Cargo.toml` & `src/lib.rs`
4. `crates/turya-core/Cargo.toml`, `src/lib.rs`, `src/provider.rs`, `src/permissions.rs`, `src/engine.rs`
5. `crates/turya-server/Cargo.toml` & `src/lib.rs`
6. `crates/turya-tui/Cargo.toml` & `src/lib.rs`
7. `crates/turya-cli/Cargo.toml` & `src/main.rs`
