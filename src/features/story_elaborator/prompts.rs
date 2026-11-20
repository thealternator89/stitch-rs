use crate::types::{KnownDoc, TicketData};

/// Builds the Story Elaborator system + task prompt matching the Electron specification.
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
        format!("\n{custom_general_trimmed}\n")
    };

    let ticket_id = ticket_data
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("N/A");

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
        r#"
You have access to the local codebase of the project through your built-in tools (such as reading files and browsing directories).
Use these tools to search, analyze, and inspect the codebase files to understand the project structure and existing implementations before formulating questions or plans.
"#
    } else {
        r#"
You DO NOT have access to a local codebase or repository.
Base all your questions, architectural assumptions, and the final plan entirely on the details provided in the ticket and user inputs.
Do NOT attempt to run any filesystem or command tools, as no repository context is available.
"#
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
            "\nYou have identified the following documentation links in this ticket. You can request the content of any of these documents using the \"request_documentation\" tool with the corresponding document ID:\n{docs_list}\n"
        )
    };

    format!(
        r##"You are a Story Elaborator. Your task is to elaborate the following user story / ticket into a detailed implementation plan.
{custom_general_block}
Ticket ID: {ticket_id}
Title: {}
Description: {}
Acceptance Criteria: {acceptance_criteria}

Additional Context: {context_str}

{repo_instructions}
{docs_instructions}

Once you have enough information, generate the final plan in markdown format and output it in a "plan" message.
If you have access to the repository you are forbidden from modifying, creating, or deleting any files. You must only read.

Your communication protocol with the host application is strictly JSON Lines (JSONL).
Every output line MUST be a single, standalone, valid JSON object. Do NOT wrap the JSON objects in an array. Do NOT output markdown fences (like ```json) wrapping your JSONL output.
All double quotes inside string values must be escaped as \". All actual newlines inside string values must be escaped as \n.

For any status updates, progress updates, or internal thoughts that you want to show to the user, you MUST call the "report_intent" tool with the details of what you are doing (e.g., calling report_intent(intent: "Analyzing codebase / reading package.json...")). You are forbidden from outputting status updates as JSON lines.

You must choose one of the following JSON formats for each line you output:
1. Questions (if you need clarification on requirements, architectural choices, styling preferences, or codebase details from the user). Ask exactly ONE question at a time and then STOP. Do not output anything else in that turn. You can optionally provide a list of suggested answers if you are able to guess or suggest sensible options:
   `{{"type": "question", "text": "Should we use React state or Redux to store this new field?", "suggestedAnswers": ["React State", "Redux", "Context API"]}}`

2. The Final Plan (when all questions are answered and the plan is ready. In this case, output a single JSON object):
   `{{"type": "plan", "text": "# Detailed Implementation Plan\n\n### Proposed Changes..."}}`

Follow this process:
1. Analyze the ticket, and if a repository is available, inspect the files using your tools.
2. If you want to request the content of any of the known documents, use the "request_documentation" tool to fetch its content.
3. If you are still analyzing or reading files, call your filesystem/grep tools to continue. Each turn where you do not call a tool must ask the user a clarifying question or present the final plan. Do not stop without either calling a tool or asking a question/presenting the plan.
4. Ask clarifying questions one by one, stopping after each question to wait for the user's response.
5. Once all details are resolved, draft the detailed implementation plan and return the "plan" message in JSONL format.

You are forbidden from ending the interaction without returning the "plan" message in JSONL format.

Start by analyzing the ticket details and/or repository, and ask your first question or call the "report_intent" tool followed by a tool call or question.

Your plan will be written as a comment on the ticket, so be concise and avoid repeating the ticket contents. Ideally your output is a list of things that need to be done to complete this ticket."##,
        ticket_data.title, ticket_data.description
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

        assert!(prompt.contains("Ticket ID: N/A"));
        assert!(prompt.contains("Title: Export audit logs"));
        assert!(prompt.contains("Description: Allow CSV download from history page."));
        assert!(prompt.contains("Acceptance Criteria: N/A"));
        assert!(prompt.contains("Additional Context: None provided"));
        assert!(prompt.contains("You DO NOT have access to a local codebase or repository."));
        assert!(prompt.contains("Do NOT attempt to run any filesystem or command tools"));
        assert!(!prompt.contains("You have identified the following documentation links"));
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

        assert!(prompt.contains("\nKeep plan concise with bullet points.\n"));
        assert!(prompt.contains("Ticket ID: 108"));
        assert!(prompt.contains("Acceptance Criteria: * Redirects to IdP"));
        assert!(prompt.contains("Additional Context: Prioritize PKCE flow"));
        assert!(prompt.contains("You have access to the local codebase of the project"));
        assert!(prompt.contains(r#"- "Auth Architecture" (ID: 5544)"#));
    }
}
