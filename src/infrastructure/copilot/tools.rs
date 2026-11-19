use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use github_copilot_sdk::tool::ToolHandler;
use github_copilot_sdk::types::{Tool, ToolInvocation};
use github_copilot_sdk::{Error as SdkError, ToolResult};

/// Extracts complete JSON objects from a streaming text buffer, discarding non-JSON prefix noise
/// and returning any trailing incomplete fragment for future chunks.
pub fn parse_resilient_jsonl<F>(buffer: &str, mut on_line: F) -> String
where
    F: FnMut(&str),
{
    let mut temp = buffer.to_string();
    loop {
        let Some(start_idx) = temp.find('{') else {
            return String::new();
        };
        if start_idx > 0 {
            temp.drain(..start_idx);
        }

        let mut matched_end = None;
        for (idx, ch) in temp.char_indices().skip(1) {
            if ch == '}' {
                let candidate = &temp[..=idx];
                if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                    on_line(candidate);
                    matched_end = Some(idx + 1);
                    break;
                }
            }
        }

        if let Some(next_start) = matched_end {
            temp.drain(..next_start);
        } else {
            break;
        }
    }
    temp
}

struct ReportIntentHandler;

#[async_trait]
impl ToolHandler for ReportIntentHandler {
    async fn call(&self, invocation: ToolInvocation) -> Result<ToolResult, SdkError> {
        let intent = invocation
            .arguments
            .get("intent")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        Ok(ToolResult::Text(format!("Status updated: \"{intent}\"")))
    }
}

/// Creates the `report_intent` custom tool used by Copilot agents to stream live progress.
pub fn create_report_intent_tool() -> Tool {
    Tool::new("report_intent")
        .with_description("Report the current intent, status, or progress of the agent to the user.")
        .with_parameters(serde_json::json!({
            "type": "object",
            "properties": {
                "intent": {
                    "type": "string",
                    "description": "The status or intent message to report to the user."
                }
            },
            "required": ["intent"]
        }))
        .with_skip_permission(true)
        .with_handler(Arc::new(ReportIntentHandler))
}

fn split_shell_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let bytes = command.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && !in_single && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            let sep_len = if i + 1 < bytes.len()
                && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
            {
                2
            } else if matches!(b, b';' | b'|' | b'\n') {
                1
            } else {
                0
            };

            if sep_len > 0 {
                let seg = command[start..i].trim();
                if !seg.is_empty() {
                    segments.push(seg);
                }
                i += sep_len;
                start = i;
                continue;
            }
        }
        i += 1;
    }

    let tail = command[start..].trim();
    if !tail.is_empty() {
        segments.push(tail);
    }
    segments
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn extract_executable_names(command: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();

    for segment in split_shell_segments(command) {
        let exec = segment
            .split_whitespace()
            .find(|tok| !is_env_assignment(tok));

        if let Some(raw_tok) = exec {
            let unquoted = raw_tok.trim_matches(|c| c == '"' || c == '\'');
            let base = Path::new(unquoted)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty())
                .unwrap_or(unquoted);
            if !base.is_empty() && seen.insert(base.to_string()) {
                names.push(base.to_string());
            }
        }
    }

    names
}

/// Formats a concise human-readable status message when Copilot starts executing a tool.
pub fn format_tool_start(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    match tool_name {
        "report_intent" => arguments
            .and_then(|a| a.get("intent"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Working...")
            .to_string(),
        "grep" => {
            let pattern = arguments
                .and_then(|a| a.get("pattern").or_else(|| a.get("query")))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match pattern {
                Some(p) => format!("grep ({p})"),
                None => "grep".to_string(),
            }
        }
        "view" => {
            let file_path = arguments
                .and_then(|a| a.get("path").or_else(|| a.get("file")))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match file_path {
                Some(p) => {
                    let display = Path::new(p)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .filter(|n| !n.is_empty())
                        .unwrap_or(p);
                    format!("view ({display})")
                }
                None => "view".to_string(),
            }
        }
        "bash" | "powershell" => {
            let cmd = arguments
                .and_then(|a| a.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let names = extract_executable_names(cmd);
            if names.is_empty() {
                tool_name.to_string()
            } else {
                format!("{tool_name} ({})", names.join(", "))
            }
        }
        "get_existing_comments" => {
            let mut parts = Vec::new();
            if let Some(file) = arguments
                .and_then(|a| a.get("file"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let name = Path::new(file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(file);
                parts.push(name.to_string());
            }
            if arguments
                .and_then(|a| a.get("keywords"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.trim().is_empty())
            {
                parts.push("keywords".to_string());
            }
            if parts.is_empty() {
                "get_existing_comments".to_string()
            } else {
                format!("get_existing_comments ({})", parts.join(", "))
            }
        }
        other => other.to_string(),
    }
}

/// Formats an error status line if a tool execution fails; returns `None` on success.
pub fn format_tool_complete(tool_name: &str, success: bool, error: Option<&str>) -> Option<String> {
    if success {
        None
    } else {
        let msg = error
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown error");
        Some(format!("Tool failed: {tool_name} - {msg}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resilient_jsonl_discards_prefix_and_handles_split_and_multiple_objects() {
        let mut lines = Vec::new();

        // Prefix noise + two complete objects + partial trailing object
        let chunk1 = "Sure, here is the output:\n{\"a\":1}{\"b\":{\"nested\":true},\"text\":\"hi } there\"}\n{\"partial\":";
        let rem1 = parse_resilient_jsonl(chunk1, |line| lines.push(line.to_string()));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"a\":1}");
        assert_eq!(lines[1], "{\"b\":{\"nested\":true},\"text\":\"hi } there\"}");
        assert_eq!(rem1, "{\"partial\":");

        let chunk2 = format!("{rem1}\"done\"}} trailing noise");
        let rem2 = parse_resilient_jsonl(&chunk2, |line| lines.push(line.to_string()));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "{\"partial\":\"done\"}");
        assert_eq!(rem2, "");
    }

    #[test]
    fn format_tool_start_formats_known_tools() {
        assert_eq!(
            format_tool_start(
                "report_intent",
                Some(&serde_json::json!({ "intent": "Reading Cargo.toml" }))
            ),
            "Reading Cargo.toml"
        );
        assert_eq!(
            format_tool_start("grep", Some(&serde_json::json!({ "pattern": "AppState" }))),
            "grep (AppState)"
        );
        assert_eq!(
            format_tool_start(
                "view",
                Some(&serde_json::json!({ "path": "src/ui/shell.rs" }))
            ),
            "view (shell.rs)"
        );
        assert_eq!(
            format_tool_start(
                "bash",
                Some(&serde_json::json!({ "command": "FOO=1 git status && rg \"a | b\" src | head -n 5" }))
            ),
            "bash (git, rg, head)"
        );
        assert_eq!(
            format_tool_start(
                "powershell",
                Some(&serde_json::json!({ "command": "Get-ChildItem; Select-String foo" }))
            ),
            "powershell (Get-ChildItem, Select-String)"
        );
        assert_eq!(format_tool_start("glob", None), "glob");
    }

    #[test]
    fn format_tool_complete_handles_success_and_failure() {
        assert_eq!(format_tool_complete("view", true, None), None);
        assert_eq!(
            format_tool_complete("view", false, Some("permission denied")),
            Some("Tool failed: view - permission denied".to_string())
        );
    }
}
