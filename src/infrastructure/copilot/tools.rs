use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use github_copilot_sdk::tool::ToolHandler;
use github_copilot_sdk::types::{Tool, ToolInvocation};
use github_copilot_sdk::{Error as SdkError, ToolResult};

use crate::infrastructure::providers::{CodeReviewProvider, DocumentationProvider};
use crate::types::{ExistingPrCommentThread, ReviewCommentType};

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

struct RequestDocumentationHandler {
    doc_provider: Option<Arc<dyn DocumentationProvider>>,
    provided_doc_ids: Mutex<HashSet<String>>,
    on_status: Arc<dyn Fn(String) + Send + Sync>,
}

impl RequestDocumentationHandler {
    fn emit_status(&self, msg: impl Into<String>) {
        (self.on_status)(msg.into());
    }

    async fn fetch_and_record(&self, raw_doc_id: &str) -> String {
        let doc_id = raw_doc_id.trim().to_string();
        if doc_id.is_empty() {
            let msg = "Agent requested documentation with an empty document ID.";
            self.emit_status(msg);
            return "Error: documentId is required.".to_string();
        }

        let Some(provider) = self.doc_provider.as_ref() else {
            let err_msg = "Documentation provider not configured. Cannot retrieve document.";
            self.emit_status(err_msg);
            return format!("Error: {err_msg}");
        };

        let already_provided = {
            let guard = self
                .provided_doc_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.contains(&doc_id)
        };
        if already_provided {
            let warning_msg = format!(
                "Document with ID {doc_id} has already been provided to you. Do not request it again. Use the information already in your context."
            );
            self.emit_status(format!(
                "Agent requested duplicate document ID: {doc_id} (declined)"
            ));
            return format!("Error: {warning_msg}");
        }

        match provider.fetch_page(&doc_id).await {
            Ok(page) => {
                {
                    let mut guard = self
                        .provided_doc_ids
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.insert(doc_id);
                    if !page.id.trim().is_empty() {
                        guard.insert(page.id.trim().to_string());
                    }
                }
                self.emit_status(format!("Agent viewed document: {}", page.title));
                format!("Title: {}\nID: {}\nContent:\n{}", page.title, page.id, page.body)
            }
            Err(err) => {
                let err_msg = format!("Failed to fetch document: {err}");
                self.emit_status(err_msg.clone());
                format!("Error: {err_msg}")
            }
        }
    }
}

#[async_trait]
impl ToolHandler for RequestDocumentationHandler {
    async fn call(&self, invocation: ToolInvocation) -> Result<ToolResult, SdkError> {
        let raw_id = invocation
            .arguments
            .get("documentId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let response_text = self.fetch_and_record(raw_id).await;
        Ok(ToolResult::Text(response_text))
    }
}

/// Creates the `request_documentation` custom tool with per-session document ID deduplication.
pub fn create_request_documentation_tool(
    doc_provider: Option<Arc<dyn DocumentationProvider>>,
    on_status: Arc<dyn Fn(String) + Send + Sync>,
) -> Tool {
    Tool::new("request_documentation")
        .with_description(
            "Request the content of a document by its ID (e.g. from Confluence) to read its content.",
        )
        .with_parameters(serde_json::json!({
            "type": "object",
            "properties": {
                "documentId": {
                    "type": "string",
                    "description": "The ID of the document to request."
                }
            },
            "required": ["documentId"]
        }))
        .with_skip_permission(true)
        .with_handler(Arc::new(RequestDocumentationHandler {
            doc_provider,
            provided_doc_ids: Mutex::new(HashSet::new()),
            on_status,
        }))
}

/// Filters existing PR comment threads by optional file path, line range, and keywords.
pub fn filter_existing_comment_threads(
    threads: &[ExistingPrCommentThread],
    file: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    keywords: Option<&str>,
) -> Vec<ExistingPrCommentThread> {
    let target_file = file
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|f| f.replace('\\', "/").trim_start_matches('/').to_ascii_lowercase());
    let query_terms: Vec<String> = keywords
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|kw| {
            kw.split_whitespace()
                .map(|t| t.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let has_keywords = !query_terms.is_empty();

    let effective_start = start_line.or(end_line);
    let effective_end = end_line.or(start_line);

    let mut matched: Vec<ExistingPrCommentThread> = if let Some(ref tf) = target_file {
        threads
            .iter()
            .filter(|t| {
                if t.comment_type != ReviewCommentType::Line {
                    return false;
                }
                let Some(ref raw_tf) = t.file else {
                    return false;
                };
                let thread_file = raw_tf
                    .replace('\\', "/")
                    .trim_start_matches('/')
                    .to_ascii_lowercase();
                if !thread_file.ends_with(tf.as_str()) && !tf.ends_with(thread_file.as_str()) {
                    return false;
                }

                if let (Some(start), Some(lr)) = (effective_start, t.line_range.as_ref()) {
                    let end = effective_end.unwrap_or(start);
                    let thread_start = lr.start_line.or(lr.end_line).unwrap_or(0);
                    let thread_end = lr.end_line.or(lr.start_line).unwrap_or(thread_start);
                    if thread_end < start || thread_start > end {
                        return false;
                    }
                }

                if has_keywords {
                    let content = t
                        .comments
                        .iter()
                        .map(|c| c.content.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_ascii_lowercase();
                    if !query_terms.iter().all(|term| content.contains(term)) {
                        return false;
                    }
                }

                true
            })
            .cloned()
            .collect()
    } else if has_keywords {
        threads
            .iter()
            .filter(|t| {
                if t.comment_type != ReviewCommentType::General {
                    return false;
                }
                let content = t
                    .comments
                    .iter()
                    .map(|c| c.content.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_lowercase();
                query_terms.iter().all(|term| content.contains(term))
            })
            .cloned()
            .collect()
    } else {
        threads.to_vec()
    };

    let mut seen_ids = HashSet::new();
    matched.retain(|t| seen_ids.insert(t.id));
    matched
}

/// Formats matching PR comment threads into the text block returned by `get_existing_comments`.
pub fn format_existing_comment_threads(results: &[ExistingPrCommentThread]) -> String {
    if results.is_empty() {
        return "No existing comment threads found matching the specified criteria.".to_string();
    }

    let formatted = results
        .iter()
        .map(|t| {
            let location_str = if t.comment_type == ReviewCommentType::Line {
                let mut loc = format!(
                    "Line Comment on {}",
                    t.file.as_deref().unwrap_or("unknown")
                );
                if let Some(start) = t.line_range.as_ref().and_then(|lr| lr.start_line) {
                    loc.push_str(&format!(":{start}"));
                    if let Some(end) = t
                        .line_range
                        .as_ref()
                        .and_then(|lr| lr.end_line)
                        .filter(|&e| e != start)
                    {
                        loc.push_str(&format!("-{end}"));
                    }
                }
                loc
            } else {
                "General PR Comment".to_string()
            };

            let status_label = if t.status.trim().is_empty() {
                "unknown"
            } else {
                t.status.trim()
            };
            let resolved_label = if t.is_resolved { "YES" } else { "NO" };
            let header = format!(
                "Thread #{} [{location_str}]\nStatus: {status_label} (Resolved: {resolved_label})",
                t.id
            );

            let comments_text = t
                .comments
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let author = if c.author.trim().is_empty() {
                        "Unknown"
                    } else {
                        c.author.trim()
                    };
                    let date = c
                        .published_date
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|d| format!(" at {d}"))
                        .unwrap_or_default();
                    let indented = c.content.replace('\n', "\n  ");
                    format!("  [Comment {} by {author}{date}]:\n  {indented}", i + 1)
                })
                .collect::<Vec<_>>()
                .join("\n\n");

            format!(
                "--- {header} ---\n{comments_text}\n--- End Thread #{} ---",
                t.id
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        "Found {} relevant comment thread(s) on the Pull Request:\n\n{formatted}",
        results.len()
    )
}

struct ViewExistingCommentsHandler {
    provider: Arc<dyn CodeReviewProvider>,
    pr_url_or_id: String,
    remote_url: Option<String>,
    cached_threads: tokio::sync::Mutex<Option<Vec<ExistingPrCommentThread>>>,
    on_status: Arc<dyn Fn(String) + Send + Sync>,
}

impl ViewExistingCommentsHandler {
    async fn query(
        &self,
        file: Option<&str>,
        start_line: Option<usize>,
        end_line: Option<usize>,
        keywords: Option<&str>,
    ) -> String {
        let mut guard = self.cached_threads.lock().await;
        if guard.is_none() {
            (self.on_status)("Critic fetching existing PR comments...".to_string());
            match self
                .provider
                .get_pr_comment_threads(&self.pr_url_or_id, self.remote_url.as_deref())
                .await
            {
                Ok(threads) => {
                    *guard = Some(threads);
                }
                Err(err) => {
                    let msg = format!("Failed to fetch existing comments: {err}");
                    (self.on_status)(msg.clone());
                    return format!("Error: {msg}");
                }
            }
        }

        let threads = guard.as_deref().unwrap_or(&[]);
        let filtered =
            filter_existing_comment_threads(threads, file, start_line, end_line, keywords);
        format_existing_comment_threads(&filtered)
    }
}

#[async_trait]
impl ToolHandler for ViewExistingCommentsHandler {
    async fn call(&self, invocation: ToolInvocation) -> Result<ToolResult, SdkError> {
        let file = invocation.arguments.get("file").and_then(|v| v.as_str());
        let keywords = invocation
            .arguments
            .get("keywords")
            .and_then(|v| v.as_str());
        let start_line = invocation
            .arguments
            .get("startLine")
            .and_then(|v| v.as_u64())
            .and_then(|n| usize::try_from(n).ok());
        let end_line = invocation
            .arguments
            .get("endLine")
            .and_then(|v| v.as_u64())
            .and_then(|n| usize::try_from(n).ok());

        let text = self.query(file, start_line, end_line, keywords).await;
        Ok(ToolResult::Text(text))
    }
}

/// Creates the `get_existing_comments` custom Copilot tool used by the AI Critic pass.
pub fn create_view_existing_comments_tool(
    provider: Arc<dyn CodeReviewProvider>,
    pr_url_or_id: String,
    remote_url: Option<String>,
    on_status: Arc<dyn Fn(String) + Send + Sync>,
) -> Tool {
    Tool::new("get_existing_comments")
        .with_description(
            "Fetch and search existing comment threads already posted on this Pull Request. File-based search can be either line-based (by specifying \"file\" and optional \"startLine\"/\"endLine\") or keyword-based (by specifying \"file\" and \"keywords\"). You can also search general PR comments by providing \"keywords\" without a \"file\". Use this tool to check if an issue has already been reported or resolved on the PR, and consider suggesting a reply to the existing thread instead of creating a duplicate comment.",
        )
        .with_parameters(serde_json::json!({
            "type": "object",
            "properties": {
                "keywords": {
                    "type": "string",
                    "description": "Keywords to search for in comment threads. When \"file\" is provided, searches comments on that file matching these keywords. When \"file\" is omitted, searches general PR comments."
                },
                "file": {
                    "type": "string",
                    "description": "File path to filter comments (e.g. \"src/auth/login.ts\"). Can be combined with line ranges (\"startLine\", \"endLine\") for line-based search, or with \"keywords\" for file-specific keyword search."
                },
                "startLine": {
                    "type": "number",
                    "description": "Optional start line number to filter line-based comments within this range."
                },
                "endLine": {
                    "type": "number",
                    "description": "Optional end line number to filter line-based comments within this range."
                }
            }
        }))
        .with_skip_permission(true)
        .with_handler(Arc::new(ViewExistingCommentsHandler {
            provider,
            pr_url_or_id,
            remote_url,
            cached_threads: tokio::sync::Mutex::new(None),
            on_status,
        }))
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

    #[tokio::test]
    async fn request_documentation_handler_fetches_deduplicates_and_handles_errors() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use crate::types::DocPageData;

        struct StubDocProvider {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl DocumentationProvider for StubDocProvider {
            async fn fetch_page(&self, page_id: &str) -> anyhow::Result<DocPageData> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                match page_id {
                    "101" => Ok(DocPageData {
                        id: "101".to_string(),
                        title: "Auth Design".to_string(),
                        body: "<p>Use PKCE</p>".to_string(),
                    }),
                    _ => anyhow::bail!("not found"),
                }
            }

            async fn search_pages(&self, _query: &str) -> anyhow::Result<Vec<DocPageData>> {
                Ok(Vec::new())
            }

            fn is_doc_page_url(&self, _url: &str) -> bool {
                false
            }

            fn extract_page_id(&self, _url: &str) -> Option<String> {
                None
            }
        }

        let statuses: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = Arc::clone(&statuses);
        let unconfigured = RequestDocumentationHandler {
            doc_provider: None,
            provided_doc_ids: Mutex::new(HashSet::new()),
            on_status: Arc::new(move |msg| {
                statuses_cb.lock().unwrap().push(msg);
            }),
        };
        let unconf_res = unconfigured.fetch_and_record("101").await;
        assert!(unconf_res.contains("Documentation provider not configured"));

        let provider = Arc::new(StubDocProvider {
            calls: AtomicUsize::new(0),
        });
        let statuses2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses2_cb = Arc::clone(&statuses2);
        let handler = RequestDocumentationHandler {
            doc_provider: Some(provider.clone()),
            provided_doc_ids: Mutex::new(HashSet::new()),
            on_status: Arc::new(move |msg| {
                statuses2_cb.lock().unwrap().push(msg);
            }),
        };

        // Failed fetch should not poison deduplication for that ID.
        let err_res = handler.fetch_and_record("999").await;
        assert!(err_res.starts_with("Error: Failed to fetch document:"));
        let err_res_retry = handler.fetch_and_record("999").await;
        assert!(err_res_retry.starts_with("Error: Failed to fetch document:"));
        assert_eq!(provider.calls.load(Ordering::Relaxed), 2);

        // First successful fetch succeeds and caches ID.
        let ok_res = handler.fetch_and_record("101").await;
        assert_eq!(
            ok_res,
            "Title: Auth Design\nID: 101\nContent:\n<p>Use PKCE</p>"
        );
        assert_eq!(provider.calls.load(Ordering::Relaxed), 3);

        // Duplicate request is declined without calling the provider again.
        let dup_res = handler.fetch_and_record("101").await;
        assert!(dup_res.contains("has already been provided to you"));
        assert_eq!(provider.calls.load(Ordering::Relaxed), 3);

        let recorded_statuses = statuses2.lock().unwrap();
        assert!(
            recorded_statuses
                .iter()
                .any(|s| s == "Agent viewed document: Auth Design")
        );
        assert!(
            recorded_statuses
                .iter()
                .any(|s| s == "Agent requested duplicate document ID: 101 (declined)")
        );
    }

    #[test]
    fn filter_and_format_existing_comment_threads_supports_file_lines_keywords_and_general() {
        use crate::types::{ExistingPrCommentLineRange, ExistingPrThreadComment};

        let threads = vec![
            ExistingPrCommentThread {
                id: 10,
                status: "fixed".to_string(),
                is_resolved: true,
                comment_type: ReviewCommentType::Line,
                file: Some("src/auth/login.rs".to_string()),
                line_range: Some(ExistingPrCommentLineRange {
                    start_line: Some(40),
                    end_line: Some(45),
                }),
                comments: vec![ExistingPrThreadComment {
                    id: Some(1),
                    author: "Alice".to_string(),
                    content: "SQL injection risk in query builder.".to_string(),
                    published_date: Some("2026-05-01T10:00:00Z".to_string()),
                }],
            },
            ExistingPrCommentThread {
                id: 11,
                status: "active".to_string(),
                is_resolved: false,
                comment_type: ReviewCommentType::Line,
                file: Some("src/auth/login.rs".to_string()),
                line_range: Some(ExistingPrCommentLineRange {
                    start_line: Some(100),
                    end_line: Some(100),
                }),
                comments: vec![ExistingPrThreadComment {
                    id: Some(2),
                    author: "Bob".to_string(),
                    content: "Nit: rename variable.".to_string(),
                    published_date: None,
                }],
            },
            ExistingPrCommentThread {
                id: 12,
                status: "active".to_string(),
                is_resolved: false,
                comment_type: ReviewCommentType::General,
                file: None,
                line_range: None,
                comments: vec![ExistingPrThreadComment {
                    id: Some(3),
                    author: "Carol".to_string(),
                    content: "Please update architecture docs.".to_string(),
                    published_date: None,
                }],
            },
        ];

        // File + overlapping line range
        let by_line = filter_existing_comment_threads(
            &threads,
            Some("/src/auth/login.rs"),
            Some(38),
            Some(42),
            None,
        );
        assert_eq!(by_line.len(), 1);
        assert_eq!(by_line[0].id, 10);

        // File + keywords
        let by_file_kw = filter_existing_comment_threads(
            &threads,
            Some("login.rs"),
            None,
            None,
            Some("rename variable"),
        );
        assert_eq!(by_file_kw.len(), 1);
        assert_eq!(by_file_kw[0].id, 11);

        // General keywords (no file)
        let by_general_kw =
            filter_existing_comment_threads(&threads, None, None, None, Some("architecture docs"));
        assert_eq!(by_general_kw.len(), 1);
        assert_eq!(by_general_kw[0].id, 12);

        let formatted = format_existing_comment_threads(&by_line);
        assert!(formatted.contains("Found 1 relevant comment thread(s)"));
        assert!(formatted.contains("Thread #10 [Line Comment on src/auth/login.rs:40-45]"));
        assert!(formatted.contains("Status: fixed (Resolved: YES)"));
        assert!(formatted.contains("[Comment 1 by Alice at 2026-05-01T10:00:00Z]:"));
        assert!(formatted.contains("SQL injection risk in query builder."));

        assert_eq!(
            format_existing_comment_threads(&[]),
            "No existing comment threads found matching the specified criteria."
        );
    }

    #[tokio::test]
    async fn view_existing_comments_handler_caches_provider_fetch_and_handles_errors() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use crate::types::{
            ExistingPrThreadComment, ParsedPrUrl, ParsedRemoteUrl, PostPrCommentRequest,
            PrMetadata, PrSearchType,
        };

        struct StubCodeProvider {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl CodeReviewProvider for StubCodeProvider {
            fn parse_pr_url(&self, _url: &str) -> Option<ParsedPrUrl> {
                None
            }
            fn parse_remote_url(&self, _url: &str) -> Option<ParsedRemoteUrl> {
                None
            }
            async fn get_pr_details(
                &self,
                _pr_url_or_id: &str,
                _remote_url: Option<&str>,
            ) -> anyhow::Result<PrMetadata> {
                anyhow::bail!("unused")
            }
            async fn get_project_prs(
                &self,
                _search_type: PrSearchType,
            ) -> anyhow::Result<Vec<PrMetadata>> {
                Ok(Vec::new())
            }
            async fn get_linked_tickets(
                &self,
                _pr_id: &str,
                _repository_id: Option<&str>,
            ) -> anyhow::Result<Vec<String>> {
                Ok(Vec::new())
            }
            async fn get_pr_comment_threads(
                &self,
                _pr_url_or_id: &str,
                _remote_url: Option<&str>,
            ) -> anyhow::Result<Vec<ExistingPrCommentThread>> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![ExistingPrCommentThread {
                    id: 77,
                    status: "active".to_string(),
                    is_resolved: false,
                    comment_type: ReviewCommentType::General,
                    file: None,
                    line_range: None,
                    comments: vec![ExistingPrThreadComment {
                        id: Some(1),
                        author: "Reviewer".to_string(),
                        content: "Check retry backoff.".to_string(),
                        published_date: None,
                    }],
                }])
            }
            async fn post_pr_comment(
                &self,
                _pr_url_or_id: &str,
                _comment: &PostPrCommentRequest,
                _remote_url: Option<&str>,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let provider = Arc::new(StubCodeProvider {
            calls: AtomicUsize::new(0),
        });
        let statuses: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses_cb = Arc::clone(&statuses);
        let handler = ViewExistingCommentsHandler {
            provider: provider.clone(),
            pr_url_or_id: "42".to_string(),
            remote_url: None,
            cached_threads: tokio::sync::Mutex::new(None),
            on_status: Arc::new(move |s| statuses_cb.lock().unwrap().push(s)),
        };

        let first = handler.query(None, None, None, Some("retry")).await;
        assert!(first.contains("Thread #77"));
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);

        // Second query uses cached threads without calling provider again.
        let second = handler.query(None, None, None, Some("nonexistent")).await;
        assert_eq!(
            second,
            "No existing comment threads found matching the specified criteria."
        );
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            statuses.lock().unwrap().as_slice(),
            &["Critic fetching existing PR comments...".to_string()]
        );
    }
}
