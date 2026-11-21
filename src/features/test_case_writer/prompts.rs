use crate::types::TestCaseWriterPrompts;

pub const DEFAULT_TC_DESCRIPTION_PROMPT: &str = "Brief description of the test scenario";
pub const DEFAULT_TC_PRECONDITIONS_PROMPT: &str = "Any preconditions required before running the test";
pub const DEFAULT_TC_STEPS_PROMPT: &str =
    "Bullet-pointed or numbered steps to execute the test";
pub const DEFAULT_TC_EXPECTED_RESULT_PROMPT: &str = "The expected result";
pub const SYSTEM_TC_PRIORITY_PROMPT: &str = r#"Priority of the test (e.g., "High", "Medium", "Low")"#;

fn non_empty_or_default<'a>(custom: &'a str, default: &'a str) -> &'a str {
    let trimmed = custom.trim();
    if trimmed.is_empty() { default } else { trimmed }
}

pub fn build_test_case_prompt(
    ticket_id: &str,
    ticket_title: &str,
    ticket_description: &str,
    ticket_acceptance_criteria: Option<&str>,
    additional_context: &str,
    prompts: &TestCaseWriterPrompts,
) -> String {
    let general = prompts.general.trim();
    let general_block = if general.is_empty() {
        String::new()
    } else {
        format!("\n<custom_instructions>\n{general}\n</custom_instructions>\n")
    };

    let description_prompt =
        non_empty_or_default(&prompts.description, DEFAULT_TC_DESCRIPTION_PROMPT);
    let pre_conditions_prompt =
        non_empty_or_default(&prompts.pre_conditions, DEFAULT_TC_PRECONDITIONS_PROMPT);
    let steps_prompt = non_empty_or_default(&prompts.steps, DEFAULT_TC_STEPS_PROMPT);
    let expected_result_prompt =
        non_empty_or_default(&prompts.expected_result, DEFAULT_TC_EXPECTED_RESULT_PROMPT);

    let ac_str = ticket_acceptance_criteria
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("N/A");

    let context_str = {
        let trimmed = additional_context.trim();
        if trimmed.is_empty() {
            "None provided"
        } else {
            trimmed
        }
    };

    format!(
        r#"Generate comprehensive test cases for the user story/ticket below.
{general_block}
<ticket>
ID: {ticket_id}
Title: {ticket_title}
Description: {ticket_description}
Acceptance Criteria: {ac_str}
</ticket>

<additional_context>
{context_str}
</additional_context>

## Output Format
Output the test cases directly in your response as JSON Lines (JSONL), where each line is a single, standalone, valid JSON object. Do not wrap the objects in a JSON array, create any files, use markdown code fences, or include any surrounding prose.

Each JSON object must contain exactly these keys (do not include an `"id"` key):
- "description": (string) {description_prompt}
- "preConditions": (string) {pre_conditions_prompt}
- "steps": (string) {steps_prompt}
- "expectedResult": (string) {expected_result_prompt}
- "priority": (string) {SYSTEM_TC_PRIORITY_PROMPT}

Because field values are rendered inside a Markdown table, keep values as concise plain text (simple `-` bullets or `1.` numbered steps separated by `\n` are fine; avoid headings, fenced code blocks, or tables). Inside JSON string values, escape newlines as `\n` (no raw literal newlines) and double quotes as `\"`.

Example line:
{{"description": "Verify login with valid credentials", "preConditions": "- Account exists\n- User is signed out", "steps": "1. Open login page\n2. Submit valid credentials", "expectedResult": "User is redirected to dashboard", "priority": "High"}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_test_case_prompt_uses_defaults_when_custom_fields_are_blank() {
        let prompts = TestCaseWriterPrompts::default();
        let prompt = build_test_case_prompt(
            "42",
            "Checkout Flow",
            "<p>User pays via card</p>",
            None,
            "   ",
            &prompts,
        );

        assert!(prompt.contains("<ticket>\nID: 42\nTitle: Checkout Flow\nDescription: <p>User pays via card</p>\nAcceptance Criteria: N/A\n</ticket>"));
        assert!(prompt.contains("<additional_context>\nNone provided\n</additional_context>"));
        assert!(!prompt.contains("<custom_instructions>"));
        assert!(prompt.contains(DEFAULT_TC_DESCRIPTION_PROMPT));
        assert!(prompt.contains(DEFAULT_TC_PRECONDITIONS_PROMPT));
        assert!(prompt.contains(DEFAULT_TC_STEPS_PROMPT));
        assert!(prompt.contains(DEFAULT_TC_EXPECTED_RESULT_PROMPT));
        assert!(prompt.contains(SYSTEM_TC_PRIORITY_PROMPT));
        assert!(prompt.contains(r#"do not include an `"id"` key"#));
        assert!(!prompt.contains("\\\\n"));
    }

    #[test]
    fn build_test_case_prompt_applies_custom_general_and_field_overrides() {
        let prompts = TestCaseWriterPrompts {
            general: "Focus on accessibility checks.".to_string(),
            description: "Include negative cases.".to_string(),
            pre_conditions: String::new(),
            steps: "Use Given/When/Then format.".to_string(),
            expected_result: "Specify ARIA state transitions.".to_string(),
        };
        let prompt = build_test_case_prompt(
            "repo/19",
            "Keyboard Nav",
            "Tab order works",
            Some("* Focus ring visible"),
            "Include screen readers",
            &prompts,
        );

        assert!(prompt.contains("<custom_instructions>\nFocus on accessibility checks.\n</custom_instructions>"));
        assert!(prompt.contains("Acceptance Criteria: * Focus ring visible\n</ticket>"));
        assert!(prompt.contains("<additional_context>\nInclude screen readers\n</additional_context>"));
        assert!(prompt.contains("- \"description\": (string) Include negative cases."));
        assert!(prompt.contains(DEFAULT_TC_PRECONDITIONS_PROMPT));
        assert!(prompt.contains("- \"steps\": (string) Use Given/When/Then format."));
        assert!(prompt.contains("- \"expectedResult\": (string) Specify ARIA state transitions."));
    }
}
