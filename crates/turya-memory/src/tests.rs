use super::*;
use turya_protocol::Transcript;

fn tmp_db(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("turya-mem-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("turya.db");
    let _ = std::fs::remove_file(&path);
    path
}

fn text_turn(id: &str, text: &str) -> Turn {
    turn_with_id(
        id,
        vec![Part::UserText {
            text: text.to_string(),
        }],
    )
}

#[test]
fn appends_turns_in_order_and_replays_them() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo", "first task").unwrap();
    store.append_turn("s1", &text_turn("t1", "hello")).unwrap();
    store.append_turn("s1", &text_turn("t2", "world")).unwrap();

    let turns = store.load_transcript("s1").unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].id.0, "t1");
    assert_eq!(turns[1].id.0, "t2");
    assert!(matches!(
        &turns[1].parts[0],
        Part::UserText { text } if text == "world"
    ));
}

#[test]
fn sequence_is_contiguous_and_never_skips() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo", "").unwrap();
    for i in 1..=5 {
        store
            .append_turn("s1", &text_turn(&format!("t{i}"), "x"))
            .unwrap();
        let meta = store.session_meta("s1").unwrap().unwrap();
        assert_eq!(meta.seq, i, "sequence must be dense");
    }
    let turns = store.load_transcript("s1").unwrap();
    assert_eq!(turns.len(), 5);
    assert_eq!(
        turns.iter().map(|t| t.id.0.clone()).collect::<Vec<_>>(),
        vec!["t1", "t2", "t3", "t4", "t5"]
    );
}

#[test]
fn interrupted_tool_call_is_closed_not_dropped() {
    // The provider rejects a replayed tool_use with no tool_result, so a
    // crashed turn must be closed rather than truncated.
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo", "").unwrap();
    store
        .append_turn(
            "s1",
            &turn_with_id(
                "t1",
                vec![
                    Part::UserText {
                        text: "run it".to_string(),
                    },
                    Part::ToolCall {
                        call_id: "c1".to_string(),
                        tool_name: "run_bash".to_string(),
                        arguments: serde_json::json!({}),
                        signature: Some("sig".to_string()),
                    },
                ],
            ),
        )
        .unwrap();

    let turns = store.load_transcript("s1").unwrap();
    assert_eq!(turns.len(), 1, "the turn survives the crash");
    let msgs = transcript_parts(turns[0].parts.clone());
    let results: Vec<_> = msgs
        .iter()
        .filter(|m| matches!(m, turya_protocol::MessagePart::ToolResult { .. }))
        .collect();
    assert_eq!(results.len(), 1, "the open call is closed");
    assert!(
        store.session_meta("s1").unwrap().unwrap().repaired,
        "the session is marked repaired"
    );
    // Repair is persisted: a second load is stable, not re-repaired.
    let again = store.load_transcript("s1").unwrap();
    assert_eq!(
        again[0].parts.len(),
        turns[0].parts.len(),
        "second load must be idempotent"
    );
}

#[test]
fn answered_tool_calls_are_left_alone() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo", "").unwrap();
    store
        .append_turn(
            "s1",
            &turn_with_id(
                "t1",
                vec![
                    Part::ToolCall {
                        call_id: "c1".to_string(),
                        tool_name: "run_bash".to_string(),
                        arguments: serde_json::json!({}),
                        signature: None,
                    },
                    Part::ToolResult {
                        call_id: "c1".to_string(),
                        output: "ok".to_string(),
                        truncated: false,
                    },
                ],
            ),
        )
        .unwrap();
    let turns = store.load_transcript("s1").unwrap();
    assert_eq!(turns[0].parts.len(), 2, "no synthetic result added");
    assert!(!store.session_meta("s1").unwrap().unwrap().repaired);
    assert_eq!(
        transcript_parts(turns[0].parts.clone())
            .iter()
            .filter(|m| matches!(m, turya_protocol::MessagePart::ToolResult { .. }))
            .count(),
        1
    );
}

#[test]
fn closed_interrupted_calls_is_pure() {
    let mut turn = turn_with_id(
        "t1",
        vec![
            Part::ToolCall {
                call_id: "a".to_string(),
                tool_name: "t".to_string(),
                arguments: serde_json::json!({}),
                signature: None,
            },
            Part::ToolCall {
                call_id: "b".to_string(),
                tool_name: "t".to_string(),
                arguments: serde_json::json!({}),
                signature: None,
            },
            Part::ToolResult {
                call_id: "a".to_string(),
                output: "done".to_string(),
                truncated: false,
            },
        ],
    );
    assert!(close_interrupted_calls(&mut turn));
    assert_eq!(turn.parts.len(), 4, "only the open call is closed");
    assert!(matches!(
        &turn.parts[3],
        Part::ToolResult { call_id, .. } if call_id == "b"
    ));
    assert!(
        !close_interrupted_calls(&mut turn),
        "second pass is a no-op"
    );
}

#[test]
fn lists_sessions_newest_first_and_scopes_by_cwd() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo-a", "a").unwrap();
    store.append_turn("s1", &text_turn("t1", "x")).unwrap();
    store.begin_session("s2", "/repo-b", "b").unwrap();
    store.append_turn("s2", &text_turn("t2", "y")).unwrap();

    let all = store.list_sessions(None, 10).unwrap();
    assert_eq!(all.len(), 2);
    let scoped = store.list_sessions(Some("/repo-a"), 10).unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].id, "s1");
    assert_eq!(scoped[0].title, "a");
    let limited = store.list_sessions(None, 1).unwrap();
    assert_eq!(limited.len(), 1);
}

#[test]
fn findses_stored_text_by_keyword() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo", "").unwrap();
    store
        .append_turn("s1", &text_turn("t1", "the migration lives in db.rs"))
        .unwrap();
    store
        .append_turn("s1", &text_turn("t2", "unrelated"))
        .unwrap();
    let hits = store.search("s1", "migration", 5).unwrap();
    assert_eq!(hits.len(), 1, "hits: {hits:?}");
    assert!(hits[0].contains("migration"));
    assert!(store.search("s1", "nothinghere", 5).unwrap().is_empty());
}

#[test]
fn exports_jsonl_with_a_header_and_one_line_per_part() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.begin_session("s1", "/repo", "export me").unwrap();
    store
        .append_turn(
            "s1",
            &turn_with_id(
                "t1",
                vec![
                    Part::UserText {
                        text: "hi".to_string(),
                    },
                    Part::Text {
                        text: "hello".to_string(),
                    },
                ],
            ),
        )
        .unwrap();
    let jsonl = store.export_jsonl("s1").unwrap();
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 4, "header + turn + 2 parts: {jsonl}");
    assert!(lines[0].contains("session_meta"));
    assert!(lines[1].contains("\"type\":\"turn\""));
    for line in &lines {
        serde_json::from_str::<serde_json::Value>(line).expect("every line is JSON");
    }
}

#[test]
fn foreign_format_is_moved_aside_not_migrated() {
    // Rule 5.3/5.4: a pre-A1 database is backed up and replaced, never
    // migrated, and the caller is told.
    let path = tmp_db("legacy");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 payload TEXT NOT NULL DEFAULT '{}'
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_events (session_id, kind, payload) VALUES ('s', 'x', '{}')",
            [],
        )
        .unwrap();
    }
    let (store, note) = MemoryStore::open(&path).unwrap();
    let note = note.expect("recovery must be reported");
    assert!(note.contains("backed up"), "{note}");
    assert!(path.exists(), "a fresh store exists at the same path");
    assert!(
        path.with_file_name("turya.db.pre-A1.bak").exists(),
        "the old file is preserved"
    );
    // The new store works, and the old rows are not silently carried over.
    store.begin_session("new", "/repo", "").unwrap();
    assert!(store.load_transcript("new").unwrap().is_empty());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn reopening_a_current_store_keeps_its_data() {
    let path = tmp_db("reopen");
    {
        let (store, note) = MemoryStore::open(&path).unwrap();
        assert!(note.is_none(), "no recovery on a clean open");
        store.begin_session("s1", "/repo", "keep me").unwrap();
        store
            .append_turn("s1", &text_turn("t1", "durable"))
            .unwrap();
    }
    let (store, note) = MemoryStore::open(&path).unwrap();
    assert!(note.is_none(), "current format must not be flagged");
    let turns = store.load_transcript("s1").unwrap();
    assert_eq!(turns.len(), 1);
    assert!(matches!(&turns[0].parts[0], Part::UserText { text } if text == "durable"));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn retains_learned_rules_across_reopen() {
    let path = tmp_db("rules");
    {
        let (store, _) = MemoryStore::open(&path).unwrap();
        store
            .save_rule("project", "prefer workspace-relative paths")
            .unwrap();
    }
    let (store, _) = MemoryStore::open(&path).unwrap();
    let hits = store
        .rules_for("use workspace relative paths always", 5)
        .unwrap();
    assert_eq!(hits.len(), 1, "rules survive: {hits:?}");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn old_audit_surface_still_records_and_replays() {
    let store = MemoryStore::open_in_memory().unwrap();
    store
        .record_event("s1", "note", &serde_json::json!({"k": 1}))
        .unwrap();
    let hist = store.session_history("s1", 10).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].0, "note");
    assert_eq!(hist[0].1["k"], 1);
}

#[test]
fn truncates_tool_output_on_a_char_boundary() {
    let out = MemoryStore::truncate_tool_output(&"a".repeat(50), 10);
    assert!(out.starts_with(&"a".repeat(10)));
    assert!(out.ends_with("[truncated]"));
    assert_eq!(MemoryStore::truncate_tool_output("short", 10), "short");
}

fn transcript_parts(parts: Vec<Part>) -> Vec<turya_protocol::MessagePart> {
    let mut t = Transcript::new("s");
    t.extend(parts);
    t.to_messages()
        .iter()
        .flat_map(|m| m.content.clone())
        .collect()
}

#[test]
fn exporting_an_unknown_session_is_an_error_not_an_empty_file() {
    let store = MemoryStore::open_in_memory().unwrap();
    match store.export_jsonl("nope") {
        Err(MemoryError::UnknownSession(id)) => assert_eq!(id, "nope"),
        other => panic!("expected UnknownSession, got {other:?}"),
    }
}
