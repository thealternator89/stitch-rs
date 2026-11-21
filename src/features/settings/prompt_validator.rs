use std::sync::Arc;

use tokio::sync::watch;

use crate::features::story_writer::prompts::build_story_prompt;
use crate::features::test_case_writer::prompts::build_test_case_prompt;
use crate::infrastructure::copilot::{CopilotService, SessionOptions, StreamError};
use crate::types::{AppSettings, StoryWriterPrompts, TestCaseWriterPrompts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationVerdict {
    Pass,
    Fail,
    Neutral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptValidationResult {
    pub raw_response: String,
    pub verdict: ValidationVerdict,
    pub markdown_body: String,
}

pub fn parse_validation_response(raw: &str) -> PromptValidationResult {
    let trimmed = raw.trim();
    let upper = trimmed.to_ascii_uppercase();

    if upper.starts_with("PASS:") {
        let body = trimmed[5..].trim_start().to_string();
        PromptValidationResult {
            raw_response: trimmed.to_string(),
            verdict: ValidationVerdict::Pass,
            markdown_body: body,
        }
    } else if upper.starts_with("FAIL:") {
        let body = trimmed[5..].trim_start().to_string();
        PromptValidationResult {
            raw_response: trimmed.to_string(),
            verdict: ValidationVerdict::Fail,
            markdown_body: body,
        }
    } else {
        PromptValidationResult {
            raw_response: trimmed.to_string(),
            verdict: ValidationVerdict::Neutral,
            markdown_body: trimmed.to_string(),
        }
    }
}

pub fn build_prompt_complexity_check_prompt(
    prompt_to_check: &str,
    custom_inputs: &[(&str, &str)],
) -> String {
    let active_inputs: Vec<String> = custom_inputs
        .iter()
        .filter_map(|(key, val)| {
            let trimmed = val.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("[Customized Field: {key}]\n{trimmed}"))
            }
        })
        .collect();

    let active_inputs_list = if active_inputs.is_empty() {
        "No custom prompt statements entered.".to_string()
    } else {
        active_inputs.join("\n\n")
    };

    let prompt_trimmed = prompt_to_check.trim();

    format!(
        r#"Evaluate whether the user-customized statements in `<custom_statements>` are likely to confuse the model or break the JSON Lines (JSONL) output contract of `<prompt_template>`.

## Evaluation Scope
- Evaluate only `<custom_statements>`; do not flag or invent issues for the fixed default parts of `<prompt_template>`.
- Focus on concrete, high-probability risks to JSONL validity or schema compliance (such as encouraging markdown code fences around output, unescaped double quotes, raw newlines inside JSON string values, or instructions that contradict the required keys).
- Be lenient on style preferences and low-risk phrasing; do not invent issues when the custom statements are clear and safe.

## Output Format
Format your feedback as brief, clear Markdown, starting with one of:
- `PASS: ` followed by a short confirmation if the custom statements are safe and compatible with the JSONL contract.
- `FAIL: ` followed by a concise list of issues only if there are significant, high-probability risks to JSONL validity or schema compliance.
- No prefix (begin directly with your Markdown feedback) if there are only mild concerns or you are uncertain.

Example PASS response:
PASS: The custom prompt statements are clear and compatible with the JSONL format.

Example FAIL response:
FAIL: The customized statements introduce JSONL formatting risks.
- **general**: Avoid requesting raw newlines or fenced code blocks, as they can break JSONL parsing.

<prompt_template>
{prompt_trimmed}
</prompt_template>

<custom_statements>
{active_inputs_list}
</custom_statements>"#
    )
}

const NO_CUSTOM_PROMPT_PASS: &str =
    "PASS: No customized prompt statements detected. Please customize at least one field before checking.";

async fn run_prompt_complexity_check(
    copilot: Arc<CopilotService>,
    prompt_to_check: &str,
    custom_inputs: &[(&str, &str)],
    settings: &AppSettings,
) -> anyhow::Result<PromptValidationResult> {
    let meta_prompt = build_prompt_complexity_check_prompt(prompt_to_check, custom_inputs);

    let session = copilot
        .create_session(SessionOptions {
            token: settings.copilot_token.clone(),
            model: Some("auto".to_string()),
            working_directory: None,
            tools: Vec::new(),
            available_tools: Some(Vec::new()),
            streaming: false,
            label: "Prompt Complexity".to_string(),
        })
        .await?;

    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let collect_res = CopilotService::send_and_collect_text(&session, &meta_prompt, cancel_rx).await;
    let _ = session.disconnect().await;

    match collect_res {
        Ok((text, _usage)) => Ok(parse_validation_response(&text)),
        Err(StreamError::Timeout(dur)) => anyhow::bail!(
            "Prompt validation timed out after {} seconds without a response from Copilot.",
            dur.as_secs().max(1)
        ),
        Err(StreamError::Cancelled) => anyhow::bail!("Prompt validation was cancelled."),
        Err(StreamError::Other(err)) => Err(err),
    }
}

pub async fn check_story_writer_prompt_complexity(
    copilot: Arc<CopilotService>,
    prompts: &StoryWriterPrompts,
    settings: &AppSettings,
) -> anyhow::Result<PromptValidationResult> {
    let custom_inputs = [
        ("general", prompts.general.as_str()),
        ("title", prompts.title.as_str()),
        ("description", prompts.description.as_str()),
        ("acceptanceCriteria", prompts.acceptance_criteria.as_str()),
        ("notes", prompts.notes.as_str()),
    ];

    if !custom_inputs.iter().any(|(_, val)| !val.trim().is_empty()) {
        return Ok(parse_validation_response(NO_CUSTOM_PROMPT_PASS));
    }

    let prompt_to_check = build_story_prompt(
        "[Page Title Placeholder]",
        "[Page Content Placeholder]",
        "[Additional Context Placeholder]",
        prompts,
    );
    run_prompt_complexity_check(copilot, &prompt_to_check, &custom_inputs, settings).await
}

pub async fn check_test_case_writer_prompt_complexity(
    copilot: Arc<CopilotService>,
    prompts: &TestCaseWriterPrompts,
    settings: &AppSettings,
) -> anyhow::Result<PromptValidationResult> {
    let custom_inputs = [
        ("general", prompts.general.as_str()),
        ("description", prompts.description.as_str()),
        ("preConditions", prompts.pre_conditions.as_str()),
        ("steps", prompts.steps.as_str()),
        ("expectedResult", prompts.expected_result.as_str()),
    ];

    if !custom_inputs.iter().any(|(_, val)| !val.trim().is_empty()) {
        return Ok(parse_validation_response(NO_CUSTOM_PROMPT_PASS));
    }

    let prompt_to_check = build_test_case_prompt(
        "[Ticket ID Placeholder]",
        "[Title Placeholder]",
        "[Description Placeholder]",
        Some("[Acceptance Criteria Placeholder]"),
        "[Additional Context Placeholder]",
        prompts,
    );
    run_prompt_complexity_check(copilot, &prompt_to_check, &custom_inputs, settings).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_validation_response_detects_pass_fail_and_neutral() {
        let pass = parse_validation_response("  pass: Looks great!\n\n- Clear rules  ");
        assert_eq!(pass.verdict, ValidationVerdict::Pass);
        assert_eq!(pass.markdown_body, "Looks great!\n\n- Clear rules");

        let fail = parse_validation_response("FAIL: Formatting risks found.");
        assert_eq!(fail.verdict, ValidationVerdict::Fail);
        assert_eq!(fail.markdown_body, "Formatting risks found.");

        let neutral = parse_validation_response("Consider clarifying bullet style.");
        assert_eq!(neutral.verdict, ValidationVerdict::Neutral);
        assert_eq!(neutral.markdown_body, "Consider clarifying bullet style.");
    }

    #[test]
    fn build_prompt_complexity_check_prompt_lists_only_non_empty_custom_fields() {
        let inputs = [
            ("general", "  Keep stories concise.  "),
            ("title", "   "),
            ("notes", "Include risks"),
        ];
        let meta = build_prompt_complexity_check_prompt("TEMPLATE", &inputs);

        assert!(meta.contains("<prompt_template>\nTEMPLATE\n</prompt_template>"));
        assert!(meta.contains("<custom_statements>"));
        assert!(meta.contains("[Customized Field: general]\nKeep stories concise."));
        assert!(!meta.contains("[Customized Field: title]"));
        assert!(meta.contains("[Customized Field: notes]\nInclude risks"));
        assert!(!meta.contains("You are "));

        let empty_meta = build_prompt_complexity_check_prompt("TEMPLATE", &[("general", " ")]);
        assert!(empty_meta.contains("<custom_statements>\nNo custom prompt statements entered.\n</custom_statements>"));
    }
}
