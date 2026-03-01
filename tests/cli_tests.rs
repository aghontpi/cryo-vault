use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

const TEST_TIMESTAMP: i64 = 1_700_000_000; // 2023-11-14

fn cryo_command(db_path: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cryo-vault"));
    cmd.env("CRYO_DB_PATH", db_path);
    cmd
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
