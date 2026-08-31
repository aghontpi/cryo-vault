use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

const TEST_TIMESTAMP: i64 = 1_700_000_000; // 2023-11-14

fn cryo_command(db_path: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cryo-vault"));
    cmd.env("CRYO_DB_PATH", db_path);
    cmd
}

#[test]
fn test_cli_capture_run_and_idempotency() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join(".cryo-vault/imports");
    std::fs::create_dir_all(&import_root).unwrap();
    std::fs::write(
        import_root.join("session.json"),
        r#"{"session_id":"capture-one","messages":[{"role":"user","content":"capture me"},{"role":"assistant","content":"done"}]}"#,
    )
    .unwrap();

    cryo_command(&db_path)
        .env("HOME", temp_dir.path())
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args(["capture", "run", "--platform", "generic"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unstable 1"));

    cryo_command(&db_path)
        .env("HOME", temp_dir.path())
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args(["capture", "run", "--platform", "generic"])
        .assert()
        .success()
        .stdout(predicate::str::contains("imported 1"));

    cryo_command(&db_path)
        .arg("search")
        .arg("capture me")
        .assert()
        .success()
        .stdout(predicate::str::contains("capture-generic-"));
}

#[test]
fn test_cli_capture_verbose_first_pass_guidance_is_safe() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join("imports");
    std::fs::create_dir_all(&import_root).unwrap();
    let transcript = import_root.join("pending.json");
    std::fs::write(
        &transcript,
        r#"{"session_id":"pending","messages":[{"role":"user","content":"private transcript body"}]}"#,
    )
    .unwrap();

    cryo_command(&db_path)
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args(["capture", "run", "--platform", "generic"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unstable 1"));

    let output = cryo_command(&db_path)
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args([
            "capture",
            "run",
            "--platform",
            "generic",
            "--settle",
            "0s",
            "-v",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Capture:"));
    assert!(!stdout.contains(transcript.to_str().unwrap()));
    assert!(!stdout.contains("— imported"));
    assert!(!stdout.contains("private transcript body"));
    assert_eq!(stderr.matches(transcript.to_str().unwrap()).count(), 1);
    assert!(stderr.contains("imported"));
}

#[test]
fn test_cli_capture_no_session_id_deduplicates_across_paths_and_state_reset() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join("imports");
    std::fs::create_dir_all(&import_root).unwrap();
    let transcript = r#"{"messages":[{"role":"user","content":"same visible transcript"},{"role":"assistant","content":"same answer"}]}"#;
    std::fs::write(import_root.join("one.json"), transcript).unwrap();
    std::fs::write(import_root.join("two.json"), transcript).unwrap();

    let first = cryo_command(&db_path)
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args([
            "capture",
            "run",
            "--platform",
            "generic",
            "--settle",
            "0s",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(first.status.success());
    let report: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["imported_sessions"], 1);

    std::fs::remove_file(db_path.join("capture-state.json")).unwrap();
    let second = cryo_command(&db_path)
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args([
            "capture",
            "run",
            "--platform",
            "generic",
            "--settle",
            "0s",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(second.status.success());
    let report: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["imported_sessions"], 0);

    cryo_command(&db_path)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicate::str::contains("Total Sessions:   1"));
}

#[test]
fn test_cli_capture_changed_no_session_id_keeps_one_latest_revision() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join("imports");
    std::fs::create_dir_all(&import_root).unwrap();
    let transcript = import_root.join("revisable.json");
    std::fs::write(
        &transcript,
        r#"{"messages":[{"role":"user","content":"old visible text"}]}"#,
    )
    .unwrap();

    for expected in [1, 1] {
        let output = cryo_command(&db_path)
            .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
            .args([
                "capture",
                "run",
                "--platform",
                "generic",
                "--settle",
                "0s",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["imported_sessions"], expected);
        if expected == 1 {
            std::fs::write(
                &transcript,
                r#"{"messages":[{"role":"user","content":"new visible text"}]}"#,
            )
            .unwrap();
        }
    }

    cryo_command(&db_path)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicate::str::contains("Total Sessions:   1"));
    cryo_command(&db_path)
        .args(["show", "--diagnostics"])
        .assert()
        .success()
        .stdout(predicate::str::contains("capture_revision: 2"))
        .stdout(predicate::str::contains("new visible text").not());
    cryo_command(&db_path)
        .args(["search", "new visible text"])
        .assert()
        .success()
        .stdout(predicate::str::contains("capture-generic-"));
    cryo_command(&db_path)
        .args(["search", "old visible text"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No matches found"));
}

#[test]
fn test_cli_capture_reimports_reverted_no_session_id_revision() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join("imports");
    std::fs::create_dir_all(&import_root).unwrap();
    let transcript = import_root.join("revertible.json");

    for content in ["revision A", "revision B", "revision A"] {
        std::fs::write(
            &transcript,
            format!(r#"{{"messages":[{{"role":"user","content":"{content}"}}]}}"#),
        )
        .unwrap();
        let output = cryo_command(&db_path)
            .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
            .args([
                "capture",
                "run",
                "--platform",
                "generic",
                "--settle",
                "0s",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["imported_sessions"], 1, "{content}");
    }

    cryo_command(&db_path)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicate::str::contains("Total Sessions:   1"));
}

#[test]
fn test_cli_capture_json_settle_imports_without_transcript_content() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join("imports");
    std::fs::create_dir_all(&import_root).unwrap();
    std::fs::write(
        import_root.join("settled.json"),
        r#"{"session_id":"settled","messages":[{"role":"user","content":"do not print this transcript"}]}"#,
    )
    .unwrap();

    let output = cryo_command(&db_path)
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args([
            "capture",
            "run",
            "--platform",
            "generic",
            "--settle",
            "0s",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["imported_sessions"], 1);
    assert_eq!(report["discovered_by_platform"]["generic"], 1);
    assert_eq!(report["candidates"][0]["outcome"], "imported");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("do not print this transcript"));
}

#[test]
fn test_cli_capture_malformed_file_is_named_on_stderr() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    let import_root = temp_dir.path().join("imports");
    std::fs::create_dir_all(&import_root).unwrap();
    let transcript = import_root.join("malformed.json");
    std::fs::write(&transcript, "not valid json").unwrap();

    let output = cryo_command(&db_path)
        .env("CRYO_CAPTURE_IMPORT_ROOTS", &import_root)
        .args([
            "capture",
            "run",
            "--platform",
            "generic",
            "--settle",
            "0s",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed_files"], 1);
    assert_eq!(report["candidates"][0]["outcome"], "failed");
    assert!(String::from_utf8_lossy(&output.stderr).contains(transcript.to_str().unwrap()));
    assert!(String::from_utf8_lossy(&output.stdout).contains(transcript.to_str().unwrap()));
}

#[test]
fn test_cli_capture_scheduler_dry_run_and_time_validation() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .args(["capture", "install", "--time", "01:15", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("01:15"));

    cryo_command(&db_path)
        .args(["capture", "install", "--time", "25:00", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("capture time must be HH:MM"));
}

#[test]
fn test_cli_capture_hint_queues_only_json_result() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .args(["capture", "hint", "--platform", "antigravity", "--stdin"])
        .write_stdin(
            r#"{"conversationId":"conversation-1","transcriptPath":"/tmp/transcript.jsonl"}"#,
        )
        .assert()
        .success()
        .stdout("{\"queued\":true}\n")
        .stderr(predicate::str::is_empty());

    assert!(!db_path.join("capture-state.json").exists());
    assert_eq!(
        std::fs::read_dir(db_path.join("capture-hints"))
            .unwrap()
            .count(),
        1
    );
}

/// Tests that the `stats` command works correctly on an empty database.
/// Verifies that the output contains "Database Statistics" and "Total Sessions: 0".
#[test]
fn test_cli_stats_empty() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicate::str::contains("Database Statistics"))
        .stdout(predicate::str::contains("Total Sessions:   0"));
}

/// Tests the basic workflow of adding a session via stdin and searching for it.
/// Validates:
/// - Adding a session with `add` command via JSON input
/// - Stats command shows correct session count
/// - Search command finds the session by content
#[test]
fn test_cli_add_and_search() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    // Add session via stdin
    let session_json = format!(
        r#"{{
        "id": "s1",
        "title": "CLI Test",
        "source": "test",
        "model": "gpt-4",
        "created_at": {},
        "metadata": {{}},
        "messages": [
            {{
                "role": "user",
                "content": "Hello CLI"
            }}
        ]
    }}"#,
        TEST_TIMESTAMP
    );

    cryo_command(&db_path)
        .arg("add")
        .write_stdin(session_json)
        .assert()
        .success();

    // Verify stats
    cryo_command(&db_path)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicate::str::contains("Total Sessions:   1"));

    // Search
    cryo_command(&db_path)
        .arg("search")
        .arg("Hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("[s1] CLI Test"));

    cryo_command(&db_path)
        .args(["show", "s1", "--diagnostics"])
        .assert()
        .success()
        .stdout(predicate::str::contains("importer: \"add\""))
        .stdout(predicate::str::contains("source_path: \"unknown\""))
        .stdout(predicate::str::contains("source_session_id: \"s1\""))
        .stdout(predicate::str::contains("records_read:"))
        .stdout(predicate::str::contains("visible_messages_extracted:"))
        .stdout(predicate::str::contains("records_skipped_by_reason:"))
        .stdout(predicate::str::contains("malformed_records:"));
}

/// Tests adding sessions via the streaming interface.
/// Validates:
/// - Processing streaming events (session_start, message, finalize)
/// - Correct session archival count
/// - Search can find the streamed session
#[test]
fn test_cli_add_stream() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    let event1 = r#"{"type":"session_start", "session_id":"ws1", "metadata":{}}"#;
    let event2 = r#"{"type":"message", "session_id":"ws1", "message":{"id":"m1","role":"user","content":"Streamed Msg"}}"#;
    let event3 = r#"{"type":"finalize", "session_id":"ws1"}"#;
    let input = format!("{}\n{}\n{}", event1, event2, event3);

    cryo_command(&db_path)
        .arg("add")
        .arg("--stream")
        .write_stdin(input)
        .assert()
        .success()
        .stdout(predicate::str::contains("Archived 1 sessions"));

    // Verify search
    cryo_command(&db_path)
        .arg("search")
        .arg("Streamed")
        .assert()
        .success()
        .stdout(predicate::str::contains("[ws1] Untitled")); // No title in stream

    cryo_command(&db_path)
        .args(["show", "ws1", "--diagnostics"])
        .assert()
        .success()
        .stdout(predicate::str::contains("importer: \"stream\""))
        .stdout(predicate::str::contains("parser_version:"))
        .stdout(predicate::str::contains("source_platform: \"unknown\""))
        .stdout(predicate::str::contains("source_path: \"unknown\""))
        .stdout(predicate::str::contains("source_session_id: \"ws1\""));
}

/// Tests the `show` command to display a specific session by ID.
/// Validates:
/// - Adding a session
/// - Retrieving and displaying the session with correct title
#[test]
fn test_cli_show() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    // Add session
    let session_json = r#"{
        "id": "show_me",
        "title": "Show Title",
        "metadata": {},
        "messages": []
    }"#;

    cryo_command(&db_path)
        .arg("add")
        .write_stdin(session_json)
        .assert()
        .success();

    // Show
    cryo_command(&db_path)
        .arg("show")
        .arg("show_me")
        .assert()
        .success()
        .stdout(predicate::str::contains("Title: Show Title"));
}

/// Tests the `first` and `last` commands for retrieving sessions by creation order.
/// Validates:
/// - Adding multiple sessions
/// - `first N` command retrieves the first N sessions
/// - `last N` command retrieves the last N sessions
#[test]
fn test_cli_first_last() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    // Add 3 sessions
    for i in 1..=3 {
        let session = format!(r#"{{ "id": "s{}", "messages": [] }}"#, i);
        cryo_command(&db_path)
            .arg("add")
            .write_stdin(session)
            .assert()
            .success();
    }

    // Test First 2
    cryo_command(&db_path)
        .arg("first")
        .arg("2")
        .assert()
        .success()
        .stdout(predicate::str::contains("first 2"));

    // Test Last 2
    cryo_command(&db_path)
        .arg("last")
        .arg("2")
        .assert()
        .success()
        .stdout(predicate::str::contains("last 2"));
}

#[test]
fn test_cli_first_last_source_filter_is_chronological() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    for (id, source, created_at) in [
        ("old-codex", "codex", 1_700_000_001),
        ("middle-claude", "claude-code", 1_700_000_002),
        ("new-codex", "codex", 1_700_000_003),
    ] {
        cryo_command(&db_path)
            .arg("add")
            .write_stdin(format!(
                r#"{{"id":"{id}","source":"{source}","created_at":{created_at},"messages":[]}}"#
            ))
            .assert()
            .success();
    }

    cryo_command(&db_path)
        .args(["first", "1", "--source", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("old-codex"))
        .stdout(predicate::str::contains("new-codex").not());
    cryo_command(&db_path)
        .args(["last", "1", "--source", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("new-codex"))
        .stdout(predicate::str::contains("old-codex").not());
}

#[test]
fn test_cli_audit_provenance_reports_source_less_legacy_session() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");
    cryo_command(&db_path)
        .arg("add")
        .write_stdin(r#"{"id":"legacy-session","messages":[]}"#)
        .assert()
        .success();

    cryo_command(&db_path)
        .args(["audit", "provenance"])
        .assert()
        .success()
        .stdout(predicate::str::contains("legacy-session"))
        .stdout(predicate::str::contains(
            "original source cannot be reconstructed",
        ))
        .stdout(predicate::str::contains("Untraceable sessions: 1"));
}

/// Tests the `reindex` command to rebuild the search index.
/// Validates:
/// - Adding a session
/// - Reindexing with --yes flag
/// - Correct count of reindexed sessions
#[test]
fn test_cli_reindex() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    // Add data
    let session = r#"{ "id": "s1", "messages": [] }"#;
    cryo_command(&db_path)
        .arg("add")
        .write_stdin(session)
        .assert()
        .success();

    cryo_command(&db_path)
        .arg("reindex")
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicate::str::contains("Reindexed 1 sessions"));
}

/// Tests error handling when adding invalid JSON input.
/// Validates that the command fails with appropriate error message.
#[test]
fn test_cli_add_invalid_json() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .arg("add")
        .write_stdin("not valid json")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to parse input"));
}

/// Tests error handling for invalid date format in search command.
/// Validates that --after flag with invalid date produces appropriate error.
#[test]
fn test_cli_search_invalid_date() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .arg("search")
        .arg("query")
        .arg("--after")
        .arg("invalid-date")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid date format"));
}

/// Tests the date range filtering functionality in search command.
/// Validates:
/// - Adding a session with specific timestamp (1700000000 = 2023-11-14)
/// - --after flag correctly includes sessions after the specified date
/// - --before flag correctly excludes sessions after the specified date
#[test]
fn test_cli_search_date_range() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    // Add session with specific date (1700000000 = 2023-11-14)
    let session = format!(
        r#"{{ "id": "s1", "created_at": {}, "messages": [{{"role":"user","content":"test"}}] }}"#,
        TEST_TIMESTAMP
    );
    cryo_command(&db_path)
        .arg("add")
        .write_stdin(session)
        .assert()
        .success();

    // Search After (Match)
    cryo_command(&db_path)
        .arg("search")
        .arg("test")
        .arg("--after")
        .arg("2023-11-01")
        .assert()
        .success()
        .stdout(predicate::str::contains("[s1]"));

    // Search Before (No Match)
    cryo_command(&db_path)
        .arg("search")
        .arg("test")
        .arg("--before")
        .arg("2023-01-01")
        .assert()
        .success()
        .stdout(predicate::str::contains("No matches found"));
}

/// Tests the `show` command behavior when session ID doesn't exist.
/// Validates that the command fails (or succeeds with message) as expected.
#[test]
fn test_cli_show_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .arg("show")
        .arg("unknown_id")
        .assert()
        .success() // Should succeed but print "not found"
        .stdout(predicate::str::contains("Session not found"));
}

/// Tests importing ChatGPT export format (array of conversations).
/// Validates:
/// - Parsing ChatGPT export JSON structure with conversation mapping
/// - Successful import with appropriate success message
/// - Imported session is retrievable with correct title
#[test]
fn test_cli_add_chatgpt_export() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    // Minimal ChatGPT export format (Array of conversations)
    let export = r#"[
        {
            "id": "conv1",
            "title": "GPT Chat",
            "create_time": 1600000000,
            "mapping": {},
            "current_node": null
        }
    ]"#;

    cryo_command(&db_path)
        .arg("add")
        .write_stdin(export)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Imported 1 ChatGPT conversations to",
        ));

    // Verify it exists
    cryo_command(&db_path)
        .arg("show")
        .arg("conv1")
        .assert()
        .success()
        .stdout(predicate::str::contains("GPT Chat"));
}

/// Tests importing multiple sessions via JSON array input.
/// Validates:
/// - Parsing array of session objects
/// - Correct count of imported sessions in output
#[test]
fn test_cli_add_array() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    let array = r#"[
        { "id": "a1", "messages": [] },
        { "id": "a2", "messages": [] }
    ]"#;

    cryo_command(&db_path)
        .arg("add")
        .write_stdin(array)
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported 2 sessions"));
}

/// Tests the cancellation flow for the reindex command.
/// Validates that reindex without --yes flag can be cancelled via stdin input.
#[test]
fn test_cli_reindex_cancel() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join(".cryo");

    cryo_command(&db_path)
        .arg("reindex") // No --yes
        .write_stdin("n\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cancelled"));
}
