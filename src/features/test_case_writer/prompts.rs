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
        format!("\n{general}\n")
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
        r#"Generate a set of comprehensive test cases for the following user story/ticket.
{general_block}
Ticket ID: {ticket_id}
Title: {ticket_title}
Description: {ticket_description}
Acceptance Criteria: {ac_str}

Additional Context: {context_str}

Please format the output as JSON Lines (JSONL), where each line is a valid JSON object.
Do NOT wrap the JSON objects inside a JSON array. Each line MUST be a standalone JSON object.

Do NOT use markdown code blocks, fences, or any formatting other than plain JSON objects. All newlines within string values must be represented as `\n` (double backslash-n), not as actual newlines.

Each JSON object must have exactly the following keys:
- "description": (string) {description_prompt}
- "preConditions": (string) {pre_conditions_prompt}
- "steps": (string) {steps_prompt}
- "expectedResult": (string) {expected_result_prompt}
- "priority": (string) {SYSTEM_TC_PRIORITY_PROMPT}

DO NOT include an "id" key in the JSON object.

For example:
{{"description": "It works", "preConditions": "* One\\n* Two\\n* Three", "steps": "1. Step 1\\n2. Step 2\\n3. Step 3", "expectedResult": "Nothing", "priority": "High"}}

All double quotes inside string values must be escaped as \".
Do not use markdown formatting or syntax; only plain text is allowed.
Do not use actual newlines inside string values; use \n instead.
Bullet points or numbers should be plain text only (e.g., "1. Step one\\n2. Step two").

DO NOT create any files, directly output the test cases in your response here.
DO NOT include any other text in your response (no explanation, no intro, no outro, no markdown fences)."#
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

        assert!(prompt.contains("Ticket ID: 42"));
        assert!(prompt.contains("Title: Checkout Flow"));
        assert!(prompt.contains("Description: <p>User pays via card</p>"));
        assert!(prompt.contains("Acceptance Criteria: N/A"));
        assert!(prompt.contains("Additional Context: None provided"));
        assert!(prompt.contains(DEFAULT_TC_DESCRIPTION_PROMPT));
        assert!(prompt.contains(DEFAULT_TC_PRECONDITIONS_PROMPT));
        assert!(prompt.contains(DEFAULT_TC_STEPS_PROMPT));
        assert!(prompt.contains(DEFAULT_TC_EXPECTED_RESULT_PROMPT));
        assert!(prompt.contains(SYSTEM_TC_PRIORITY_PROMPT));
        assert!(prompt.contains(r#"DO NOT include an "id" key in the JSON object."#));
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

        assert!(prompt.contains("\nFocus on accessibility checks.\n"));
        assert!(prompt.contains("Acceptance Criteria: * Focus ring visible"));
        assert!(prompt.contains("Additional Context: Include screen readers"));
        assert!(prompt.contains("- \"description\": (string) Include negative cases."));
        assert!(prompt.contains(DEFAULT_TC_PRECONDITIONS_PROMPT));
        assert!(prompt.contains("- \"steps\": (string) Use Given/When/Then format."));
        assert!(prompt.contains("- \"expectedResult\": (string) Specify ARIA state transitions."));
    }
}
