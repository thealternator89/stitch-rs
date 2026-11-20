use crate::types::StoryWriterPrompts;

pub const DEFAULT_STORY_TITLE_PROMPT: &str = "The title of the story";
pub const DEFAULT_STORY_DESCRIPTION_PROMPT: &str = "Description. This should contain a statement in the format \"As a... I want to... So that...\" followed by 2 blank lines and then a longer description of the changes required for story.";
pub const DEFAULT_STORY_AC_PROMPT: &str = "Formatted as a markdown list.";
pub const DEFAULT_STORY_NOTES_PROMPT: &str =
    "Any additional notes or assumptions (Optional, can be empty)";

fn non_empty_or_default<'a>(custom: &'a str, default: &'a str) -> &'a str {
    let trimmed = custom.trim();
    if trimmed.is_empty() { default } else { trimmed }
}

pub fn build_story_prompt(
    page_title: &str,
    page_content: &str,
    additional_context: &str,
    prompts: &StoryWriterPrompts,
) -> String {
    let general = prompts.general.trim();
    let general_block = if general.is_empty() {
        String::new()
    } else {
        format!("\n{general}\n")
    };

    let title_prompt = non_empty_or_default(&prompts.title, DEFAULT_STORY_TITLE_PROMPT);
    let description_prompt =
        non_empty_or_default(&prompts.description, DEFAULT_STORY_DESCRIPTION_PROMPT);
    let ac_prompt = non_empty_or_default(&prompts.acceptance_criteria, DEFAULT_STORY_AC_PROMPT);
    let notes_prompt = non_empty_or_default(&prompts.notes, DEFAULT_STORY_NOTES_PROMPT);

    let context_str = {
        let trimmed = additional_context.trim();
        if trimmed.is_empty() {
            "None provided"
        } else {
            trimmed
        }
    };

    format!(
        r#"Generate a set of user stories based on the following functional requirements from a Confluence page.
{general_block}
Page Title: {page_title}
Page Content: {page_content}

Additional Context: {context_str}

Please format the output as JSON Lines (JSONL), where each line is a valid JSON object.
Do NOT wrap the JSON objects inside a JSON array. Each line MUST be a standalone JSON object.

Do NOT use markdown code blocks, fences, or any formatting other than plain JSON objects. All newlines within string values must be represented as `\n` (double backslash-n), not as actual newlines.

Each JSON object must have exactly the following keys:
- "title": (string) {title_prompt}
- "description": (string) {description_prompt}
- "acceptanceCriteria": (string) {ac_prompt}
- "notes": (string) {notes_prompt}

For any fields containing markdown, these MUST be formatted and escaped for JSON.

For example:
{{"title": "Title text", "description": "First line\\n\\nSecond line", "acceptanceCriteria": "* AC 1\\n* AC 2\\n* AC 3", "notes": "First line\\nSecond Line\\nThird Line"}}

All double quotes inside string values must be escaped as \".
Do not use markdown formatting or syntax; only plain text is allowed.
Do not use actual newlines inside string values; use \n instead.
Bullet points or numbers should be plain text only (e.g., "1. Step one\n2. Step two").

DO NOT create any files, directly output the user stories in your response here.
DO NOT include any other text in your response (no explanation, no intro, no outro, no markdown fences)."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_story_prompt_uses_defaults_when_custom_fields_are_blank() {
        let prompts = StoryWriterPrompts::default();
        let prompt = build_story_prompt("Auth Spec", "<p>Body</p>", "   ", &prompts);

        assert!(prompt.contains("Page Title: Auth Spec"));
        assert!(prompt.contains("Page Content: <p>Body</p>"));
        assert!(prompt.contains("Additional Context: None provided"));
        assert!(prompt.contains(DEFAULT_STORY_TITLE_PROMPT));
        assert!(prompt.contains(DEFAULT_STORY_DESCRIPTION_PROMPT));
        assert!(prompt.contains(DEFAULT_STORY_AC_PROMPT));
        assert!(prompt.contains(DEFAULT_STORY_NOTES_PROMPT));
    }

    #[test]
    fn build_story_prompt_applies_custom_general_and_field_overrides() {
        let prompts = StoryWriterPrompts {
            general: "Focus on Rust services.".to_string(),
            title: "Use verb-noun titles.".to_string(),
            description: String::new(),
            acceptance_criteria: "Given/When/Then list.".to_string(),
            notes: "Include edge cases.".to_string(),
        };
        let prompt = build_story_prompt("Billing", "<p>Specs</p>", "Split by API", &prompts);

        assert!(prompt.contains("\nFocus on Rust services.\n"));
        assert!(prompt.contains("Additional Context: Split by API"));
        assert!(prompt.contains("- \"title\": (string) Use verb-noun titles."));
        assert!(prompt.contains(DEFAULT_STORY_DESCRIPTION_PROMPT));
        assert!(prompt.contains("- \"acceptanceCriteria\": (string) Given/When/Then list."));
        assert!(prompt.contains("- \"notes\": (string) Include edge cases."));
    }
}
