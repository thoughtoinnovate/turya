use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use tokio::fs;
use tokio::process::Command;
use turya_protocol::{RiskLevel, ToolResult, ToolSpec};

#[async_trait]
pub trait Tool: Send + Sync {
    /// Owned strings, not `&'static str`: a tool discovered at runtime (an
    /// MCP server's tool) has no static name to return.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for this tool's arguments, sent to the model as a real
    /// function declaration. Prose in the transcript does not compete with a
    /// declared function, so anything the model is meant to call needs one.
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
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

/// How long a single `run_bash` may run before it is killed.
///
/// A shell command has no natural upper bound and the model writes the
/// script: an accidental `while true`, a `tail -f`, a server that never
/// exits. Without a deadline the call never returns and the turn is stuck
/// with no error and no way out. Override with `TURYA_BASH_TIMEOUT_SECS`.
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;

/// Longest output handed back to the model. A command that prints a million
/// lines would otherwise spend the whole context window on one result.
pub const MAX_BASH_OUTPUT_CHARS: usize = 32_000;

fn bash_timeout() -> std::time::Duration {
    let secs = std::env::var("TURYA_BASH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_BASH_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
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

        let limit = bash_timeout();
        let mut child = match Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    call_id: call_id.to_string(),
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to start bash: {e}")),
                }
            }
        };

        // Both pipes are drained on their own tasks. Waiting for a child to
        // exit while its output sits unread in a full pipe buffer is a
        // deadlock of its own: the child blocks on write, we block on wait.
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut p) = out_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut p, &mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut p) = err_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut p, &mut buf).await;
            }
            buf
        });

        let status = match tokio::time::timeout(limit, child.wait()).await {
            Ok(Ok(st)) => st,
            Ok(Err(e)) => {
                let _ = child.start_kill();
                return ToolResult {
                    call_id: call_id.to_string(),
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to wait for bash: {e}")),
                };
            }
            Err(_) => {
                // Killed, then reaped, so no orphan is left holding the pipe.
                let _ = child.start_kill();
                let _ = child.wait().await;
                // Whatever it printed before the deadline is usually the
                // interesting part of a runaway command, so it is kept.
                let partial = clip_output(collect_pipes(out_task, err_task).await);
                return ToolResult {
                    call_id: call_id.to_string(),
                    success: false,
                    output: partial,
                    error: Some(format!(
                        "command exceeded the {}-second limit and was killed; bound it \
                         yourself or run it in the background",
                        limit.as_secs()
                    )),
                };
            }
        };

        let combined = clip_output(collect_pipes(out_task, err_task).await);
        ToolResult {
            call_id: call_id.to_string(),
            success: status.success(),
            output: combined,
            error: if status.success() {
                None
            } else {
                Some(format!("Exited with code: {:?}", status.code()))
            },
        }
    }
}

/// Join the two pipe readers into the shape this tool has always returned:
/// stdout, with stderr appended when there was any.
async fn collect_pipes(
    out_task: tokio::task::JoinHandle<Vec<u8>>,
    err_task: tokio::task::JoinHandle<Vec<u8>>,
) -> String {
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    let out = String::from_utf8_lossy(&stdout).to_string();
    let err = String::from_utf8_lossy(&stderr).to_string();
    if err.is_empty() {
        out
    } else {
        format!("{out}\nSTDERR:\n{err}")
    }
}

fn clip_output(text: String) -> String {
    if text.chars().count() <= MAX_BASH_OUTPUT_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX_BASH_OUTPUT_CHARS).collect();
    clipped.push_str("\n[output truncated at 32000 chars]");
    clipped
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

    /// Add a tool discovered at runtime. First registration wins, so a
    /// builtin is never shadowed by a server offering the same name.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> bool {
        if self.get(tool.name()).is_some() {
            return false;
        }
        self.tools.push(tool);
        true
    }

    /// Tool names, in registration order (drives the MCP listing).
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Every tool, as declarations for the model to call.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec::new(t.name(), t.description(), t.schema()))
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }
}

#[cfg(test)]
mod bash_tests {
    use super::*;

    /// The process environment is global, so every test in here takes this
    /// lock. Without it two tests setting and clearing
    /// `TURYA_BASH_TIMEOUT_SECS` interleave and the suite is flaky for a
    /// reason that has nothing to do with the code.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn run(command: &str) -> ToolResult {
        RunBashTool
            .execute("c1", serde_json::json!({ "command": command }))
            .await
    }

    #[tokio::test]
    async fn ordinary_output_still_comes_back() {
        let _guard = ENV_LOCK.lock().await;
        let r = run("echo hello").await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(r.output.trim(), "hello");
    }

    #[tokio::test]
    async fn stderr_is_still_appended() {
        let _guard = ENV_LOCK.lock().await;
        let r = run("echo out; echo err 1>&2").await;
        assert!(r.output.contains("out"), "{}", r.output);
        assert!(r.output.contains("STDERR:"), "{}", r.output);
        assert!(r.output.contains("err"), "{}", r.output);
    }

    #[tokio::test]
    async fn a_command_that_never_exits_is_killed_instead_of_hanging() {
        // The bug this exists for: `while true` used to block the tool call
        // forever, so the turn never finished and the user had no error and
        // no way out.
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("TURYA_BASH_TIMEOUT_SECS", "1");
        let r = run("while true; do sleep 0.1; done").await;
        std::env::remove_var("TURYA_BASH_TIMEOUT_SECS");
        assert!(!r.success);
        let err = r.error.unwrap();
        assert!(err.contains("exceeded the 1-second limit"), "{err}");
        assert!(err.contains("killed"), "{err}");
    }

    #[tokio::test]
    async fn a_timed_out_command_still_returns_whatever_it_printed() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("TURYA_BASH_TIMEOUT_SECS", "1");
        let r = run("echo partial-progress; while true; do sleep 0.1; done").await;
        std::env::remove_var("TURYA_BASH_TIMEOUT_SECS");
        assert!(r.output.contains("partial-progress"), "{}", r.output);
    }

    #[tokio::test]
    async fn runaway_output_is_clipped_rather_than_filling_the_context() {
        let _guard = ENV_LOCK.lock().await;
        let r =
            run("for i in $(seq 1 200000); do echo 'a very repetitive line of output'; done").await;
        assert!(r.success);
        assert!(
            r.output.chars().count() <= MAX_BASH_OUTPUT_CHARS + 40,
            "{} chars",
            r.output.chars().count()
        );
        assert!(
            r.output.contains("truncated"),
            "{}",
            &r.output[..80.min(r.output.len())]
        );
    }

    #[tokio::test]
    async fn a_failing_command_still_reports_its_exit_code() {
        let _guard = ENV_LOCK.lock().await;
        let r = run("exit 3").await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("3"));
    }

    #[tokio::test]
    async fn the_timeout_default_is_finite() {
        // A compile-time guarantee rather than a test: nobody can make this
        // unbounded and still build.
        const {
            assert!(DEFAULT_BASH_TIMEOUT_SECS > 0 && DEFAULT_BASH_TIMEOUT_SECS <= 600);
            assert!(MAX_BASH_OUTPUT_CHARS > 0);
        };
    }
}
