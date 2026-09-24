use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

#[derive(Debug, Error)]
pub enum LspError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("timeout waiting for diagnostics")]
    Timeout,
    #[error("server not found: {0}")]
    NoServer(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone)]
pub struct LspDiagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
    pub severity: Severity,
}

impl LspDiagnostic {
    pub fn to_protocol(&self) -> turya_protocol::DiagnosticItem {
        turya_protocol::DiagnosticItem {
            file: self.file.clone(),
            line: self.line,
            message: self.message.clone(),
            severity: match self.severity {
                Severity::Error => "error".to_string(),
                Severity::Warning => "warning".to_string(),
                Severity::Info => "info".to_string(),
            },
        }
    }
}

/// Mid-turn diagnostic feedback loop (`.plans/plugin_and_subagent_architecture.md` §3).
///
/// Strategy: try a real language server over stdio JSON-RPC when one is
/// configured; otherwise fall back to a fast local syntax sanity check so the
/// agent loop never blocks on missing toolchains.
pub struct LspBridge {
    /// e.g. `rust-analyzer`, `typescript-language-server --stdio`, `pyright-langserver --stdio`
    pub server_cmd: Vec<String>,
    pub timeout: Duration,
}

impl LspBridge {
    pub fn new(server_cmd: Vec<String>) -> Self {
        Self {
            server_cmd,
            timeout: Duration::from_secs(8),
        }
    }

    pub fn rust_analyzer() -> Self {
        Self::new(vec!["rust-analyzer".to_string()])
    }

    /// Diagnose one file. Returns an empty vec (not an error) when no server
    /// is available so callers can proceed without false positives.
    pub async fn diagnose_file(&self, path: &Path) -> Result<Vec<LspDiagnostic>, LspError> {
        if self.server_cmd.is_empty() {
            return Ok(local_syntax_check(path));
        }
        match self.query_server(path).await {
            Ok(diags) => Ok(diags),
            Err(LspError::NoServer(_)) | Err(LspError::Timeout) => {
                Ok(local_syntax_check(path))
            }
            Err(e) => Err(e),
        }
    }

    /// Build the system-prompt feedback fed back to the model after an edit.
    pub fn format_feedback(path: &Path, diagnostics: &[LspDiagnostic]) -> Option<String> {
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        if errors.is_empty() {
            return None;
        }
        let mut out = format!(
            "Your last edit to {} caused {} compiler error(s). Fix them before finishing the turn:\n",
            path.display(),
            errors.len()
        );
        for e in errors.iter().take(10) {
            out.push_str(&format!("- line {}: {}\n", e.line, e.message));
        }
        Some(out)
    }

    async fn query_server(&self, path: &Path) -> Result<Vec<LspDiagnostic>, LspError> {
        let program = self.server_cmd.first().cloned().unwrap_or_default();
        if program.is_empty() {
            return Err(LspError::NoServer("<empty>".to_string()));
        }
        let mut child: Child = Command::new(&program)
            .args(&self.server_cmd[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::PermissionDenied
                {
                    // Unusable server binary -> caller falls back to local check.
                    LspError::NoServer(program.clone())
                } else {
                    LspError::Io(e)
                }
            })?;

        let result = tokio::time::timeout(
            self.timeout,
            run_lsp_session(&mut child, path, &self.server_cmd),
        )
        .await
        .map_err(|_| LspError::Timeout)?;

        let _ = child.kill().await;
        result
    }
}

/// Best-effort local check: missing file is an error; otherwise no verdict.
/// Deliberately conservative — never emit fake compiler errors.
fn local_syntax_check(path: &Path) -> Vec<LspDiagnostic> {
    if path.exists() {
        vec![]
    } else {
        vec![LspDiagnostic {
            file: path.to_path_buf(),
            line: 0,
            message: format!("file not found: {}", path.display()),
            severity: Severity::Error,
        }]
    }
}

fn encode_message(value: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_string(value).unwrap_or_default();
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
}

async fn read_message(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> Result<serde_json::Value, LspError> {
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(LspError::Protocol("eof from language server".to_string()));
        }
        if line.trim().is_empty() {
            break;
        }
        headers.push_str(&line);
    }
    let len: usize = headers
        .lines()
        .find_map(|l| {
            l.split_once(':').and_then(|(k, v)| {
                if k.trim().eq_ignore_ascii_case("Content-Length") {
                    v.trim().parse().ok()
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| LspError::Protocol("missing Content-Length".to_string()))?;
    let mut buf = vec![0u8; len];
    tokio::io::AsyncReadExt::read_exact(reader, &mut buf).await?;
    serde_json::from_slice(&buf).map_err(|e| LspError::Protocol(e.to_string()))
}

async fn run_lsp_session(
    child: &mut Child,
    path: &Path,
    _server_cmd: &[String],
) -> Result<Vec<LspDiagnostic>, LspError> {
    let stdin = child.stdin.take().ok_or_else(|| LspError::Protocol("no stdin".to_string()))?;
    let stdout = child.stdout.take().ok_or_else(|| LspError::Protocol("no stdout".to_string()))?;
    let mut writer = stdin;
    let mut reader = BufReader::new(stdout);

    let root = path.parent().unwrap_or(Path::new("."));
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": format!("file://{}", root.display()),
            "capabilities": { "textDocument": { "publishDiagnostics": {} } }
        }
    });
    writer.write_all(&encode_message(&init)).await?;
    // Drain initialize response (id 1); ignore errors — servers vary.
    let _ = read_message(&mut reader).await;
    let initialized =
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
    writer.write_all(&encode_message(&initialized)).await?;

    let content = std::fs::read_to_string(path).unwrap_or_default();
    let did_open = serde_json::json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": format!("file://{}", path.display()),
            "languageId": language_id(path),
            "version": 1,
            "text": content,
        }}
    });
    writer.write_all(&encode_message(&did_open)).await?;

    // Collect publishDiagnostics notifications for a short window.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut diags = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, read_message(&mut reader)).await {
            Ok(Ok(msg)) => {
                if msg.get("method").and_then(|m| m.as_str())
                    == Some("textDocument/publishDiagnostics")
                {
                    if let Some(items) =
                        msg.pointer("/params/diagnostics").and_then(|d| d.as_array())
                    {
                        for item in items {
                            let line = item
                                .pointer("/range/start/line")
                                .and_then(|l| l.as_u64())
                                .unwrap_or(0) as usize;
                            let message = item
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("diagnostic")
                                .to_string();
                            let sev_no = item.get("severity").and_then(|s| s.as_u64()).unwrap_or(1);
                            diags.push(LspDiagnostic {
                                file: path.to_path_buf(),
                                line: line + 1,
                                message,
                                severity: if sev_no == 1 {
                                    Severity::Error
                                } else if sev_no == 2 {
                                    Severity::Warning
                                } else {
                                    Severity::Info
                                },
                            });
                        }
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    Ok(diags)
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "ts" | "tsx" | "js" | "jsx" => "typescript",
        "py" => "python",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_server_falls_back_gracefully() {
        let bridge = LspBridge::new(vec!["turya-nonexistent-lsp-binary-xyz".to_string()]);
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let diags = bridge.diagnose_file(&manifest).await.unwrap();
        assert!(diags.is_empty());
    }

    #[tokio::test]
    async fn missing_file_reports_error() {
        let bridge = LspBridge::new(vec![]);
        let diags = bridge
            .diagnose_file(Path::new("/nonexistent/turya-missing-file.rs"))
            .await
            .unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn feedback_none_when_no_errors() {
        let diags = vec![LspDiagnostic {
            file: PathBuf::from("a.rs"),
            line: 3,
            message: "unused import".to_string(),
            severity: Severity::Warning,
        }];
        assert!(LspBridge::format_feedback(Path::new("a.rs"), &diags).is_none());
    }

    #[test]
    fn feedback_lists_errors() {
        let diags = vec![LspDiagnostic {
            file: PathBuf::from("db.rs"),
            line: 12,
            message: "unresolved import".to_string(),
            severity: Severity::Error,
        }];
        let fb = LspBridge::format_feedback(Path::new("db.rs"), &diags).unwrap();
        assert!(fb.contains("line 12"));
        assert!(fb.contains("unresolved import"));
    }

    #[test]
    fn protocol_conversion() {
        let d = LspDiagnostic {
            file: PathBuf::from("x.rs"),
            line: 1,
            message: "e".to_string(),
            severity: Severity::Error,
        };
        assert_eq!(d.to_protocol().severity, "error");
    }
}
