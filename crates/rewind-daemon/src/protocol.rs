//! Types representing Claude Code conversation data parsed from .jsonl transcripts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Raw JSONL line from Claude Code transcript ──────────────────────────────

/// A single line from a Claude Code .jsonl transcript file.
/// We only parse the fields we need; the rest is ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TranscriptLine {
    User {
        uuid: String,
        #[serde(rename = "parentUuid")]
        parent_uuid: Option<String>,
        #[serde(rename = "promptId")]
        prompt_id: Option<String>,
        message: UserMessage,
        timestamp: Option<String>,
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
        cwd: Option<String>,
        #[serde(rename = "gitBranch")]
        git_branch: Option<String>,
    },
    Assistant {
        uuid: String,
        #[serde(rename = "parentUuid")]
        parent_uuid: Option<String>,
        message: AssistantMessage,
        timestamp: Option<String>,
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
        cwd: Option<String>,
        #[serde(rename = "gitBranch")]
        git_branch: Option<String>,
    },
    #[serde(rename = "file-history-snapshot")]
    FileHistorySnapshot {
        #[serde(rename = "messageId")]
        message_id: Option<String>,
        snapshot: Option<SnapshotData>,
    },
    System {
        #[serde(flatten)]
        _extra: HashMap<String, serde_json::Value>,
    },
    Progress {
        #[serde(flatten)]
        _extra: HashMap<String, serde_json::Value>,
    },
    #[serde(rename = "last-prompt")]
    LastPrompt {
        #[serde(rename = "lastPrompt")]
        last_prompt: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserMessage {
    pub content: serde_json::Value, // can be string or array of content blocks
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssistantMessage {
    pub content: Option<Vec<ContentBlock>>,
    pub model: Option<String>,
    pub usage: Option<UsageInfo>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: Option<String>,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        id: Option<String>,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        #[serde(rename = "tool_use_id")]
        tool_use_id: Option<String>,
        content: Option<serde_json::Value>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageInfo {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotData {
    #[serde(rename = "trackedFileBackups")]
    pub tracked_file_backups: Option<HashMap<String, serde_json::Value>>,
    pub timestamp: Option<String>,
}

// ── Processed timeline types (what the UI consumes) ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_dir: String,
    pub git_branch: Option<String>,
    pub started_at: Option<String>,
    pub prompt_count: usize,
    pub tool_call_count: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub index: usize,
    pub prompt_text: String,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub text_responses: Vec<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub has_errors: bool,
    pub files_touched: Vec<String>,
    pub snapshot_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub summary: String,
    pub file_path: Option<String>,
}

impl UserMessage {
    /// Extract the text content from a user message (handles both string and array formats).
    pub fn text(&self) -> String {
        match &self.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => {
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        if obj.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                                return text.to_string();
                            }
                        }
                    }
                }
                String::new()
            }
            _ => String::new(),
        }
    }
}
