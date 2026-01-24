use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use uuid::Uuid;

/// The top-level storage wrapper. This is what we write to disk.
/// It uses a single byte tag (0, 1, 2...) to determine the version.
#[derive(Serialize, Deserialize, Debug)]
pub enum StoredSession {
    V1(ChatSessionV1),
    // Future: V2(ChatSessionV2)
}

/// The Input DTO (V1).
/// Used for interpreting JSON input with loose typing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionInput {
    pub id: Option<String>,
    pub title: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<u64>,
    #[serde(flatten)]
    pub metadata: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub messages: Vec<MessageInput>,
}

/// The Core Session Struct (Storage V1).
/// Aligned for Bincode efficiency (No untyped Value, No Flatten).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChatSessionV1 {
    pub id: String,
    pub title: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<u64>,
    /// Stored as JSON string to ensure Bincode compatibility
    pub metadata_json: String,
    pub messages: Vec<MessageV1>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MessageInput {
    pub role: MessageRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_outputs: Option<Vec<ToolOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(flatten)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MessageV1 {
    pub role: MessageRole,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_outputs: Option<Vec<ToolOutput>>,
    pub id: Option<String>,
    pub parent_id: Option<String>,
    /// Stored as JSON string
    pub metadata_json: String,
}

impl From<ChatSessionInput> for ChatSessionV1 {
    fn from(input: ChatSessionInput) -> Self {
        let id = input.id.unwrap_or_else(|| {
            let generated_id = Uuid::new_v4().to_string();
            tracing::trace!(session_id = %generated_id, "Auto-generated UUID for session without ID");
            generated_id
        });

        Self {
            id,
            title: input.title,
            source: input.source,
            model: input.model,
            created_at: input.created_at,
            metadata_json: serde_json::to_string(&input.metadata).unwrap_or_default(),
            messages: input.messages.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<MessageInput> for MessageV1 {
    fn from(input: MessageInput) -> Self {
        Self {
            role: input.role,
            content: input.content,
            tool_calls: input.tool_calls,
            tool_outputs: input.tool_outputs,
            id: input.id,
            parent_id: input.parent_id,
            metadata_json: serde_json::to_string(&input.metadata).unwrap_or_default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    System,
    #[serde(alias = "assistant")]
    /// Standard AI response
    Model,
    /// Internal reasoning/Chain of thought
    Thought,
    /// Tool execution result
    Tool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub arguments: String,
    pub id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolOutput {
    pub tool_call_id: Option<String>,
    pub content: String,
}

/// Events for streaming ingestion
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")] // e.g. { "type": "chunk", ... }
pub enum StreamEvent {
    #[serde(rename = "session_start")]
    SessionStart {
        session_id: String,
        #[serde(flatten)]
        metadata: HashMap<String, serde_json::Value>,
    },
    #[serde(rename = "message")]
    AppendMessage {
        session_id: String,
        message: MessageInput,
    },
    #[serde(rename = "finalize")]
    Finalize { session_id: String },
}

// --- ChatGPT Export Support ---

#[derive(Deserialize, Debug)]
pub struct ChatGptConversation {
    pub id: String,
    pub title: Option<String>,
    pub create_time: Option<f64>,
    pub mapping: HashMap<String, ChatGptNode>,
    pub current_node: Option<String>,
}

#[derive(Deserialize, Debug)]
/// Represents a node in the ChatGPT conversation tree structure.
/// Each node contains an optional message and links to its parent.
pub struct ChatGptNode {
    pub id: String,
    pub message: Option<ChatGptMessage>,
    pub parent: Option<String>,
}

#[derive(Deserialize, Debug)]
/// Represents a message in a ChatGPT export.
pub struct ChatGptMessage {
    pub id: String,
    pub author: ChatGptAuthor,
    pub content: ChatGptContent,
    pub create_time: Option<f64>,
}

/// Author information for a ChatGPT message.
#[derive(Deserialize, Debug)]
pub struct ChatGptAuthor {
    pub role: String,
}

#[derive(Deserialize, Debug)]
/// Content structure for ChatGPT messages.
/// Messages can have multiple parts, which may include text or structured data.
pub struct ChatGptContent {
    pub content_type: String,
    pub parts: Option<Vec<serde_json::Value>>,
}

impl TryFrom<ChatGptConversation> for ChatSessionV1 {
    type Error = anyhow::Error;

    /// Converts a ChatGPT export conversation into our internal stored session format.
    ///
    /// - Traverses messages backwards from the `current_node`.
    /// - Extracts text content from message parts.
    /// - Filters out empty messages (structural nodes).
    /// - Reverses the list to get chronological order.
    fn try_from(raw: ChatGptConversation) -> Result<Self, Self::Error> {
        let mut messages = Vec::new();

        // Traverse backwards from current_node
        let mut curr = raw.current_node.clone();

        while let Some(node_id) = curr {
            if let Some(node) = raw.mapping.get(&node_id) {
                if let Some(msg) = &node.message {
                    let mut content_str = String::new();
                    if let Some(parts) = &msg.content.parts {
                        for part in parts {
                            if let Some(s) = part.as_str() {
                                content_str.push_str(s);
                            } else if part.is_object() {
                                // Handle mixed content (plugins usually), skip for now or stringify
                            }
                        }
                    }

                    if !content_str.is_empty() {
                        let role = match msg.author.role.as_str() {
                            "user" => MessageRole::User,
                            "assistant" => MessageRole::Model,
                            "system" => MessageRole::System,
                            "tool" => MessageRole::Tool,
                            _ => MessageRole::User,
                        };

                        messages.push(MessageV1 {
                            role,
                            content: content_str,
                            tool_calls: None,
                            tool_outputs: None,
                            id: Some(msg.id.clone()),
                            parent_id: node.parent.clone(),
                            metadata_json: String::new(),
                        });
                    }
                }
                curr = node.parent.clone();
            } else {
                break;
            }
        }

        messages.reverse();

        Ok(ChatSessionV1 {
            id: raw.id,
            title: raw.title,
            source: Some("chatgpt-export".to_string()),
            model: None, // Could infer from message metadata if needed
            created_at: raw.create_time.map(|t| t as u64),
            metadata_json: String::new(), // Could store other fields here
            messages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chatgpt_export_parsing() {
        let json_data = r#"
        {
            "id": "conv1",
            "title": "Test Chat",
            "create_time": 1678886400,
            "current_node": "node3",
            "mapping": {
                "node1": {
                    "id": "node1",
                    "parent": null,
                    "message": {
                        "id": "msg1",
                        "author": { "role": "system" },
                        "content": { "content_type": "text", "parts": ["System prompt"] },
                        "create_time": 1678886400
                    }
                },
                "node2": {
                    "id": "node2",
                    "parent": "node1",
                    "message": {
                        "id": "msg2",
                        "author": { "role": "user" },
                        "content": { "content_type": "text", "parts": ["Hello"] },
                        "create_time": 1678886401
                    }
                },
                "node3": {
                    "id": "node3",
                    "parent": "node2",
                    "message": {
                        "id": "msg3",
                        "author": { "role": "assistant" },
                        "content": { "content_type": "text", "parts": ["Hi there"] },
                        "create_time": 1678886402
                    }
                }
            }
        }
        "#;

        let conversation: ChatGptConversation =
            serde_json::from_str(json_data).expect("Failed to parse JSON");
        assert_eq!(conversation.id, "conv1");

        let session: ChatSessionV1 = conversation.try_into().expect("Failed to convert");
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].content, "System prompt");
        assert_eq!(session.messages[1].content, "Hello");
        assert_eq!(session.messages[2].content, "Hi there");
        assert_eq!(session.created_at, Some(1678886400));
    }

    #[test]
    fn test_chatgpt_mixed_content() {
        // Test with non-string parts (should be skipped or handled)
        let json_data = r#"
         {
             "id": "conv2",
             "current_node": "node1",
             "mapping": {
                 "node1": {
                     "id": "node1",
                     "parent": null,
                     "message": {
                         "id": "msg1",
                         "author": { "role": "user" },
                         "content": { "content_type": "text", "parts": ["Text", {"some": "obj"}] },
                         "create_time": 123
                     }
                 }
             }
         }
         "#;

        let conversation: ChatGptConversation = serde_json::from_str(json_data).unwrap();
        let session: ChatSessionV1 = conversation.try_into().unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Text"); // Object part skipped
    }
}
