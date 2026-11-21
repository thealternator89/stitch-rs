use crate::types::StoryWriterPrompts;

pub const DEFAULT_STORY_TITLE_PROMPT: &str = "The title of the story";
pub const DEFAULT_STORY_DESCRIPTION_PROMPT: &str = "A user story statement (\"As a... I want to... So that...\"), followed by a blank line and a detailed description of the required changes.";
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
        format!("\n<custom_instructions>\n{general}\n</custom_instructions>\n")
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
        r#"Generate user stories from the functional requirements in the Confluence page below.
{general_block}
<confluence_page>
Title: {page_title}
Content:
{page_content}
</confluence_page>

<additional_context>
{context_str}
</additional_context>

## Output Format
Output the user stories directly in your response as JSON Lines (JSONL), where each line is a single, standalone, valid JSON object. Do not wrap the objects in a JSON array, create any files, use markdown code fences, or include any surrounding prose.

Each JSON object must contain exactly these keys:
- "title": (string) {title_prompt}
- "description": (string) {description_prompt}
- "acceptanceCriteria": (string) {ac_prompt}
- "notes": (string) {notes_prompt}

Inside JSON string values, escape newlines as `\n` (no raw literal newlines) and double quotes as `\"`. Markdown inside string values (such as bullet lists in `acceptanceCriteria`) is allowed where requested by the field instructions.

Example line:
{{"title": "Title text", "description": "As a user...\n\nDetailed description of the change.", "acceptanceCriteria": "* AC 1\n* AC 2\n* AC 3", "notes": "Assumption 1\nAssumption 2"}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_story_prompt_uses_defaults_when_custom_fields_are_blank() {
        let prompts = StoryWriterPrompts::default();
        let prompt = build_story_prompt("Auth Spec", "<p>Body</p>", "   ", &prompts);

        assert!(prompt.contains("<confluence_page>\nTitle: Auth Spec\nContent:\n<p>Body</p>\n</confluence_page>"));
        assert!(prompt.contains("<additional_context>\nNone provided\n</additional_context>"));
        assert!(!prompt.contains("<custom_instructions>"));
        assert!(prompt.contains(DEFAULT_STORY_TITLE_PROMPT));
        assert!(prompt.contains(DEFAULT_STORY_DESCRIPTION_PROMPT));
        assert!(prompt.contains(DEFAULT_STORY_AC_PROMPT));
        assert!(prompt.contains(DEFAULT_STORY_NOTES_PROMPT));
        assert!(!prompt.contains("\\\\n"));
        assert!(!prompt.contains("only plain text is allowed"));
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

        assert!(prompt.contains("<custom_instructions>\nFocus on Rust services.\n</custom_instructions>"));
        assert!(prompt.contains("<additional_context>\nSplit by API\n</additional_context>"));
        assert!(prompt.contains("- \"title\": (string) Use verb-noun titles."));
        assert!(prompt.contains(DEFAULT_STORY_DESCRIPTION_PROMPT));
        assert!(prompt.contains("- \"acceptanceCriteria\": (string) Given/When/Then list."));
        assert!(prompt.contains("- \"notes\": (string) Include edge cases."));
    }
}
