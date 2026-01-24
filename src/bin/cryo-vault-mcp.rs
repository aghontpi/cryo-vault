use anyhow::Result;
use cryo_vault::schema::{ChatGptConversation, ChatSessionInput, ChatSessionV1};
use cryo_vault::storage::Storage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, BufRead};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

// MCP Types
#[derive(Serialize, Deserialize, Debug)]
struct CallToolParams {
    name: String,
    arguments: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Tool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

fn main() -> Result<()> {
    // Determine DB path
    let db_path = std::env::var("CRYO_DB_PATH")
        .map(PathBuf::from)
        .or_else(|_| {
            dirs::home_dir()
                .map(|d| d.join(".cryo"))
                .ok_or(anyhow::anyhow!("No home dir"))
        })
        .unwrap_or_else(|_| PathBuf::from(".cryo"));

    let storage = Storage::new(db_path.clone());

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => {
                let id = req.id.clone();
                let response = handle_request(req, &storage, &db_path);

                // Only send response if request has an ID (normal requests)
                if id.is_some() {
                    let json_resp = serde_json::to_string(&response)?;
                    println!("{}", json_resp);
                }
            }
            Err(e) => {
                // Parse error
                let resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                    id: None,
                };
                println!("{}", serde_json::to_string(&resp)?);
            }
        }
    }

    Ok(())
}

/// Handles incoming JSON-RPC requests for the MCP server.
///
/// Note: Per MCP spec, requests have IDs and expect responses.
/// Notifications (like "notifications/initialized") may have null IDs and typically don't require responses,
/// but we return empty responses for compatibility.
fn handle_request(req: JsonRpcRequest, storage: &Storage, db_path: &PathBuf) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": "2024-11-05", // Spec version
                "serverInfo": {
                    "name": "cryo-vault-mcp",
                    "version": "0.1.0"
                },
                "capabilities": {
                    "tools": {}
                }
            });
            success(req.id, result)
        }
        "notifications/initialized" => {
            // Return empty response for compatibility (notification typically has null ID)
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: None, // Notifications don't have results, but strict RPC might want checking
                error: None,
                id: req.id, // Should be null for notifications
            }
        }
        "tools/list" => {
            let tools = vec![
                Tool {
                    name: "add_log".to_string(),
                    description: "Ingest chat logs. Supports: \n\
                                  1. Direct JSON Object (Single Session)\n\
                                  2. Direct JSON Array (Multiple Sessions)\n\
                                  3. Raw JSON String (e.g. \"{\\\"messages\\\": ...}\")\n\
                                  4. File Path (set is_file_path=true)\n\
                                  \n\
                                  Session Object Schema:\n\
                                  - id: Option<String> (UUID generated if missing)\n\
                                  - messages: Array of objects with:\n\
                                    - role: \"user\" | \"model\" | \"system\"\n\
                                    - content: String\n\
                                  - title: Option<String>\n\
                                  - model: Option<String>\n\
                                  - created_at: Option<u64> (Unix timestamp)"
                        .to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "data": {
                                "description": "Log content (string, object, or array) or file path if is_file_path=true",
                                "anyOf": [
                                    { "type": "string" },
                                    { "type": "object" },
                                    { "type": "array" }
                                ]
                            },
                             "is_file_path": { "type": "boolean", "description": "True if data is a file path. Defaults to false." }
                        },
                        "required": ["data"]
                    }),
                },
                Tool {
                    name: "search".to_string(),
                    description: "Search the archive for sessions matching a query.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"]
                    }),
                },
                Tool {
                    name: "read_session".to_string(),
                    description: "Retrieve full details of a session by ID.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" }
                        },
                        "required": ["id"]
                    }),
                },
                Tool {
                    name: "get_recent_sessions".to_string(),
                    description: "Retrieve the last N sessions (newest first).".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "count": { "type": "integer", "minimum": 1, "default": 10 }
                        },
                    }),
                },
                Tool {
                    name: "stats".to_string(),
                    description: "Get archive statistics.".to_string(),
                    input_schema: json!({ "type": "object" }),
                },
            ];

            success(req.id, json!({ "tools": tools }))
        }
        "tools/call" => {
            // Parse params
            if let Some(params_val) = req.params {
                match serde_json::from_value::<CallToolParams>(params_val) {
                    Ok(params) => handle_tool_call(params, storage, db_path, req.id),
                    Err(e) => error(req.id, -32602, format!("Invalid params: {}", e)),
                }
            } else {
                error(req.id, -32602, "Missing params".to_string())
            }
        }
        "ping" => success(req.id, json!({})),
        _ => error(req.id, -32601, format!("Method not found: {}", req.method)),
    }
}

fn handle_tool_call(
    params: CallToolParams,
    storage: &Storage,
    _db_path: &PathBuf,
    id: Option<Value>,
) -> JsonRpcResponse {
    match params.name.as_str() {
        "add_log" => {
            let args = params.arguments.unwrap_or(json!({}));
            // Support 'data' (new) or 'input' (old)
            let input_val = args.get("data").or_else(|| args.get("input"));
            let is_file_path = args
                .get("is_file_path")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if input_val.is_none() {
                return error(id, -32602, "Missing required argument: data".to_string());
            }

            let val_ref = input_val.unwrap();

            // Helper to get JSON Value
            let get_json = || -> Result<Value, anyhow::Error> {
                if is_file_path {
                    let path = val_ref.as_str().ok_or_else(|| {
                        anyhow::anyhow!("is_file_path is true but data is not a string")
                    })?;
                    let content = std::fs::read_to_string(path)?;
                    serde_json::from_str(&content)
                        .map_err(|e| anyhow::anyhow!("File content invalid JSON: {}", e))
                } else {
                    match val_ref {
                        Value::String(s) => serde_json::from_str(s)
                            .or_else(|_| Err(anyhow::anyhow!("String data is not valid JSON"))),
                        _ => Ok(val_ref.clone()),
                    }
                }
            };

            match get_json() {
                Ok(json_val) => match ingest_content(storage, json_val) {
                    Ok(msg) => success(id, json!({ "content": [{ "type": "text", "text": msg }] })),
                    Err(e) => error(id, 1, format!("Ingestion failed: {}", e)),
                },
                Err(e) => error(id, 1, format!("Input processing failed: {}", e)),
            }
        }
        "search" => {
            let args = params.arguments.unwrap_or(json!({}));
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");

            match storage.search(query, None, None) {
                Ok(sessions) => {
                    // Brief summary for search
                    let summaries: Vec<Value> = sessions
                        .into_iter()
                        .map(|s| {
                            json!({
                                "id": s.id,
                                "title": s.title,
                                "message_count": s.messages.len(),
                                "created_at": s.created_at
                            })
                        })
                        .collect();
                    success(
                        id,
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string(&summaries).unwrap() }] }),
                    )
                }
                Err(e) => error(id, 1, format!("Search failed: {}", e)),
            }
        }
        "read_session" => {
            let args = params.arguments.unwrap_or(json!({}));
            let session_id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");

            match storage.get_session_by_id(session_id) {
                Ok(opt) => match opt {
                    Some(session) => success(
                        id,
                        json!({ "content": [{ "type": "text", "text": serde_json::to_string(&session).unwrap() }] }),
                    ),
                    None => error(id, 1, "Session not found".to_string()),
                },
                Err(e) => error(id, 1, format!("Error reading session: {}", e)),
            }
        }
        "get_recent_sessions" => {
            let args = params.arguments.unwrap_or(json!({}));
            let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

            match get_recent(storage, count) {
                Ok(sessions) => success(
                    id,
                    json!({ "content": [{ "type": "text", "text": serde_json::to_string(&sessions).unwrap() }] }),
                ),
                Err(e) => error(id, 1, format!("Error getting recent sessions: {}", e)),
            }
        }
        "stats" => match storage.get_stats() {
            Ok(stats) => success(
                id,
                json!({ "content": [{ "type": "text", "text": serde_json::to_string(&stats).unwrap() }] }),
            ),
            Err(e) => error(id, 1, format!("Error getting stats: {}", e)),
        },
        _ => error(id, -32601, format!("Unknown tool: {}", params.name)),
    }
}

fn success(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        result: Some(result),
        error: None,
        id,
    }
}

fn error(id: Option<Value>, code: i32, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data: None,
        }),
        id,
    }
}

/// Ingest chat session content from various formats.
///
/// Attempts to parse the input in the following priority order:
/// 1. Single `ChatSessionInput` object
/// 2. Array of `ChatGptConversation` (ChatGPT export format)
/// 3. Array of `ChatSessionInput` objects
///
/// Returns a success message indicating how many sessions were imported.
fn ingest_content(storage: &Storage, content: Value) -> Result<String> {
    // Try ChatSessionInput (Single Object)
    if let Ok(input) = serde_json::from_value::<ChatSessionInput>(content.clone()) {
        let session: ChatSessionV1 = input.into();
        storage.append_session(session)?;
        return Ok("Session saved.".to_string());
    }

    // If it's an array, it could be ChatGPT export OR List of Sessions
    if let Ok(conversations) = serde_json::from_value::<Vec<ChatGptConversation>>(content.clone()) {
        let count = conversations.len();
        if count > 0 {
            let mut writer = storage.get_writer()?;
            let mut imported_count = 0;
            for conv in conversations {
                if let Ok(session) = conv.try_into() {
                    writer.append(session)?;
                    imported_count += 1;
                }
            }
            writer.flush()?;
            return Ok(format!(
                "Imported {}/{} conversations from ChatGPT export.",
                imported_count, count
            ));
        }
    }

    // Try Array of ChatSessionInput
    match serde_json::from_value::<Vec<ChatSessionInput>>(content) {
        Ok(sessions) => {
            let count = sessions.len();
            let mut writer = storage.get_writer()?;
            for input in sessions {
                let session: ChatSessionV1 = input.into();
                writer.append(session)?;
            }
            writer.flush()?;
            Ok(format!("Imported {} sessions.", count))
        }
        Err(_) => Err(anyhow::anyhow!(
            "Failed to parse input as Session, ChatGPT Export, or Session Array."
        )),
    }
}

/// Retrieves the most recent N sessions from storage.
///
/// Scans all sessions and returns the last `count` entries in reverse chronological order
/// (newest first).
fn get_recent(storage: &Storage, count: usize) -> Result<Vec<ChatSessionV1>> {
    let sessions = storage.scan_all()?;
    let len = sessions.len();
    let start = len.saturating_sub(count);
    let mut recent = sessions[start..].to_vec();
    recent.reverse(); // Newest first
    Ok(recent)
}
