use anyhow::Result;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

/// Helper struct to interact with the running MCP server process.
///
/// This struct manages the lifecycle of an MCP server process for testing purposes.
/// It spawns the server in a temporary directory, manages stdin/stdout communication,
/// and ensures proper cleanup on drop to allow LLVM coverage profiles to be written.
struct McpServer {
    process: Child,
    reader: BufReader<std::process::ChildStdout>,
    _temp_dir: TempDir, // Keep alive to prevent cleanup
}

impl McpServer {
    /// Creates and starts a new MCP server process for testing.
    ///
    /// The server is configured with:
    /// - A temporary database directory that is cleaned up on drop
    /// - Piped stdin/stdout for JSON-RPC communication
    /// - Inherited stderr for debugging test failures
    ///
    /// # Errors
    /// Returns an error if the server process fails to spawn or stdout cannot be captured.
    fn new() -> Result<Self> {
        let temp_dir = TempDir::new()?;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cryo-vault-mcp"));

        cmd.env("CRYO_DB_PATH", temp_dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()); // See stderr in test output if needed

        let mut process = cmd.spawn()?;
        let stdout = process.stdout.take().expect("Failed to open stdout");
        let reader = BufReader::new(stdout);

        Ok(Self {
            process,
            reader,
            _temp_dir: temp_dir,
        })
    }

    /// Sends a JSON-RPC request to the server and reads the response.
    ///
    /// This method serializes the request to JSON, writes it to the server's stdin,
    /// and reads one line from stdout as the response.
    ///
    /// # Arguments
    /// * `request` - A JSON-RPC request object (must include "jsonrpc", "method", and optionally "id" and "params")
    ///
    /// # Errors
    /// Returns an error if:
    /// - Serialization fails
    /// - Writing to stdin fails
    /// - The server closes the connection unexpectedly
    /// - Response parsing fails
    fn send_request(&mut self, request: Value) -> Result<Value> {
        let stdin = self.process.stdin.as_mut().expect("Failed to open stdin");
        let req_str = serde_json::to_string(&request)?;
        writeln!(stdin, "{}", req_str)?;

        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(anyhow::anyhow!("Server closed connection"));
        }

        let response: Value = serde_json::from_str(&line)?;
        Ok(response)
    }

    // Explicit close not needed if Drop is implemented, but good for error checking
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Close stdin to signal the server to stop (EOF)
        // This is required for the process to exit gracefully and write LLVM coverage profiles
        let _ = self.process.stdin.take();
        let _ = self.process.wait();
    }
}

/// Tests the MCP protocol initialize handshake.
/// Verifies that the server responds with proper server info and capabilities.
#[test]
fn test_initialize() -> Result<()> {
    let mut server = McpServer::new()?;

    let req = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
             "protocolVersion": "2024-11-05",
             "capabilities": {},
             "clientInfo": {
                 "name": "test-client",
                 "version": "1.0.0"
             }
        },
        "id": 1
    });

    let resp = server.send_request(req)?;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);

    let result = &resp["result"];
    assert_eq!(result["serverInfo"]["name"], "cryo-vault-mcp");
    assert!(result["capabilities"].is_object());

    Ok(())
}

/// Tests listing available MCP tools.
/// Verifies that all expected tools (add_log, search, read_session, get_recent_sessions, stats) are present.
#[test]
fn test_list_tools() -> Result<()> {
    let mut server = McpServer::new()?;

    // Initialize first (though not strictly enforced by current server impl, it's good practice)
    server.send_request(json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": 1
    }))?;

    let req = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 2
    });

    let resp = server.send_request(req)?;
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools should be an array");

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(tool_names.contains(&"add_log"));
    assert!(tool_names.contains(&"search"));
    assert!(tool_names.contains(&"read_session"));
    assert!(tool_names.contains(&"get_recent_sessions"));
    assert!(tool_names.contains(&"stats"));

    Ok(())
}

/// Tests the complete workflow of adding a log, searching for it, and reading the full session.
/// This integration test verifies that data flows correctly through add_log → search → read_session.
#[test]
fn test_add_log_search_and_read() -> Result<()> {
    let mut server = McpServer::new()?;

    // 1. Add Log
    let log_content = json!({
        "title": "Test Session",
        "messages": [
            { "role": "user", "content": "Hello" },
            { "role": "model", "content": "World" }
        ]
    });

    let add_req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": {
                "data": log_content
            }
        },
        "id": 1
    });

    let add_resp = server.send_request(add_req)?;
    assert!(add_resp["error"].is_null());

    // 2. Search for it
    let search_req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": {
                "query": "Hello"
            }
        },
        "id": 2
    });

    let search_resp = server.send_request(search_req)?;
    let content_str = search_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let search_results: Vec<Value> = serde_json::from_str(content_str)?;

    assert_eq!(search_results.len(), 1);
    let session_id = search_results[0]["id"].as_str().unwrap();
    assert_eq!(search_results[0]["title"], "Test Session");

    // 3. Read full session
    let read_req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "read_session",
            "arguments": {
                "id": session_id
            }
        },
        "id": 3
    });

    let read_resp = server.send_request(read_req)?;
    let read_content_str = read_resp["result"]["content"][0]["text"].as_str().unwrap();
    let session: Value = serde_json::from_str(read_content_str)?;

    assert_eq!(session["id"], session_id);
    assert_eq!(session["messages"].as_array().unwrap().len(), 2);

    Ok(())
}

/// Tests retrieving recent sessions with a count limit.
/// Verifies that sessions are returned in newest-first order and the count parameter is respected.
#[test]
fn test_get_recent_sessions() -> Result<()> {
    let mut server = McpServer::new()?;

    // Add 3 sessions
    for i in 1..=3 {
        let log = json!({
            "title": format!("Session {}", i),
            "messages": [{"role": "user", "content": "hi"}]
        });
        server.send_request(json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "add_log",
                "arguments": { "data": log }
            },
            "id": i
        }))?;
    }

    // Get recent 2
    let req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "get_recent_sessions",
            "arguments": { "count": 2 }
        },
        "id": 10
    });

    let resp = server.send_request(req)?;
    let content_str = resp["result"]["content"][0]["text"].as_str().unwrap();
    let recent: Vec<Value> = serde_json::from_str(content_str)?;

    assert_eq!(recent.len(), 2);
    // Should be Session 3 and Session 2 (newest first)
    assert_eq!(recent[0]["title"], "Session 3");
    assert_eq!(recent[1]["title"], "Session 2");

    Ok(())
}

/// Tests the stats tool to verify session and message counts are tracked correctly.
#[test]
fn test_stats() -> Result<()> {
    let mut server = McpServer::new()?;

    // Add 1 session
    let log = json!({
        "title": "Session 1",
        "messages": [{"role": "user", "content": "hi"}]
    });
    server.send_request(json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": { "data": log }
        },
        "id": 1
    }))?;

    let req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "stats",
            "arguments": {}
        },
        "id": 2
    });

    let resp = server.send_request(req)?;
    let content_str = resp["result"]["content"][0]["text"].as_str().unwrap();
    let stats: Value = serde_json::from_str(content_str)?;

    assert!(stats["session_count"].as_u64().unwrap() >= 1);
    assert!(stats["message_count"].as_u64().unwrap() >= 1);

    Ok(())
}

/// Tests the ping method for basic server responsiveness.
#[test]
fn test_ping() -> Result<()> {
    let mut server = McpServer::new()?;
    let req = json!({
        "jsonrpc": "2.0",
        "method": "ping",
        "id": 1
    });
    let resp = server.send_request(req)?;
    assert!(resp["result"].is_object());
    Ok(())
}

/// Tests the notifications/initialized method.
/// Verifies that the notification is acknowledged with a null result and no error.
#[test]
fn test_notification_initialized() -> Result<()> {
    let mut server = McpServer::new()?;
    // Send notification with ID (to get response for test verification)
    let req_with_id = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "id": 1
    });

    let resp = server.send_request(req_with_id)?;
    assert!(resp["result"].is_null());
    assert!(resp["error"].is_null());
    Ok(())
}

/// Tests error handling for unknown methods.
/// Verifies that the server returns a JSON-RPC -32601 (Method not found) error.
#[test]
fn test_unknown_method() -> Result<()> {
    let mut server = McpServer::new()?;
    let req = json!({
        "jsonrpc": "2.0",
        "method": "not_a_method",
        "id": 1
    });
    let resp = server.send_request(req)?;
    assert!(resp["error"].is_object());
    assert_eq!(resp["error"]["code"], -32601);
    Ok(())
}

/// Tests error handling for invalid JSON input.
/// Verifies that the server returns a JSON-RPC -32700 (Parse error) error.
#[test]
fn test_invalid_json() -> Result<()> {
    let mut server = McpServer::new()?;
    let stdin = server.process.stdin.as_mut().expect("Failed to open stdin");
    writeln!(stdin, "{{ invalid json")?;

    let mut line = String::new();
    // Use reader directly as send_request expects valid JSON response immediately
    if server.reader.read_line(&mut line)? > 0 {
        let resp: Value = serde_json::from_str(&line)?;
        assert!(resp["error"].is_object());
        assert_eq!(resp["error"]["code"], -32700);
    }
    Ok(())
}

/// Tests various error cases for tool calls:
/// - Missing params (-32602: Invalid params)
/// - Invalid params structure (-32602: Invalid params)
/// - Unknown tool name (-32601: Method not found)
#[test]
fn test_tool_call_errors() -> Result<()> {
    let mut server = McpServer::new()?;

    // Missing params
    let req1 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 1
    });
    let resp1 = server.send_request(req1)?;
    assert_eq!(resp1["error"]["code"], -32602);

    // Invalid params structure
    let req2 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": "not_an_object",
        "id": 2
    });
    let resp2 = server.send_request(req2)?;
    assert_eq!(resp2["error"]["code"], -32602);

    // Unknown tool
    let req3 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "unknown_tool",
            "arguments": {}
        },
        "id": 3
    });
    let resp3 = server.send_request(req3)?;
    assert_eq!(resp3["error"]["code"], -32601);

    Ok(())
}

/// Tests adding a log from a file path.
/// Verifies that the is_file_path parameter correctly loads session data from a JSON file.
#[test]
fn test_add_log_file_path() -> Result<()> {
    let mut server = McpServer::new()?;
    let temp_dir = TempDir::new()?;
    let file_path = temp_dir.path().join("session.json");

    let session_json = json!({
        "title": "File Session",
        "messages": [{"role": "user", "content": "from file"}]
    });
    std::fs::write(&file_path, serde_json::to_string(&session_json)?)?;

    let req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": {
                "data": file_path.to_str().unwrap(),
                "is_file_path": true
            }
        },
        "id": 1
    });

    let resp = server.send_request(req)?;
    assert!(resp["error"].is_null());

    // Verify
    let search_req = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": "search", "arguments": { "query": "from file" } },
        "id": 2
    });
    let search_resp = server.send_request(search_req)?;
    let content = search_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(content.contains("File Session"));

    Ok(())
}

/// Tests adding multiple sessions at once via arrays.
/// Tests both simple session arrays and ChatGPT export format.
#[test]
fn test_add_log_arrays() -> Result<()> {
    let mut server = McpServer::new()?;

    // 1. Array of ChatSessionInput
    let session_array = json!([
        {
            "title": "Array Session 1",
            "messages": [{"role": "user", "content": "msg1"}]
        },
        {
            "title": "Array Session 2",
            "messages": [{"role": "user", "content": "msg2"}]
        }
    ]);

    let req1 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": { "data": session_array }
        },
        "id": 1
    });
    let resp1 = server.send_request(req1)?;
    let msg1 = resp1["result"]["content"][0]["text"].as_str().unwrap();
    assert!(msg1.contains("Imported 2 sessions"));

    // 2. Chat GPT Export (Array of ChatGptConversation)
    // Structure: [{ "title": "...", "mapping": { ... } }]
    // We need to mock a minimal valid ChatGPT conversation structure that strict parsing accepts
    let gpt_export = json!([
        {
            "id": "gpt-conv-1",
            "title": "GPT Session",
            "create_time": 1234567890.0,
            "current_node": "uuid-1",
            "mapping": {
                "uuid-1": {
                    "id": "uuid-1",
                    "parent": null,
                    "message": {
                        "id": "msg-1",
                        "author": { "role": "user" },
                        "content": { "content_type": "text", "parts": ["gpt msg"] },
                        "create_time": 1234567890.0
                    }
                }
            }
        }
    ]);

    let req2 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": { "data": gpt_export }
        },
        "id": 2
    });
    let resp2 = server.send_request(req2)?;
    let msg2 = resp2["result"]["content"][0]["text"].as_str().unwrap();
    assert!(msg2.contains("Imported 1/1 conversations"));

    Ok(())
}

/// Tests error handling in add_log for:
/// - Missing required 'data' argument
/// - Invalid file path when is_file_path is true
#[test]
fn test_add_log_errors() -> Result<()> {
    let mut server = McpServer::new()?;

    // Missing 'data'
    let req1 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": {}
        },
        "id": 1
    });
    let resp1 = server.send_request(req1)?;
    assert!(
        resp1["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Missing required argument")
    );

    // Invalid file path
    // server.send_request might fail if server crashes or closes, so we handle basic response check
    let req2 = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "add_log",
            "arguments": {
                "data": "/non/existent/file.json",
                "is_file_path": true
            }
        },
        "id": 2
    });
    let resp2 = server.send_request(req2)?;
    assert!(
        resp2["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Input processing failed")
    );

    Ok(())
}
