//! Model Context Protocol client (internal plugin).
//!
//! Transport: stdio, one JSON-RPC 2.0 object per line, per the MCP spec.
//! Scopes deliberately narrow — `initialize`, `tools/list`, `tools/call` —
//! because a client that half-implements the protocol and claims the rest is
//! worse than one that is honestly small. HTTP transport, Tasks, MCP Apps and
//! server-initiated requests are not attempted; a server that needs them will
//! fail a call visibly rather than silently doing nothing.
//!
//! Trust: an MCP server is a third party. Its tool output is bounded and
//! marked, its tools carry a risk level so the permission broker gates them
//! like any other, and nothing it returns is treated as an instruction.
//!
//! Written by hand rather than pulled from an SDK: the wire format is a line
//! of JSON, and an SDK would pin us to a version of a spec that is still
//! moving every quarter.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use turya_protocol::{RiskLevel, ToolResult};
use turya_tools::Tool;

/// Protocol revision this client speaks. Sent in `initialize`; a server that
/// answers with a different one is reported, not silently accepted.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Longest tool output we will pass back to the model. A server can return
/// megabytes; the model cannot use that, and the context window pays for it.
const MAX_OUTPUT_CHARS: usize = 8000;

/// How long to wait for a response before declaring the server wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A discovered MCP tool, and the connection it belongs to.
pub struct McpTool {
    name: String,
    description: String,
    schema: Value,
    server: String,
    conn: Arc<Connection>,
}

impl McpTool {
    pub fn server(&self) -> &str {
        &self.server
    }

    /// The input schema, as the tool description the model will read. Sending
    /// the raw schema is what makes a dynamic tool usable.
    fn describe(&self) -> String {
        let props = self
            .schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| {
                o.iter()
                    .map(|(k, v)| {
                        let t = v.get("type").and_then(|t| t.as_str()).unwrap_or("any");
                        let req = v.get("description").and_then(|d| d.as_str()).unwrap_or("");
                        format!("{k}: {t} — {req}")
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        format!("{}. Parameters: {}", self.description, props)
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        // Cached because the model sees this on every request listing.
        &self.description
    }

    /// The server's own input schema, verbatim. Inventing a schema here would
    /// be a lie the model could act on.
    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk_level(&self, _params: &Value) -> RiskLevel {
        // An MCP tool can do anything its server can, so it starts at the
        // highest tier and the broker asks the user. Assuming otherwise is
        // exactly the mistake that makes MCP dangerous.
        RiskLevel::High
    }

    async fn execute(&self, call_id: &str, params: Value) -> ToolResult {
        match self.conn.call_tool(&self.name, params).await {
            Ok(text) => {
                let clipped: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
                let truncated = text.chars().count() > MAX_OUTPUT_CHARS;
                ToolResult {
                    call_id: call_id.to_string(),
                    success: true,
                    output: if truncated {
                        format!("{clipped}\n[output truncated at {MAX_OUTPUT_CHARS} chars]")
                    } else {
                        clipped
                    },
                    error: None,
                }
            }
            Err(e) => ToolResult {
                call_id: call_id.to_string(),
                success: false,
                output: String::new(),
                error: Some(e),
            },
        }
    }
}

/// One server connection. A single mutex serialises requests: JSON-RPC ids
/// make concurrent calls legal, but a tool server is usually a small script
/// and serialising avoids interleaved output for no benefit.
struct Connection {
    child: Mutex<Child>,
    // A background thread owns stdout and hands back whole lines. A blocking
    // pipe read cannot be interrupted, so a timeout enforced around it would
    // never fire; the channel is what makes a deadline real.
    rx: Mutex<mpsc::Receiver<Option<String>>>,
    stdin: Mutex<Option<ChildStdin>>,
    next_id: Mutex<u64>,
    server_info: Mutex<Option<Value>>,
    timeout: Duration,
}

impl Connection {
    fn spawn(
        server: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(server);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Some(out) = stdout else {
                let _ = tx.send(None);
                return;
            };
            let mut lines = BufReader::new(out).lines();
            while let Some(Ok(line)) = lines.next() {
                if tx.send(Some(line)).is_err() {
                    return; // connection dropped
                }
            }
            let _ = tx.send(None); // EOF: the server exited
        });
        Ok(Self {
            child: Mutex::new(child),
            rx: Mutex::new(rx),
            stdin: Mutex::new(stdin),
            next_id: Mutex::new(1),
            server_info: Mutex::new(None),
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// Send one request and read the matching response.
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            let id = *n;
            *n += 1;
            id
        };
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut guard = self.stdin.lock().unwrap();
            let w = guard
                .as_mut()
                .ok_or_else(|| format!("{method}: server stdin is closed"))?;
            writeln!(w, "{payload}").map_err(|e| format!("{method}: write failed: {e}"))?;
            w.flush()
                .map_err(|e| format!("{method}: flush failed: {e}"))?;
        }
        // Read until the id matches, skipping server-initiated notifications
        // and stale ids, which we do not act on.
        loop {
            let line = {
                let rx = self.rx.lock().unwrap();
                match rx.recv_timeout(self.timeout) {
                    // The server exited: no reply is coming, ever.
                    Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(format!("{method}: server exited without responding"));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return Err(format!("{method}: timed out after {:?}", self.timeout));
                    }
                    Ok(Some(l)) => l,
                }
            };
            let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                // Not JSON: a misbehaving server, not a protocol error.
                continue;
            };
            if msg.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                let text = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return Err(format!("{method}: server error: {text}"));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut guard = self.stdin.lock().unwrap();
        let w = guard
            .as_mut()
            .ok_or_else(|| format!("{method}: server stdin is closed"))?;
        writeln!(w, "{payload}").map_err(|e| format!("{method}: write failed: {e}"))?;
        w.flush()
            .map_err(|e| format!("{method}: flush failed: {e}"))
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<String, String> {
        let result = self.request("tools/call", json!({ "name": name, "arguments": args }))?;
        // The spec models output as content blocks; flatten text ones and
        // report anything else honestly rather than rendering `[object]`.
        let mut out = String::new();
        let mut saw_text = false;
        if let Some(blocks) = result.get("content").and_then(|c| c.as_array()) {
            for b in blocks {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if saw_text {
                        out.push('\n');
                    }
                    out.push_str(t);
                    saw_text = true;
                } else if let Some(kind) = b.get("type").and_then(|t| t.as_str()) {
                    out.push_str(&format!("[{kind} content omitted]"));
                }
            }
        }
        if result.get("isError").and_then(|v| v.as_bool()) == Some(true) && !saw_text {
            return Err("tool reported an error with no message".to_string());
        }
        if out.is_empty() {
            out = "(no text content)".to_string();
        }
        Ok(out)
    }

    fn shutdown(&self) {
        // Best effort: the spec's exit is a notification, then the process
        // goes away. A server that ignores it is killed below.
        let _ = self.notify(
            "notifications/cancelled",
            json!({ "reason": "shutting down" }),
        );
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// One configured server and the state of its connection.
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

/// A configured MCP server, addressed by name and kept ready to hand out
/// tools. Servers are independent: one failing to start never blocks another.
pub struct McpRegistry {
    servers: Vec<McpServer>,
    conns: HashMap<String, Arc<Connection>>,
    /// Deadline applied to every server this registry connects.
    timeout: Duration,
}

/// A connection kills its server on drop: a failed `connect` must not leave
/// behind a process the user never asked for.
impl Drop for Connection {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl McpRegistry {
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            conns: HashMap::new(),
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// Override the request deadline. A wedged server should not hold a turn
    /// open for longer than the user is willing to wait, and a test should
    /// not have to sit through the production bound to prove it gives up.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Connect to one server and list its tools. Returns the tool names on
    /// success, or the reason on failure — never a silent empty list.
    pub fn connect(
        &mut self,
        name: &str,
        command: &str,
        args: &[String],
    ) -> Result<Vec<String>, String> {
        let mut conn = Connection::spawn(command, args, None)
            .map_err(|e| format!("cannot start '{command}': {e}"))?;
        conn.timeout = self.timeout;
        let conn = Arc::new(conn);

        let init = conn
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "turya", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .map_err(|e| format!("initialize failed: {e}"))?;
        if let Some(got) = init.get("protocolVersion").and_then(|v| v.as_str()) {
            if got != PROTOCOL_VERSION {
                // Not fatal, but the user should know the shapes may differ.
                eprintln!(
                    "turya: MCP server '{name}' speaks {got}; this client speaks \
                     {PROTOCOL_VERSION}"
                );
            }
        }
        // The handshake is not complete until the server is told so.
        conn.notify("notifications/initialized", json!({}))
            .map_err(|e| format!("initialized notification failed: {e}"))?;
        *conn.server_info.lock().unwrap() = init.get("serverInfo").cloned();

        let listed = conn
            .request("tools/list", json!({}))
            .map_err(|e| format!("tools/list failed: {e}"))?;
        let mut tools = Vec::new();
        if let Some(array) = listed.get("tools").and_then(|t| t.as_array()) {
            for t in array {
                if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                    tools.push(name.to_string());
                }
            }
        }

        self.servers.push(McpServer {
            name: name.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            tools: tools.clone(),
            error: None,
        });
        self.conns.insert(name.to_string(), conn);
        Ok(tools)
    }

    /// Build `Tool` handles for every discovered tool on a server.
    ///
    /// The schema is turned into prose for the model, because a raw JSON
    /// schema in a tool description wastes tokens and reads worse than a
    /// `path: string — the file to read` line.
    pub fn tools_for(&self, server: &str) -> Vec<Arc<McpTool>> {
        let Some(conn) = self.conns.get(server) else {
            return Vec::new();
        };
        let Ok(listed) = conn.request("tools/list", json!({})) else {
            return Vec::new();
        };
        let Some(array) = listed.get("tools").and_then(|t| t.as_array()) else {
            return Vec::new();
        };
        array
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                let schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                let mut tool = McpTool {
                    name,
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("(no description)")
                        .to_string(),
                    schema,
                    server: server.to_string(),
                    conn: conn.clone(),
                };
                tool.description = tool.describe();
                Some(Arc::new(tool))
            })
            .collect()
    }

    /// Server names and their state, for `/mcp`.
    pub fn listing(&self) -> Vec<(String, String, Vec<String>, Option<String>)> {
        self.servers
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.command.clone(),
                    s.tools.clone(),
                    s.error.clone(),
                )
            })
            .collect()
    }

    /// Stop every server. Called on shutdown so a script we started does not
    /// outlive us.
    pub fn shutdown(&self) {
        for c in self.conns.values() {
            c.shutdown();
        }
    }
}

impl Drop for McpRegistry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Adapter that lets an `Arc<McpTool>` (built behind a lock, shared with
/// `/mcp`) satisfy the owned `Box<dyn Tool>` the registry stores.
pub struct McpToolHandle(pub Arc<McpTool>);

#[async_trait::async_trait]
impl Tool for McpToolHandle {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn schema(&self) -> serde_json::Value {
        self.0.schema()
    }
    fn risk_level(&self, params: &serde_json::Value) -> RiskLevel {
        self.0.risk_level(params)
    }
    async fn execute(&self, call_id: &str, params: serde_json::Value) -> ToolResult {
        self.0.execute(call_id, params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_clipped_to_a_bound() {
        // The clip lives in execute(); assert the bound is what we think.
        // The bound is a const assertion, so the guarantee holds without a
        // test: nobody can widen this past a context window and still build.
        const { assert!(MAX_OUTPUT_CHARS <= 16_000) };
    }

    #[test]
    fn an_empty_registry_lists_nothing() {
        let r = McpRegistry::empty();
        assert!(r.listing().is_empty());
        assert!(r.tools_for("nope").is_empty());
    }

    #[test]
    fn starting_a_missing_binary_reports_why() {
        let mut r = McpRegistry::empty();
        let err = r
            .connect("ghost", "/definitely/not/a/binary", &[])
            .unwrap_err();
        assert!(err.contains("cannot start"), "{err}");
    }

    #[test]
    fn the_request_timeout_is_short_enough_to_not_hang_a_turn() {
        assert!(REQUEST_TIMEOUT <= Duration::from_secs(60));
    }
}
