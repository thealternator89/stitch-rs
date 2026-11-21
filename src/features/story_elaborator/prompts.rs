use crate::types::{KnownDoc, TicketData};

/// Builds the Story Elaborator prompt.
pub fn build_story_elaborator_prompt(
    ticket_data: &TicketData,
    additional_context: &str,
    custom_general: &str,
    has_repo: bool,
    known_docs: &[KnownDoc],
) -> String {
    let custom_general_trimmed = custom_general.trim();
    let custom_general_block = if custom_general_trimmed.is_empty() {
        String::new()
    } else {
        format!("\n<custom_instructions>\n{custom_general_trimmed}\n</custom_instructions>\n")
    };

    let ticket_id = ticket_data
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("N/A");

    let title = &ticket_data.title;
    let description = &ticket_data.description;

    let acceptance_criteria = ticket_data
        .acceptance_criteria
        .as_deref()
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

    let repo_instructions = if has_repo {
        "Read-only access to the local repository: inspect existing code to ground your questions and implementation plan without modifying, creating, or deleting any files or running mutating commands."
    } else {
        "No local repository is attached: base all questions, architectural assumptions, and the final plan solely on the ticket, additional context, and linked documentation, and do not invoke filesystem or command tools."
    };

    let docs_instructions = if known_docs.is_empty() {
        String::new()
    } else {
        let docs_list = known_docs
            .iter()
            .map(|d| format!("- \"{}\" (ID: {})", d.title, d.id))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n<available_documentation>\nFetch any of these documents with the `request_documentation` tool using its ID:\n{docs_list}\n</available_documentation>\n"
        )
    };

    format!(
        r##"Elaborate the ticket below into a concise, actionable Markdown implementation plan.
{custom_general_block}
<ticket>
ID: {ticket_id}
Title: {title}
Description: {description}
Acceptance Criteria: {acceptance_criteria}
</ticket>

<additional_context>
{context_str}
</additional_context>
{docs_instructions}
## Environment
{repo_instructions}

## Process & Constraints
- Ask clarifying questions one at a time when requirements or architectural choices are genuinely ambiguous, stopping after each question to wait for the user's answer (optionally providing `suggestedAnswers`).
- Once open questions are resolved, deliver the final plan in a `"plan"` message. Because the plan will be posted as a comment on the ticket, keep it concise, avoid repeating the ticket contents verbatim, and focus on concrete changes and steps.
- Progress updates: call `report_intent` (e.g., `report_intent(intent: "Analyzing codebase and Cargo.toml...")`); do not emit status updates as text or JSON lines.
- Every turn where you do not call a tool must emit either a single `"question"` object or the final `"plan"` object.

## Output Format
Respond strictly in JSON Lines (JSONL): every line must be a single, standalone, valid JSON object, with no wrapping array, markdown code fences, or surrounding prose. Escape double quotes as `\"` and newlines as `\n` inside string values.

Choose one of these schemas:
- Question (one per turn, then stop to wait for the answer; `suggestedAnswers` is optional):
  `{{"type": "question", "text": "Should we store this new field in local state or the global store?", "suggestedAnswers": ["Local state", "Global store"]}}`
- Final plan:
  `{{"type": "plan", "text": "# Implementation Plan\n\n### Proposed Changes\n..."}}"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_story_elaborator_prompt_ticket_only_mode_uses_defaults() {
        let ticket = TicketData {
            id: None,
            title: "Export audit logs".to_string(),
            description: "Allow CSV download from history page.".to_string(),
            acceptance_criteria: Some("   ".to_string()),
        };

        let prompt = build_story_elaborator_prompt(&ticket, "   ", "", false, &[]);

        assert!(prompt.contains("ID: N/A"));
        assert!(prompt.contains("Title: Export audit logs"));
        assert!(prompt.contains("Description: Allow CSV download from history page."));
        assert!(prompt.contains("Acceptance Criteria: N/A"));
        assert!(prompt.contains("<additional_context>\nNone provided\n</additional_context>"));
        assert!(prompt.contains("No local repository is attached"));
        assert!(prompt.contains("do not invoke filesystem or command tools"));
        assert!(!prompt.contains("<available_documentation>"));
        assert!(!prompt.contains("<custom_instructions>"));
        assert!(!prompt.contains("You are "));
    }

    #[test]
    fn build_story_elaborator_prompt_includes_custom_general_repo_and_known_docs() {
        let ticket = TicketData {
            id: Some("108".to_string()),
            title: "Add SSO".to_string(),
            description: "Integrate OIDC".to_string(),
            acceptance_criteria: Some("* Redirects to IdP".to_string()),
        };
        let docs = vec![KnownDoc {
            id: "5544".to_string(),
            title: "Auth Architecture".to_string(),
        }];

        let prompt = build_story_elaborator_prompt(
            &ticket,
            "Prioritize PKCE flow",
            "Keep plan concise with bullet points.",
            true,
            &docs,
        );

        assert!(prompt.contains("<custom_instructions>\nKeep plan concise with bullet points.\n</custom_instructions>"));
        assert!(prompt.contains("<ticket>\nID: 108"));
        assert!(prompt.contains("Acceptance Criteria: * Redirects to IdP\n</ticket>"));
        assert!(prompt.contains("<additional_context>\nPrioritize PKCE flow\n</additional_context>"));
        assert!(prompt.contains("Read-only access to the local repository"));
        assert!(prompt.contains("<available_documentation>"));
        assert!(prompt.contains(r#"- "Auth Architecture" (ID: 5544)"#));
        assert!(!prompt.contains("You are "));
    }
}
