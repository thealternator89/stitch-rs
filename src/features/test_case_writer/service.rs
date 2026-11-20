use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::features::test_case_writer::prompts::build_test_case_prompt;
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, resolve_model_override,
};
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage, TestCase, TicketData};

#[derive(Debug, Clone)]
pub struct TestCaseGenerationRequest {
    pub ticket_data: TicketData,
    pub additional_context: String,
    pub model_override: String,
    pub settings: AppSettings,
}

#[derive(Debug, Clone)]
pub enum TestCaseGenerationStreamEvent {
    TestCase(TestCase),
    Completed {
        count: usize,
        usage: CopilotUsage,
    },
    TimedOut {
        timeout_secs: u64,
    },
    Cancelled,
    Error(String),
}

pub struct ActiveTestCaseGenerationHandle {
    cancel_tx: watch::Sender<bool>,
}

impl ActiveTestCaseGenerationHandle {
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }
}

/// Parses a single JSONL line into a validated `TestCase` record.
pub fn parse_test_case_jsonl_line(line: &str) -> Option<TestCase> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }

    let mut tc: TestCase = serde_json::from_str(trimmed).ok()?;
    tc.description = tc.description.trim().to_string();
    if tc.description.is_empty() {
        return None;
    }
    tc.pre_conditions = tc.pre_conditions.trim().to_string();
    tc.steps = tc.steps.trim().to_string();
    tc.expected_result = tc.expected_result.trim().to_string();
    tc.priority = tc.priority.trim().to_string();
    if tc.priority.is_empty() {
        tc.priority = "Medium".to_string();
    }
    tc.id = String::new();
    tc.deleted = false;

    Some(tc)
}

fn sanitize_table_cell(raw: &str, newline_replacement: &str) -> String {
    raw.replace("\r\n", "\n")
        .replace('|', "\\|")
        .replace('\n', newline_replacement)
        .trim()
        .to_string()
}

/// Serializes active (non-deleted) test cases as a GFM Markdown table numbered `TC-1..N`.
pub fn convert_to_markdown_table(test_cases: &[TestCase]) -> String {
    let active: Vec<&TestCase> = test_cases.iter().filter(|tc| !tc.deleted).collect();
    if active.is_empty() {
        return String::new();
    }

    let mut lines = Vec::with_capacity(active.len() + 2);
    lines.push(
        "| Test Case ID | Description | Pre-conditions | Steps | Expected Result | Priority |"
            .to_string(),
    );
    lines.push("| --- | --- | --- | --- | --- | --- |".to_string());

    for (idx, tc) in active.iter().enumerate() {
        let id = format!("TC-{}", idx + 1);
        let desc = sanitize_table_cell(&tc.description, " ");
        let pre = sanitize_table_cell(&tc.pre_conditions, "<br>");
        let steps = sanitize_table_cell(&tc.steps, "<br>");
        let expected = sanitize_table_cell(&tc.expected_result, "<br>");
        let priority = sanitize_table_cell(&tc.priority, " ");
        lines.push(format!(
            "| {id} | {desc} | {pre} | {steps} | {expected} | {priority} |"
        ));
    }

    lines.join("\n")
}

/// Wraps a serialized Markdown test case table for publishing as a comment or task body.
pub fn format_test_cases_payload(markdown_table: &str) -> String {
    format!("Test Cases:\n\n{markdown_table}")
}

/// Moves an element from `from` to `to`, shifting intermediate rows cleanly.
pub fn reorder_items<T>(items: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from >= items.len() || to >= items.len() || from == to {
        return false;
    }
    let item = items.remove(from);
    items.insert(to, item);
    true
}

/// Streams generated `TestCase` objects from Copilot onto a Tokio task and forwards them to the UI.
pub fn generate_test_cases(
    copilot: Arc<CopilotService>,
    req: TestCaseGenerationRequest,
) -> (
    ActiveTestCaseGenerationHandle,
    mpsc::UnboundedReceiver<TestCaseGenerationStreamEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);

    runtime().spawn(async move {
        let terminal = run_test_case_generation_task(copilot, req, cancel_rx, &tx).await;
        let _ = tx.send(terminal);
    });

    (ActiveTestCaseGenerationHandle { cancel_tx }, rx)
}

async fn run_test_case_generation_task(
    copilot: Arc<CopilotService>,
    req: TestCaseGenerationRequest,
    cancel_rx: watch::Receiver<bool>,
    tx: &mpsc::UnboundedSender<TestCaseGenerationStreamEvent>,
) -> TestCaseGenerationStreamEvent {
    if *cancel_rx.borrow() {
        return TestCaseGenerationStreamEvent::Cancelled;
    }

    let effective_model = resolve_model_override(&req.model_override, &req.settings.copilot_model);

    let session = match copilot
        .create_session(SessionOptions {
            token: req.settings.copilot_token.clone(),
            model: Some(effective_model),
            working_directory: None,
            tools: Vec::new(),
            available_tools: Some(Vec::new()),
            streaming: true,
            label: "Test Case Writer".to_string(),
        })
        .await
    {
        Ok(session) => session,
        Err(err) => return TestCaseGenerationStreamEvent::Error(err.to_string()),
    };

    let prompt = build_test_case_prompt(
        req.ticket_data.id.as_deref().unwrap_or(""),
        &req.ticket_data.title,
        &req.ticket_data.description,
        req.ticket_data.acceptance_criteria.as_deref(),
        &req.additional_context,
        &req.settings.prompts.test_case_writer,
    );

    let mut count = 0usize;
    let stream_res = CopilotService::send_and_collect_stream(&session, &prompt, cancel_rx, |ev| {
        if let StreamEvent::JsonLine(line) = ev {
            if let Some(tc) = parse_test_case_jsonl_line(&line) {
                count += 1;
                let _ = tx.send(TestCaseGenerationStreamEvent::TestCase(tc));
            }
        }
    })
    .await;

    let _ = session.disconnect().await;

    match stream_res {
        Ok(usage) => TestCaseGenerationStreamEvent::Completed { count, usage },
        Err(StreamError::Timeout(dur)) => TestCaseGenerationStreamEvent::TimedOut {
            timeout_secs: dur.as_secs().max(1),
        },
        Err(StreamError::Cancelled) => TestCaseGenerationStreamEvent::Cancelled,
        Err(StreamError::Other(err)) => TestCaseGenerationStreamEvent::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_test_case_jsonl_line_parses_valid_objects_and_skips_noise() {
        let valid = r#"{"description": "  Verify login  ", "preConditions": "* Account exists", "steps": "1. Open login\n2. Submit", "expectedResult": "Redirected home", "priority": "High"}"#;
        let tc = parse_test_case_jsonl_line(valid).expect("valid test case");
        assert_eq!(tc.description, "Verify login");
        assert_eq!(tc.pre_conditions, "* Account exists");
        assert_eq!(tc.steps, "1. Open login\n2. Submit");
        assert_eq!(tc.expected_result, "Redirected home");
        assert_eq!(tc.priority, "High");
        assert!(!tc.deleted);

        let missing_priority = r#"{"description": "Edge check", "preConditions": "", "steps": "1. Go", "expectedResult": "Ok", "priority": "  "}"#;
        let tc_default_prio = parse_test_case_jsonl_line(missing_priority).expect("default priority");
        assert_eq!(tc_default_prio.priority, "Medium");

        assert!(parse_test_case_jsonl_line("```json").is_none());
        assert!(parse_test_case_jsonl_line("   ").is_none());
        assert!(
            parse_test_case_jsonl_line(r#"{"description": "   ", "steps": "1. Go"}"#).is_none()
        );
        assert!(parse_test_case_jsonl_line("not json").is_none());
    }

    #[test]
    fn convert_to_markdown_table_numbers_active_rows_escapes_pipes_and_newlines() {
        let cases = vec![
            TestCase {
                id: "TC-99".to_string(),
                description: "Line one\nLine two | A".to_string(),
                pre_conditions: "* Pre 1\n* Pre 2".to_string(),
                steps: "1. Enter a|b\n2. Click Save".to_string(),
                expected_result: "Saved\nToast visible".to_string(),
                priority: "High".to_string(),
                deleted: false,
            },
            TestCase {
                id: "TC-2".to_string(),
                description: "Deleted scenario".to_string(),
                pre_conditions: String::new(),
                steps: String::new(),
                expected_result: String::new(),
                priority: "Low".to_string(),
                deleted: true,
            },
            TestCase {
                id: "TC-3".to_string(),
                description: "Second active".to_string(),
                pre_conditions: "None".to_string(),
                steps: "1. Run".to_string(),
                expected_result: "Passes".to_string(),
                priority: "Medium".to_string(),
                deleted: false,
            },
        ];

        let md = convert_to_markdown_table(&cases);
        let expected = "\
| Test Case ID | Description | Pre-conditions | Steps | Expected Result | Priority |\n\
| --- | --- | --- | --- | --- | --- |\n\
| TC-1 | Line one Line two \\| A | * Pre 1<br>* Pre 2 | 1. Enter a\\|b<br>2. Click Save | Saved<br>Toast visible | High |\n\
| TC-2 | Second active | None | 1. Run | Passes | Medium |";
        assert_eq!(md, expected);
        assert_eq!(
            format_test_cases_payload(&md),
            format!("Test Cases:\n\n{expected}")
        );
    }

    #[test]
    fn convert_to_markdown_table_returns_empty_when_no_active_rows() {
        let deleted_only = vec![TestCase {
            description: "Removed".to_string(),
            deleted: true,
            ..Default::default()
        }];
        assert_eq!(convert_to_markdown_table(&[]), "");
        assert_eq!(convert_to_markdown_table(&deleted_only), "");
    }

    #[test]
    fn reorder_items_moves_rows_in_both_directions_and_ignores_noop() {
        let mut items = vec!["a", "b", "c", "d"];
        assert!(reorder_items(&mut items, 0, 2));
        assert_eq!(items, vec!["b", "c", "a", "d"]);

        assert!(reorder_items(&mut items, 3, 1));
        assert_eq!(items, vec!["b", "d", "c", "a"]);

        assert!(!reorder_items(&mut items, 1, 1));
        assert!(!reorder_items(&mut items, 9, 0));
        assert!(!reorder_items(&mut items, 0, 9));
    }
}
