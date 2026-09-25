use async_trait::async_trait;
use std::path::Path;
use tokio::fs;
use tokio::process::Command;
use turya_protocol::{RiskLevel, ToolResult};

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
    fn name(&self) -> &'static str {
        "view_file"
    }
    fn description(&self) -> &'static str {
        "Read file content from the filesystem"
    }
    fn risk_level(&self, _params: &serde_json::Value) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        let path_str = match params.get("path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    call_id: call_id.to_string(),
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'path' parameter".to_string()),
                }
            }
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
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "Write or overwrite file content"
    }
    fn risk_level(&self, _params: &serde_json::Value) -> RiskLevel {
        RiskLevel::High
    }

    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        let path_str = match params.get("path").and_then(|p| p.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    call_id: call_id.to_string(),
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'path' parameter".to_string()),
                }
            }
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
    fn name(&self) -> &'static str {
        "run_bash"
    }
    fn description(&self) -> &'static str {
        "Execute a bash shell command"
    }
    fn risk_level(&self, _params: &serde_json::Value) -> RiskLevel {
        RiskLevel::High
    }

    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        let command = match params.get("command").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    call_id: call_id.to_string(),
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'command' parameter".to_string()),
                }
            }
        };

        match Command::new("bash").arg("-c").arg(command).output().await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{}\nSTDERR:\n{}", stdout, stderr)
                };
                ToolResult {
                    call_id: call_id.to_string(),
                    success: output.status.success(),
                    output: combined,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!("Exited with code: {:?}", output.status.code()))
                    },
                }
            }
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
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }
}
