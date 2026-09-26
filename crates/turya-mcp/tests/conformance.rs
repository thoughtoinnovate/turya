//! Conformance tests against a real child process speaking the protocol.
//!
//! A mock server, not a mocked transport: the interesting failures in a stdio
//! JSON-RPC client are framing bugs (two frames in one write, a notification
//! arriving mid-request, a server that dies), and none of those exist if you
//! stub out the pipe.

use serde_json::json;
use turya_tools::Tool;

const MOCK: &str = r#"#!/bin/sh
# Speaks just enough MCP: initialize, tools/list, tools/call.
# Echoes the request id (a client that matches ids is doing it right),
# emits a notification before each response, and writes two frames in a
# single write, to catch buffering bugs.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue          # notification: nothing to answer
  case "$line" in
    *'"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"debug"}}'
      printf '%s\n%s\n' \
        "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"serverInfo\":{\"name\":\"mock\",\"version\":\"1\"},\"capabilities\":{\"tools\":{}}}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":{\"stale\":true}}"
      ;;
    *'"tools/list"'*)
      printf '%s\n%s\n' \
        "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echo a string back\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\",\"description\":\"what to echo\"}},\"required\":[\"text\"]}}]}}" \
        "{\"jsonrpc\":\"2.0\",\"id\":1000,\"result\":{\"decoy\":true}}"
      ;;
    *'"tools/call"'*)
      case "$line" in
        *'"boom"'*) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32602,\"message\":\"tool exploded\"}}" ;;
        *) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"ECHO: hello\"}]}}" ;;
      esac
      ;;
    *) printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"no such method\"}}" ;;
  esac
done
"#;

fn write_mock() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("turya-mcp-mock");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mock-server.sh");
    std::fs::write(&path, MOCK).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn registry() -> turya_mcp::McpRegistry {
    let mut r = turya_mcp::McpRegistry::empty();
    r.connect(
        "mock",
        write_mock().to_str().unwrap(),
        &["--stdio".to_string()],
    )
    .expect("mock server must connect");
    r
}

#[test]
fn connects_and_lists_the_servers_tools() {
    let r = registry();
    let listing = r.listing();
    assert_eq!(listing.len(), 1);
    let (name, command, tools, error) = &listing[0];
    assert_eq!(name, "mock");
    assert!(command.contains("mock-server.sh"));
    assert_eq!(tools, &vec!["echo".to_string()]);
    assert!(error.is_none());
}

#[test]
fn builds_callable_tools_with_the_schema_in_the_description() {
    let r = registry();
    let tools = r.tools_for("mock");
    assert_eq!(tools.len(), 1);
    let t = &tools[0];
    assert_eq!(t.name(), "echo");
    assert_eq!(t.server(), "mock");
    // The model reads only the description, so the parameter must be in it.
    assert!(
        t.description().contains("text: string"),
        "{}",
        t.description()
    );
    assert!(
        t.description().contains("what to echo"),
        "{}",
        t.description()
    );
}

#[tokio::test]
async fn calls_a_tool_and_reads_the_text_block() {
    let r = registry();
    let tools = r.tools_for("mock");
    let out = tools[0]
        .execute(
            "call-1",
            json!({"name": "echo", "arguments": {"text": "hello"}}),
        )
        .await;
    assert!(out.success, "{:?}", out.error);
    assert_eq!(out.output, "ECHO: hello");
}

#[tokio::test]
async fn a_server_side_error_becomes_a_failed_tool_result_not_a_panic() {
    let r = registry();
    let tools = r.tools_for("mock");
    let out = tools[0]
        .execute("call-2", json!({"name": "boom", "arguments": {}}))
        .await;
    assert!(!out.success);
    let err = out.error.unwrap();
    assert!(err.contains("tool exploded"), "{err}");
}

#[tokio::test]
async fn mcp_tools_are_high_risk_so_the_broker_asks() {
    use turya_tools::Tool;
    let r = registry();
    let tools = r.tools_for("mock");
    // A server can do anything; assuming otherwise is how MCP hurts people.
    assert_eq!(
        tools[0].risk_level(&json!({})),
        turya_protocol::RiskLevel::High
    );
}

#[test]
fn a_server_that_says_nothing_times_out_instead_of_hanging_forever() {
    // A real regression guard for the no-timeout bug this client was written
    // to avoid: `BufRead::read_line` would block here indefinitely.
    let dir = std::env::temp_dir().join("turya-mcp-mock");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("silent-server.sh");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 3600\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut r = turya_mcp::McpRegistry::empty();
    let started = std::time::Instant::now();
    let res = r.connect("silent", path.to_str().unwrap(), &[]);
    let elapsed = started.elapsed();
    assert!(res.is_err(), "a silent server must not look like success");
    // The bound is 30s per request; allow slack for a loaded CI box.
    assert!(elapsed < std::time::Duration::from_secs(60), "{elapsed:?}");
}

#[test]
fn one_broken_server_does_not_stop_the_next_one() {
    let mut r = turya_mcp::McpRegistry::empty();
    assert!(r.connect("bad", "/definitely/not/a/binary", &[]).is_err());
    r.connect("good", write_mock().to_str().unwrap(), &[])
        .expect("the healthy server still connects after a failure");
    assert_eq!(r.listing().len(), 1);
    assert_eq!(r.listing()[0].0, "good");
}
