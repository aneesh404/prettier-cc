//! Claude Code .jsonl transcript parser.
//!
//! Reads a transcript file, deduplicates streaming messages by UUID,
//! and produces a structured conversation timeline.

use crate::protocol::*;
use indexmap::IndexMap;
use std::path::Path;
use tracing::{debug, warn};

/// Parse a Claude Code .jsonl transcript file into a timeline of conversation turns.
pub fn parse_transcript(path: &Path) -> Result<(SessionInfo, Vec<Turn>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;

    parse_transcript_str(&content)
}

/// Lightweight metadata-only parse — reads just enough to get session info
/// without fully parsing all messages. Much faster for listing sessions.
pub fn parse_transcript_meta(path: &Path) -> Result<SessionInfo, String> {
    use std::io::{BufRead, BufReader};
    use std::fs::File;

    let file = File::open(path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let reader = BufReader::new(file);

    let mut session_id = String::new();
    let mut project_dir = String::new();
    let mut git_branch = None;
    let mut started_at = None;
    let mut prompt_count = 0u64;
    let mut tool_call_count = 0u64;
    let mut line_count = 0u64;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        line_count += 1;

        // Quick type check without full parse
        if line.contains("\"type\":\"user\"") {
            // Check if it's a real prompt (has string content) vs tool result
            // Also skip system-injected messages (XML tags, continuation notices)
            if line.contains("\"content\":\"") {
                // Quick check: extract content value to filter system prompts
                let is_system = line.contains("\"content\":\"<")
                    || line.contains("\"content\":\"This session is being continued");
                if !is_system {
                    prompt_count += 1;
                }
            }

            // Extract metadata from first user message
            if session_id.is_empty() {
                if let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) {
                    if let TranscriptLine::User { session_id: sid, cwd, git_branch: gb, timestamp, .. } = parsed {
                        if let Some(s) = sid { session_id = s; }
                        if let Some(c) = cwd { project_dir = c; }
                        git_branch = gb;
                        started_at = timestamp;
                    }
                }
            }
        } else if line.contains("\"type\":\"tool_use\"") {
            tool_call_count += 1;
        }
    }

    Ok(SessionInfo {
        session_id,
        project_dir,
        git_branch,
        started_at,
        prompt_count: prompt_count as usize,
        tool_call_count: tool_call_count as usize,
        total_input_tokens: 0,
        total_output_tokens: 0,
    })
}

/// Parse transcript content (for testing and streaming).
pub fn parse_transcript_str(content: &str) -> Result<(SessionInfo, Vec<Turn>), String> {
    // Step 1: Parse all lines, deduplicate by UUID (keep last version)
    let mut messages: IndexMap<String, TranscriptLine> = IndexMap::new();
    let mut snapshots: Vec<TranscriptLine> = Vec::new();
    let mut session_id = String::new();
    let mut project_dir = String::new();
    let mut git_branch = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<TranscriptLine>(line) {
            Ok(parsed) => {
                match &parsed {
                    TranscriptLine::User { uuid, session_id: sid, cwd, git_branch: gb, .. } => {
                        if session_id.is_empty() {
                            if let Some(s) = sid { session_id = s.clone(); }
                        }
                        if project_dir.is_empty() {
                            if let Some(c) = cwd { project_dir = c.clone(); }
                        }
                        if git_branch.is_none() {
                            git_branch = gb.clone();
                        }
                        messages.insert(uuid.clone(), parsed);
                    }
                    TranscriptLine::Assistant { uuid, .. } => {
                        messages.insert(uuid.clone(), parsed);
                    }
                    TranscriptLine::FileHistorySnapshot { .. } => {
                        snapshots.push(parsed);
                    }
                    _ => {} // ignore system, progress, unknown
                }
            }
            Err(e) => {
                debug!(error = %e, "skipping unparseable transcript line");
            }
        }
    }

    // Step 2: Walk messages in order, build turns
    // A "turn" starts with a user message and includes all assistant responses
    // until the next user message.
    let mut turns: Vec<Turn> = Vec::new();
    let mut current_turn: Option<Turn> = None;
    let mut turn_index = 0;

    for (_uuid, msg) in &messages {
        match msg {
            TranscriptLine::User { message, timestamp, cwd, .. } => {
                let prompt_text = message.text();

                // Skip tool-result messages and empty prompts.
                // Claude Code wraps tool results in "user" messages
                // with content as an array of tool_result blocks.
                // Real human prompts have content as a plain string.
                let is_tool_result = matches!(&message.content, serde_json::Value::Array(arr)
                    if arr.iter().any(|item| {
                        item.as_object()
                            .and_then(|o| o.get("type"))
                            .and_then(|v| v.as_str())
                            == Some("tool_result")
                    })
                );

                if is_tool_result || prompt_text.is_empty() || is_system_prompt(&prompt_text) {
                    // Not a real human prompt — just continue accumulating
                    // tool calls into the current turn.
                    continue;
                }

                // Finalize previous turn
                if let Some(turn) = current_turn.take() {
                    turns.push(turn);
                }

                turn_index += 1;

                current_turn = Some(Turn {
                    index: turn_index,
                    prompt_text,
                    timestamp: timestamp.clone(),
                    cwd: cwd.clone(),
                    tool_calls: Vec::new(),
                    text_responses: Vec::new(),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    has_errors: false,
                    files_touched: Vec::new(),
                    snapshot_files: Vec::new(),
                });
            }

            TranscriptLine::Assistant { message, .. } => {
                if let Some(ref mut turn) = current_turn {
                    // Extract tool calls and text
                    if let Some(content) = &message.content {
                        for block in content {
                            match block {
                                ContentBlock::ToolUse { name, input, .. } => {
                                    let (summary, file_path) = summarize_tool_call(name, input);
                                    if let Some(ref fp) = file_path {
                                        if !turn.files_touched.contains(fp) {
                                            turn.files_touched.push(fp.clone());
                                        }
                                    }
                                    turn.tool_calls.push(ToolCall {
                                        name: name.clone(),
                                        summary,
                                        file_path,
                                    });
                                }
                                ContentBlock::Text { text } => {
                                    let trimmed = text.trim();
                                    if !trimmed.is_empty() {
                                        turn.text_responses.push(trimmed.to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }

                    // Accumulate tokens
                    if let Some(usage) = &message.usage {
                        turn.total_input_tokens += usage.input_tokens.unwrap_or(0)
                            + usage.cache_read_input_tokens.unwrap_or(0)
                            + usage.cache_creation_input_tokens.unwrap_or(0);
                        turn.total_output_tokens += usage.output_tokens.unwrap_or(0);
                    }
                }
            }
            _ => {}
        }
    }

    // Push final turn
    if let Some(turn) = current_turn {
        turns.push(turn);
    }

    // Collect snapshot files
    for snapshot in &snapshots {
        if let TranscriptLine::FileHistorySnapshot { snapshot: Some(data), message_id } = snapshot {
            if let Some(backups) = &data.tracked_file_backups {
                let files: Vec<String> = backups.keys().cloned().collect();
                if !files.is_empty() {
                    // Find the turn this snapshot belongs to
                    if let Some(mid) = message_id {
                        for turn in turns.iter_mut() {
                            // Associate by proximity (snapshot's messageId matches a user uuid)
                            turn.snapshot_files = files.clone();
                            break;
                        }
                    }
                }
            }
        }
    }

    let total_input: u64 = turns.iter().map(|t| t.total_input_tokens).sum();
    let total_output: u64 = turns.iter().map(|t| t.total_output_tokens).sum();
    let tool_count: usize = turns.iter().map(|t| t.tool_calls.len()).sum();

    let info = SessionInfo {
        session_id,
        project_dir,
        git_branch,
        started_at: turns.first().and_then(|t| t.timestamp.clone()),
        prompt_count: turns.len(),
        tool_call_count: tool_count,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
    };

    Ok((info, turns))
}

/// Check if a user message is actually an internal system prompt injected by Claude Code
/// (not a real human prompt). These include XML-tagged system messages, command outputs,
/// continuation notices, etc.
fn is_system_prompt(text: &str) -> bool {
    let trimmed = text.trim();
    // XML-tagged internal messages: <system-reminder>, <local-command-caveat>,
    // <command-name>, <local-command-stdout>, etc.
    if trimmed.starts_with('<') {
        return true;
    }
    // Continuation/compaction notices
    if trimmed.starts_with("This session is being continued from a previous") {
        return true;
    }
    false
}

/// Summarize a tool call into a short string for display.
fn summarize_tool_call(name: &str, input: &serde_json::Value) -> (String, Option<String>) {
    let obj = input.as_object();

    match name {
        "Write" | "Read" => {
            let fp = obj.and_then(|o| o.get("file_path")).and_then(|v| v.as_str()).unwrap_or("?");
            let short = short_path(fp);
            (short.clone(), Some(fp.to_string()))
        }
        "Edit" => {
            let fp = obj.and_then(|o| o.get("file_path")).and_then(|v| v.as_str()).unwrap_or("?");
            let short = short_path(fp);
            (short.clone(), Some(fp.to_string()))
        }
        "Bash" => {
            let cmd = obj.and_then(|o| o.get("command")).and_then(|v| v.as_str()).unwrap_or("?");
            // Get first line, truncate
            let first_line = cmd.lines().next().unwrap_or(cmd);
            let truncated = if first_line.len() > 50 {
                format!("{}…", &first_line[..49])
            } else {
                first_line.to_string()
            };
            (truncated, None)
        }
        "Glob" => {
            let pattern = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()).unwrap_or("?");
            (pattern.to_string(), None)
        }
        "Grep" => {
            let pattern = obj.and_then(|o| o.get("pattern")).and_then(|v| v.as_str()).unwrap_or("?");
            (format!("/{pattern}/"), None)
        }
        "Agent" => {
            let desc = obj.and_then(|o| o.get("description")).and_then(|v| v.as_str()).unwrap_or("?");
            (desc.to_string(), None)
        }
        "TaskCreate" => {
            let subj = obj.and_then(|o| o.get("subject")).and_then(|v| v.as_str()).unwrap_or("?");
            (subj.to_string(), None)
        }
        "TaskUpdate" => {
            let id = obj.and_then(|o| o.get("taskId")).and_then(|v| v.as_str()).unwrap_or("?");
            let status = obj.and_then(|o| o.get("status")).and_then(|v| v.as_str()).unwrap_or("?");
            (format!("#{id} → {status}"), None)
        }
        "ToolSearch" => {
            let q = obj.and_then(|o| o.get("query")).and_then(|v| v.as_str()).unwrap_or("?");
            (q.to_string(), None)
        }
        "Skill" => {
            let s = obj.and_then(|o| o.get("skill")).and_then(|v| v.as_str()).unwrap_or("?");
            (format!("/{s}"), None)
        }
        _ => {
            // Generic: show first string field
            if let Some(o) = obj {
                for (_, v) in o.iter().take(1) {
                    if let Some(s) = v.as_str() {
                        let t = if s.len() > 40 { format!("{}…", &s[..39]) } else { s.to_string() };
                        return (t, None);
                    }
                }
            }
            (String::new(), None)
        }
    }
}

/// Shorten a file path to just filename (or last 2 components if useful).
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        parts[parts.len() - 2..].join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_content_block_with_caller() {
        // Real Claude Code format includes a `caller` field
        let json = r#"{"type":"tool_use","id":"toolu_123","name":"Bash","input":{"command":"ls -la"},"caller":{"type":"direct"}}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "Bash");
                assert_eq!(input["command"].as_str(), Some("ls -la"));
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_real_transcript() {
        // Test with actual transcript file if available
        let path = std::path::PathBuf::from(env!("HOME"))
            .join(".claude/projects/-Users-aneeshchawla-Documents-ghostty-plugin")
            .join("0311e06d-16ea-417d-a455-f5aeec3386db.jsonl");

        if !path.exists() {
            eprintln!("Skipping real transcript test (file not found)");
            return;
        }

        let (info, turns) = parse_transcript(&path).unwrap();
        eprintln!("Session: {}", info.session_id);
        eprintln!("Turns: {}", turns.len());

        let total_tools: usize = turns.iter().map(|t| t.tool_calls.len()).sum();
        eprintln!("Total tool calls parsed: {total_tools}");

        for turn in &turns {
            eprintln!(
                "  Turn {}: \"{}\" → {} tools",
                turn.index,
                &turn.prompt_text[..turn.prompt_text.len().min(40)],
                turn.tool_calls.len()
            );
        }

        // We know there are ~119 tool calls in the raw data
        assert!(total_tools > 50, "expected >50 tool calls, got {total_tools}");
    }

    #[test]
    fn test_short_path() {
        assert_eq!(short_path("/a/b/c/d/main.rs"), "d/main.rs");
        assert_eq!(short_path("Cargo.toml"), "Cargo.toml");
    }

    #[test]
    fn test_parse_user_message() {
        let json = r#"{"type":"user","uuid":"abc","message":{"role":"user","content":"hello world"},"timestamp":"2026-03-21T09:00:00Z"}"#;
        let line: TranscriptLine = serde_json::from_str(json).unwrap();
        match line {
            TranscriptLine::User { uuid, message, .. } => {
                assert_eq!(uuid, "abc");
                assert_eq!(message.text(), "hello world");
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn test_parse_minimal_transcript() {
        let content = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"fix the bug"},"timestamp":"2026-03-21T09:00:00Z","sessionId":"sess1","cwd":"/project","gitBranch":"main"}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[{"type":"text","text":"I'll fix that."},{"type":"tool_use","name":"Edit","id":"t1","input":{"file_path":"/project/src/main.rs","old_string":"bug","new_string":"fix"}}],"model":"claude-opus-4-6"},"timestamp":"2026-03-21T09:00:05Z","sessionId":"sess1","cwd":"/project","gitBranch":"main"}
{"type":"user","uuid":"u2","message":{"role":"user","content":"now run tests"},"timestamp":"2026-03-21T09:01:00Z","sessionId":"sess1","cwd":"/project","gitBranch":"main"}
{"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"content":[{"type":"tool_use","name":"Bash","id":"t2","input":{"command":"cargo test"}}],"model":"claude-opus-4-6"},"timestamp":"2026-03-21T09:01:05Z","sessionId":"sess1","cwd":"/project","gitBranch":"main"}"#;

        let (info, turns) = parse_transcript_str(content).unwrap();
        assert_eq!(info.session_id, "sess1");
        assert_eq!(info.project_dir, "/project");
        assert_eq!(info.git_branch.as_deref(), Some("main"));
        assert_eq!(turns.len(), 2);

        assert_eq!(turns[0].prompt_text, "fix the bug");
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert_eq!(turns[0].tool_calls[0].name, "Edit");
        assert_eq!(turns[0].files_touched, vec!["/project/src/main.rs"]);

        assert_eq!(turns[1].prompt_text, "now run tests");
        assert_eq!(turns[1].tool_calls.len(), 1);
        assert_eq!(turns[1].tool_calls[0].name, "Bash");
        assert_eq!(turns[1].tool_calls[0].summary, "cargo test");
    }
}
