use crate::types::{KnownDoc, PrDiffFile};

/// Builds the Copilot prompt for an individual PR review phase.
#[allow(clippy::too_many_arguments)]
pub fn build_phase_review_prompt(
    files: &[PrDiffFile],
    phase_title: &str,
    phase_content: &str,
    custom_instructions: &str,
    pr_description: &str,
    story_content: &str,
    known_docs: &[KnownDoc],
    persona_guidelines: &str,
) -> String {
    let files_list = files
        .iter()
        .map(|file| format!("- {} ({})", file.path, file.status.label()))
        .collect::<Vec<_>>()
        .join("\n");

    let docs_instructions = if known_docs.is_empty() {
        String::new()
    } else {
        let list = known_docs
            .iter()
            .map(|d| format!("- \"{}\" (ID: {})", d.title, d.id))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n\nYou have identified the following documentation links in the linked story. You can request the content of any of these documents using the \"request_documentation\" tool with the corresponding document ID:\n{list}\n"
        )
    };

    let persona_instruction = if persona_guidelines.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nFor this review, you must adhere to the following specific focus guidelines:\n- Guidelines: {}\n\nMake sure your tone, review style, and feedback reflect these focus guidelines.\n",
            persona_guidelines.trim()
        )
    };

    let pr_description_block = if pr_description.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nHere is the Pull Request Description for additional context:\n--- PR DESCRIPTION ---\n{}\n----------------------\n",
            pr_description.trim()
        )
    };

    let story_content_block = if story_content.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nHere is the User Story/Work Item for additional context:\n--- LINKED STORY ---\n{}\n--------------------\n",
            story_content.trim()
        )
    };

    let custom_instructions_block = if custom_instructions.trim().is_empty() {
        String::new()
    } else {
        format!(
            "Additional specific instructions for this review:\n{}\n",
            custom_instructions.trim()
        )
    };

    format!(
        r#"You are an expert software engineer and code reviewer.
{persona_instruction}
Your task is to review the changes in the repository for the phase: "{phase_title}".

The following files have been modified/added/deleted in this Pull Request and are relevant to this phase:
{files_list}
{pr_description_block}
{story_content_block}{docs_instructions}
Please inspect these files using your codebase tools (such as reading file contents or looking at specific ranges of files) to understand the changes made.
You MUST use the git history (such as git diff or git log) to identify the exact changes made to the files we are reviewing. Only suggest comments on the changed (added or modified) lines. Do not suggest line-specific comments on lines of code that are outside the scope of the change.

Then, perform a thorough review, checking for adherence ONLY to the following phase guidelines:

--- PHASE GUIDELINES ---
{phase_content}
------------------------

DO NOT comment on any issues you see which are not explicitly related to the phase guidelines. This is critical.
You are conducting exactly one phase of a multi-phase review. Even if you spot a major issue, if it does not fall under the current phase guidelines, IGNORE IT. Assume it will be handled in a separate phase; mentioning it now will only cause disruptive, duplicate comments.

{custom_instructions_block}

Your response must strictly consist of JSON Lines (JSONL).
Every line of your response MUST be a single, standalone, valid JSON object.
Do NOT wrap the JSON objects in an array.
Do NOT output markdown code blocks (such as ```json) wrapping your JSONL output.
All double quotes and newlines inside the JSON strings must be properly escaped (e.g. \" for quotes, \n for newlines).

Each JSON object on each line must conform to one of the following formats:

1. For a general comment about the entire PR or a file as a whole:
{{
  "type": "general",
  "comment": "Your review comment in Markdown format"
}}
Only output a general comment if you have constructive feedback, suggestions, or issues to point out. Do NOT output a general comment stating that the code looks good, that there are no obvious issues, or that everything is fine. If you have no issues or feedback to report, do not output any comments (general or line) at all.

2. For a line-specific comment anchored to a particular line:
{{
  "type": "line",
  "file": "path/to/file",
  "line": 42,
  "context": 5,
  "comment": "Your line-specific review comment in Markdown format"
}}

For any status updates, progress updates, or internal thoughts that you want to show to the user, you MUST call the "report_intent" tool with the details of what you are doing (e.g., calling report_intent(intent: "Checking authService.ts for potential logic flaws")). You are forbidden from outputting status updates as JSON lines.

Ensure the "line" number corresponds to the line in the modified version of the file (after applying the diff). The "context" field must be an integer indicating how many surrounding lines of code to display before and after this line (e.g. 0 to display only line 42, or 5 to display 5 lines before, line 42, and 5 lines after).
You MUST only suggest line-specific comments (type: 'line') on lines that were actually changed (added or modified) as shown in the git history. Do not comment on unchanged lines.

You are highly encouraged to call the "report_intent" tool periodically as you proceed to let the user know what you are doing.
If you have no issues or feedback to report for a phase, simply do not output any review comments (general or line-specific) at all for that phase.

Begin your review now."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PrDiffStatus;

    #[test]
    fn build_phase_review_prompt_includes_optional_context_and_jsonl_rules() {
        let files = vec![
            PrDiffFile {
                path: "src/auth.rs".to_string(),
                status: PrDiffStatus::Modified,
            },
            PrDiffFile {
                path: "src/routes.rs".to_string(),
                status: PrDiffStatus::Added,
            },
        ];
        let docs = vec![KnownDoc {
            id: "9981".to_string(),
            title: "Auth Spec".to_string(),
        }];

        let full = build_phase_review_prompt(
            &files,
            "Security Checks",
            "Verify auth guards on every endpoint.",
            "Pay attention to JWT expiry.",
            "Adds login handler",
            "Story #42: Enforce MFA",
            &docs,
            "Direct and concise",
        );

        assert!(full.contains("For this review, you must adhere to the following specific focus guidelines:\n- Guidelines: Direct and concise"));
        assert!(full.contains("the phase: \"Security Checks\""));
        assert!(full.contains("- src/auth.rs (modified)\n- src/routes.rs (added)"));
        assert!(full.contains("--- PR DESCRIPTION ---\nAdds login handler\n----------------------"));
        assert!(full.contains("--- LINKED STORY ---\nStory #42: Enforce MFA\n--------------------"));
        assert!(full.contains("- \"Auth Spec\" (ID: 9981)"));
        assert!(full.contains("--- PHASE GUIDELINES ---\nVerify auth guards on every endpoint.\n------------------------"));
        assert!(full.contains("Additional specific instructions for this review:\nPay attention to JWT expiry."));
        assert!(full.contains("report_intent"));
        assert!(full.contains("Begin your review now."));

        let minimal = build_phase_review_prompt(
            &files,
            "Style",
            "Check naming.",
            "",
            "",
            "",
            &[],
            "",
        );
        assert!(!minimal.contains("--- PR DESCRIPTION ---"));
        assert!(!minimal.contains("--- LINKED STORY ---"));
        assert!(!minimal.contains("request_documentation"));
        assert!(!minimal.contains("specific focus guidelines"));
        assert!(!minimal.contains("Additional specific instructions"));
    }
}
