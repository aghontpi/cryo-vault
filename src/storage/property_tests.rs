use super::*;
use crate::schema::{ChatSessionV1, MessageRole, MessageV1, ToolCall, ToolOutput};
use proptest::prelude::*;
use std::collections::HashMap;
use tempfile::TempDir;

/// Random metadata JSON maps.
fn arb_metadata_json() -> impl Strategy<Value = String> {
    prop::collection::hash_map(
        prop::string::string_regex("[a-z_]{1,10}").unwrap(),
        prop::string::string_regex("[a-zA-Z0-9 ]{0,20}").unwrap(),
        0..3,
    )
    .prop_map(|map: HashMap<String, String>| {
        if map.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
        }
    })
}

/// Random tool calls.
fn arb_tool_call() -> impl Strategy<Value = ToolCall> {
    (
        prop::string::string_regex("[a-z_]{3,20}").unwrap(),
        prop::string::string_regex(r#"\{"[a-z]+":"[a-z0-9]+"\}"#).unwrap(),
        prop::option::of(prop::string::string_regex("[a-z0-9]{5,10}").unwrap()),
    )
        .prop_map(|(name, arguments, id)| ToolCall {
            name,
            arguments,
            id,
        })
}

/// Random tool outputs.
fn arb_tool_output() -> impl Strategy<Value = ToolOutput> {
    (
        prop::option::of(prop::string::string_regex("[a-z0-9]{5,10}").unwrap()),
        prop::string::string_regex("[a-zA-Z0-9 ]{1,50}").unwrap(),
    )
        .prop_map(|(tool_call_id, content)| ToolOutput {
            tool_call_id,
            content,
        })
}

/// All message roles.
fn arb_message_role() -> impl Strategy<Value = MessageRole> {
    prop::sample::select(vec![
        MessageRole::User,
        MessageRole::Model,
        MessageRole::System,
        MessageRole::Thought,
        MessageRole::Tool,
    ])
}

/// Random messages.
fn arb_message() -> impl Strategy<Value = MessageV1> {
    (
        prop::option::of(prop::string::string_regex("[a-z0-9]{1,10}").unwrap()),
        arb_message_role(),
        // Allow unicode and special characters, but ensure non-empty after trim
        any::<String>()
            .prop_filter("non-empty content", |s| !s.trim().is_empty())
            .prop_map(|s| s.chars().take(100).collect::<String>()),
        prop::option::of(prop::collection::vec(arb_tool_call(), 0..2)),
        prop::option::of(prop::collection::vec(arb_tool_output(), 0..2)),
        prop::option::of(prop::string::string_regex("[a-z0-9]{5,15}").unwrap()),
        arb_metadata_json(),
    )
        .prop_map(
            |(id, role, content, tool_calls, tool_outputs, parent_id, metadata_json)| MessageV1 {
                id,
                role,
                content,
                tool_calls,
                tool_outputs,
                parent_id,
                metadata_json,
            },
        )
}

/// Random chat sessions.
fn arb_session() -> impl Strategy<Value = ChatSessionV1> {
    (
        prop::string::string_regex("[a-z0-9]{5,20}").unwrap(),
        prop::option::of(prop::string::string_regex("[a-zA-Z0-9 ]{1,50}").unwrap()),
        prop::option::of(prop::string::string_regex("(chatgpt|gemini|claude)-export").unwrap()),
        prop::option::of(prop::string::string_regex("(gpt-4|gemini-pro|claude-3)").unwrap()),
        prop::option::of(1000u64..1_000_000u64),
        arb_metadata_json(),
        prop::collection::vec(arb_message(), 1..10),
    )
        .prop_map(
            |(id, title, source, model, created_at, metadata_json, messages)| ChatSessionV1 {
                id,
                title,
                source,
                model,
                created_at,
                metadata_json,
                messages,
            },
        )
}

fn create_test_storage() -> (Storage, TempDir) {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let storage = Storage::new(temp_dir.path().to_path_buf());
    (storage, temp_dir)
}

/// Deep comparison of two messages.
fn assert_messages_equal(
    original: &MessageV1,
    retrieved: &MessageV1,
    index: usize,
) -> Result<(), String> {
    if original.id != retrieved.id {
        return Err(format!(
            "Message[{}] ID mismatch: expected {:?}, got {:?}",
            index, original.id, retrieved.id
        ));
    }
    if original.role != retrieved.role {
        return Err(format!(
            "Message[{}] role mismatch: expected {:?}, got {:?}",
            index, original.role, retrieved.role
        ));
    }
    if original.content != retrieved.content {
        return Err(format!(
            "Message[{}] content mismatch:\nExpected: {}\nGot: {}",
            index, original.content, retrieved.content
        ));
    }
    if original.parent_id != retrieved.parent_id {
        return Err(format!(
            "Message[{}] parent_id mismatch: expected {:?}, got {:?}",
            index, original.parent_id, retrieved.parent_id
        ));
    }
    if original.metadata_json != retrieved.metadata_json {
        return Err(format!(
            "Message[{}] metadata_json mismatch:\nExpected: {}\nGot: {}",
            index, original.metadata_json, retrieved.metadata_json
        ));
    }

    // Compare tool_calls
    match (&original.tool_calls, &retrieved.tool_calls) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            return Err(format!(
                "Message[{}] tool_calls presence mismatch: original has {:?}, retrieved has {:?}",
                index,
                original.tool_calls.is_some(),
                retrieved.tool_calls.is_some()
            ));
        }
        (Some(orig), Some(retr)) if orig.len() != retr.len() => {
            return Err(format!(
                "Message[{}] tool_calls length mismatch: expected {}, got {}",
                index,
                orig.len(),
                retr.len()
            ));
        }
        _ => {}
    }

    // Compare tool_outputs
    match (&original.tool_outputs, &retrieved.tool_outputs) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => {
            return Err(format!("Message[{}] tool_outputs presence mismatch", index));
        }
        (Some(orig), Some(retr)) if orig.len() != retr.len() => {
            return Err(format!(
                "Message[{}] tool_outputs length mismatch: expected {}, got {}",
                index,
                orig.len(),
                retr.len()
            ));
        }
        _ => {}
    }

    Ok(())
}

// Configure proptest: 32 cases for speed, increased shrinking for better debugging
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        max_shrink_iters: 10000,
        .. ProptestConfig::default()
    })]

    /// Verifies that writing and reading a session preserves all data.
    #[test]
    fn prop_serialization_roundtrip(session in arb_session()) {
        let (storage, _temp) = create_test_storage();
        let original_id = session.id.clone();

        // Write session
        storage.append_session(session.clone()).unwrap();

        // Read it back
        let retrieved = storage.get_session_by_id(&original_id)
            .unwrap()
            .expect("Session must exist after write");

        // Full field-by-field comparison
        prop_assert_eq!(
            &retrieved.id, &session.id,
            "Session ID mismatch"
        );
        prop_assert_eq!(
            &retrieved.title, &session.title,
            "Title mismatch: expected {:?}, got {:?}", session.title, retrieved.title
        );
        prop_assert_eq!(
            &retrieved.source, &session.source,
            "Source mismatch: expected {:?}, got {:?}", session.source, retrieved.source
        );
        prop_assert_eq!(
            &retrieved.model, &session.model,
            "Model mismatch: expected {:?}, got {:?}", session.model, retrieved.model
        );
        prop_assert_eq!(
            &retrieved.created_at, &session.created_at,
            "Timestamp mismatch: expected {:?}, got {:?}", session.created_at, retrieved.created_at
        );
        prop_assert_eq!(
            &retrieved.metadata_json, &session.metadata_json,
            "Metadata JSON mismatch:\nExpected: {}\nGot: {}",
            session.metadata_json, retrieved.metadata_json
        );
        prop_assert_eq!(
            retrieved.messages.len(), session.messages.len(),
            "Message count mismatch: expected {}, got {}",
            session.messages.len(), retrieved.messages.len()
        );

        // Compare each message
        for (i, (orig, retr)) in session.messages.iter().zip(retrieved.messages.iter()).enumerate() {
            if let Err(e) = assert_messages_equal(orig, retr, i) {
                return Err(proptest::test_runner::TestCaseError::fail(e));
            }
        }
    }

    /// Verifies that search results contain sessions with the matching term.
    #[test]
    fn prop_search_consistency(session in arb_session()) {
        let (storage, _temp) = create_test_storage();

        // Ensure session has searchable content
        if session.messages.is_empty() || session.messages[0].content.trim().is_empty() {
            return Ok(());
        }

        storage.append_session(session.clone()).unwrap();

        // Extract first word from first message
        let search_term = session.messages[0]
            .content
            .split_whitespace()
            .next()
            .unwrap_or("");

        if !search_term.is_empty() {
            let results = storage.search(search_term, None, None).unwrap();

            // Should find at least one result (the session we just added)
            prop_assert!(
                results.iter().any(|s| s.id == session.id),
                "Failed to find session {} with search term '{}'. Message content: '{}'",
                session.id,
                search_term,
                session.messages[0].content
            );
        }
    }

    /// Verifies that scan_all returns all written sessions.
    #[test]
    fn prop_ordering_preservation(sessions in prop::collection::vec(arb_session(), 1..5)) {
        let (storage, _temp) = create_test_storage();

        let mut session_ids = Vec::new();

        // Store all sessions
        for session in sessions {
            session_ids.push(session.id.clone());
            storage.append_session(session).unwrap();
        }

        // Scan all and verify all sessions are present
        let all_sessions = storage.scan_all().unwrap();

        prop_assert_eq!(
            all_sessions.len(), session_ids.len(),
            "Session count mismatch after scan_all"
        );

        for id in &session_ids {
            prop_assert!(
                all_sessions.iter().any(|s| &s.id == id),
                "Missing session ID after scan_all: {}",
                id
            );
        }
    }

    /// Verifies that reindexing retains all data.
    #[test]
    fn prop_reindex_integrity(sessions in prop::collection::vec(arb_session(), 1..5)) {
        let (storage, _temp) = create_test_storage();

        // Store sessions
        for session in &sessions {
            storage.append_session(session.clone()).unwrap();
        }

        // Reindex
        let count = storage.reindex().unwrap();
        prop_assert_eq!(
            count, sessions.len(),
            "Reindex count mismatch: expected {}, got {}",
            sessions.len(), count
        );

        // Verify all sessions can still be retrieved by ID
        for session in &sessions {
            let retrieved = storage.get_session_by_id(&session.id).unwrap();
            prop_assert!(
                retrieved.is_some(),
                "Failed to retrieve session after reindex: {}",
                session.id
            );
        }
    }

    /// Verifies that stats accurately reflect the count and size of data.
    #[test]
    fn prop_stats_accuracy(sessions in prop::collection::vec(arb_session(), 1..10)) {
        let (storage, _temp) = create_test_storage();

        let mut total_messages = 0u64;

        for session in &sessions {
            total_messages += session.messages.len() as u64;
            storage.append_session(session.clone()).unwrap();
        }

        let stats = storage.get_stats().unwrap();

        prop_assert_eq!(
            stats.session_count, sessions.len() as u64,
            "Session count mismatch in stats"
        );
        prop_assert_eq!(
            stats.message_count, total_messages,
            "Message count mismatch in stats"
        );
        prop_assert!(
            stats.total_size_bytes > 0,
            "Total size should be positive"
        );
        prop_assert!(
            stats.data_compressed_bytes > 0,
            "Compressed data size should be positive"
        );
    }
}

#[cfg(test)]
mod edge_cases {
    use super::*;

    /// Checks all role variants.
    #[test]
    fn test_all_message_roles_roundtrip() {
        let (storage, _temp) = create_test_storage();

        let roles = [
            MessageRole::User,
            MessageRole::Model,
            MessageRole::System,
            MessageRole::Thought,
            MessageRole::Tool,
        ];

        for (i, role) in roles.iter().enumerate() {
            let session = ChatSessionV1 {
                id: format!("test-role-{}", i),
                title: Some(format!("Test {:?}", role)),
                source: None,
                model: None,
                created_at: Some(1000),
                metadata_json: String::new(),
                messages: vec![MessageV1 {
                    id: Some(format!("msg-{}", i)),
                    role: role.clone(),
                    content: format!("Testing {:?} role", role),
                    tool_calls: None,
                    tool_outputs: None,
                    parent_id: None,
                    metadata_json: String::new(),
                }],
            };

            storage.append_session(session.clone()).unwrap();
            let retrieved = storage.get_session_by_id(&session.id).unwrap().unwrap();

            assert_eq!(
                retrieved.messages[0].role, *role,
                "Role mismatch for {:?}",
                role
            );
        }
    }

    /// Checks unicode/emoji content.
    #[test]
    fn test_unicode_content_roundtrip() {
        let (storage, _temp) = create_test_storage();

        let session = ChatSessionV1 {
            id: "test-unicode".to_string(),
            title: Some("Unicode Test 世界".to_string()),
            source: None,
            model: None,
            created_at: Some(1000),
            metadata_json: String::new(),
            messages: vec![MessageV1 {
                id: Some("msg1".to_string()),
                role: MessageRole::User,
                content: "Hello 世界 🌍 \n\t Special chars: <>&\"'".to_string(),
                tool_calls: None,
                tool_outputs: None,
                parent_id: None,
                metadata_json: String::new(),
            }],
        };

        storage.append_session(session.clone()).unwrap();
        let retrieved = storage.get_session_by_id(&session.id).unwrap().unwrap();

        assert_eq!(retrieved.messages[0].content, session.messages[0].content);
        assert_eq!(retrieved.title, session.title);
    }

    /// Checks nested metadata JSON strings.
    #[test]
    fn test_metadata_json_roundtrip() {
        let (storage, _temp) = create_test_storage();

        let metadata = r#"{"key1":"value1","key2":"value2"}"#;

        let session = ChatSessionV1 {
            id: "test-metadata".to_string(),
            title: None,
            source: None,
            model: None,
            created_at: Some(1000),
            metadata_json: metadata.to_string(),
            messages: vec![MessageV1 {
                id: None,
                role: MessageRole::User,
                content: "Test".to_string(),
                tool_calls: None,
                tool_outputs: None,
                parent_id: None,
                metadata_json: r#"{"msg_meta":"data"}"#.to_string(),
            }],
        };

        storage.append_session(session.clone()).unwrap();
        let retrieved = storage.get_session_by_id(&session.id).unwrap().unwrap();

        assert_eq!(retrieved.metadata_json, session.metadata_json);
        assert_eq!(
            retrieved.messages[0].metadata_json,
            session.messages[0].metadata_json
        );
    }

    /// Checks edge cases for timestamps (0, max, none).
    #[test]
    fn test_boundary_timestamps() {
        let (storage, _temp) = create_test_storage();

        let test_cases = vec![
            ("zero", Some(0u64)),
            ("max", Some(u64::MAX)),
            ("none", None),
        ];

        for (name, timestamp) in test_cases {
            let session = ChatSessionV1 {
                id: format!("test-ts-{}", name),
                title: None,
                source: None,
                model: None,
                created_at: timestamp,
                metadata_json: String::new(),
                messages: vec![MessageV1 {
                    id: None,
                    role: MessageRole::User,
                    content: "Test".to_string(),
                    tool_calls: None,
                    tool_outputs: None,
                    parent_id: None,
                    metadata_json: String::new(),
                }],
            };

            storage.append_session(session.clone()).unwrap();
            let retrieved = storage.get_session_by_id(&session.id).unwrap().unwrap();

            assert_eq!(
                retrieved.created_at, timestamp,
                "Timestamp mismatch for test case: {}",
                name
            );
        }
    }
}
