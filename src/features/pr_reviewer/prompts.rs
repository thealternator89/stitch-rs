use crate::types::{KnownDoc, PrDiffFile, ReviewComment, ReviewCommentType};

fn escape_xml_attr(val: &str) -> String {
    val.replace('&', "&amp;").replace('"', "&quot;")
}

/// Builds the Copilot prompt for the AI Critic evaluation pass over draft PR review comments.
pub fn build_critic_prompt(
    comments: &[ReviewComment],
    pr_description: &str,
    persona_guidelines: &str,
    critic_instruction: &str,
) -> String {
    let comments_formatted = comments
        .iter()
        .enumerate()
        .map(|(index, c)| {
            let num = index + 1;
            let kind_str = match c.comment_type {
                ReviewCommentType::General => "general",
                ReviewCommentType::Line => "line",
            };
            let mut attrs = format!(r#"index="{num}" type="{kind_str}""#);
            if let Some(f) = c.file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                attrs.push_str(&format!(r#" file="{}""#, escape_xml_attr(f)));
            }
            if let Some(ln) = c.line {
                attrs.push_str(&format!(r#" line="{ln}""#));
            }
            if let Some(phase) = c.phase.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                attrs.push_str(&format!(r#" phase="{}""#, escape_xml_attr(phase)));
            }
            format!("<comment {attrs}>\n{}\n</comment>", c.comment)
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let critic_instruction_block = if critic_instruction.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<critic_instructions>\n{}\n</critic_instructions>\n",
            critic_instruction.trim()
        )
    };

    let pr_description_block = if pr_description.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<pr_description>\n{}\n</pr_description>\n",
            pr_description.trim()
        )
    };

    let persona_instruction = if persona_guidelines.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<review_focus>\nThe draft comments were generated using these focus and tone guidelines — factor them into your evaluation:\n{}\n</review_focus>\n",
            persona_guidelines.trim()
        )
    };

    let count = comments.len();
    format!(
        r#"Evaluate the {count} draft pull request review comments below and decide the disposition of each.
{critic_instruction_block}{pr_description_block}{persona_instruction}
<proposed_comments>
{comments_formatted}
</proposed_comments>

## Constraints & Verification
- Read-only: inspect repository source files without modifying, creating, or deleting any files or running mutating commands.
- Progress updates: call `report_intent` periodically (e.g., `report_intent(intent: "Verifying comment against auth handler...")`); do not emit status updates as text or JSON lines.
- Verify accuracy and scope: inspect the repository source files to verify that each proposed comment is technically sound and within the scope of the pull request changes.
- Check existing PR threads: call `get_existing_comments` (by `file` with `startLine`/`endLine`, by `file` with `keywords`, or by `keywords` alone for general PR comments) to see whether an issue has already been raised or resolved.
- Duplicate prevention: never allow two or more surviving comments on the exact same line of the same file — merge them into one comment or reject the redundant ones.
- Completeness: account for every input comment (`1` through `{count}`) in an action.

## Decision Options
1. `approve`: Accurate, actionable, relevant to changed code within PR scope, and worth raising.
2. `edit`: Valid and useful, but wording, explanation, or formatting should be improved. You may edit general or line comments, convert a line comment to `"type": "general"`, or relocate a line comment to another `"file"` and/or `"line"` (e.g., anchoring to a method signature or class declaration).
3. `reject`: Invalid, false positive, out of scope, overly pedantic/nitpicky, inaccurate, or redundant. Provide a terse reason of a few words maximum (e.g., `"False positive"`, `"Out of scope"`, `"Duplicate"`, `"Pedantic nitpick"`, or `"Already raised"`).
4. `merge`: Two or more comments address the same root issue, file section, or exact file and line. Combine them into a single general or line comment.
5. `reply`: When `get_existing_comments` shows an issue is already covered by an existing PR thread, only reply (specifying `threadId` and the reply text) if the issue persists and the thread is resolved (replying automatically reactivates the thread) or if you add useful context not already covered in the thread; otherwise choose `reject` with reason `"Already raised"`.

## Output Format
Respond strictly in JSON Lines (JSONL): every line must be a single, standalone, valid JSON object, with no wrapping array, markdown code fences, or surrounding prose. Escape double quotes as `\"` and newlines as `\n` inside string values. `commentIndex` and elements of `commentIndices` are 1-based integers matching the `index` attributes above.

Use one of these schemas per line:
- Approve unchanged:
  `{{"action": "approve", "commentIndex": 1}}`
- Edit a line comment (wording, file, or line):
  `{{"action": "edit", "commentIndex": 1, "type": "line", "file": "path/to/file", "line": 42, "comment": "Updated markdown comment text..."}}`
- Edit or convert to a general PR comment:
  `{{"action": "edit", "commentIndex": 2, "type": "general", "comment": "Updated general PR comment text..."}}`
- Reject a comment:
  `{{"action": "reject", "commentIndex": 3, "reason": "Out of scope"}}`
- Merge multiple comments into a line comment:
  `{{"action": "merge", "commentIndices": [4, 5], "type": "line", "file": "path/to/file", "line": 42, "comment": "Combined merged line comment text..."}}`
- Merge multiple comments into a general PR comment:
  `{{"action": "merge", "commentIndices": [6, 7], "type": "general", "comment": "Combined general PR comment text..."}}`
- Reply to an existing thread for one comment:
  `{{"action": "reply", "commentIndex": 8, "threadId": 12345, "comment": "Reply markdown comment text..."}}`
- Merge multiple comments into a reply to an existing thread:
  `{{"action": "reply", "commentIndices": [9, 10], "threadId": 12345, "comment": "Combined reply markdown comment text..."}}"#
    )
}

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
            "\n<available_documentation>\nFetch any of these documents with the `request_documentation` tool using its ID:\n{list}\n</available_documentation>\n"
        )
    };

    let persona_instruction = if persona_guidelines.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<review_focus>\nApply these focus and tone guidelines to your review:\n{}\n</review_focus>\n",
            persona_guidelines.trim()
        )
    };

    let pr_description_block = if pr_description.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<pr_description>\n{}\n</pr_description>\n",
            pr_description.trim()
        )
    };

    let story_content_block = if story_content.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<linked_story>\n{}\n</linked_story>\n",
            story_content.trim()
        )
    };

    let custom_instructions_block = if custom_instructions.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<custom_instructions>\n{}\n</custom_instructions>\n",
            custom_instructions.trim()
        )
    };

    format!(
        r#"Review the Pull Request changes in the repository for phase "{phase_title}".
{persona_instruction}
<changed_files>
{files_list}
</changed_files>
{pr_description_block}{story_content_block}{docs_instructions}
<phase_guidelines>
{phase_content}
</phase_guidelines>
{custom_instructions_block}
## Constraints & Scope
- Read-only: inspect the repository without modifying, creating, or deleting any files or running mutating commands.
- Inspect the changed files and use git history (`git diff` or `git log`) to identify the exact added and modified lines.
- Strictly limit feedback to `<phase_guidelines>`. Ignore issues outside these guidelines, even if important, because separate phases handle other concerns.
- Anchor `"line"` comments only to added or modified lines in the diff, using the 1-based line number in the modified file and setting `"context"` to an integer number of surrounding lines before and after the target line (e.g., `0` for only line 42, or `5` for 5 lines before and after).
- Only emit comments when you have genuine issues or actionable feedback. Never output "looks good" comments, and emit nothing if there are no findings.
- Progress updates: call `report_intent` periodically (e.g., `report_intent(intent: "Checking authService.ts for logic flaws")`); do not emit status updates as text or JSON lines.

## Output Format
Respond strictly in JSON Lines (JSONL): every line must be a single, standalone, valid JSON object, with no wrapping array, markdown code fences, or surrounding prose. Escape double quotes as `\"` and newlines as `\n` inside string values.

Use one of these schemas per line:
- General comment (for PR-wide or whole-file feedback):
  `{{"type": "general", "comment": "Your review comment in Markdown format"}}`
- Line comment (anchored to a changed line):
  `{{"type": "line", "file": "path/to/file", "line": 42, "context": 5, "comment": "Your line-specific review comment in Markdown format"}}`"#
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

        assert!(full.contains("<review_focus>\nApply these focus and tone guidelines to your review:\nDirect and concise\n</review_focus>"));
        assert!(full.contains("phase \"Security Checks\""));
        assert!(full.contains("<changed_files>\n- src/auth.rs (modified)\n- src/routes.rs (added)\n</changed_files>"));
        assert!(full.contains("<pr_description>\nAdds login handler\n</pr_description>"));
        assert!(full.contains("<linked_story>\nStory #42: Enforce MFA\n</linked_story>"));
        assert!(full.contains("<available_documentation>"));
        assert!(full.contains("- \"Auth Spec\" (ID: 9981)"));
        assert!(full.contains("<phase_guidelines>\nVerify auth guards on every endpoint.\n</phase_guidelines>"));
        assert!(full.contains("<custom_instructions>\nPay attention to JWT expiry.\n</custom_instructions>"));
        assert!(full.contains("Read-only:"));
        assert!(full.contains("report_intent"));
        assert!(full.contains(r#"{"type": "general", "comment": "Your review comment in Markdown format"}"#));
        assert!(full.contains(r#"{"type": "line", "file": "path/to/file", "line": 42, "context": 5, "comment": "Your line-specific review comment in Markdown format"}"#));
        assert!(!full.contains("You are "));

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
        assert!(!minimal.contains("<pr_description>"));
        assert!(!minimal.contains("<linked_story>"));
        assert!(!minimal.contains("<available_documentation>"));
        assert!(!minimal.contains("<review_focus>"));
        assert!(!minimal.contains("<custom_instructions>"));
        assert!(!minimal.contains("You are "));
    }

    #[test]
    fn build_critic_prompt_formats_comments_and_optional_sections() {
        let comments = vec![
            ReviewComment {
                id: "c1".to_string(),
                comment_type: ReviewCommentType::Line,
                file: Some("src/auth.rs".to_string()),
                line: Some(42),
                context: Some(5),
                comment: "Missing token expiration check.".to_string(),
                phase: Some("Security".to_string()),
                phase_id: Some("sec".to_string()),
                code_lines: None,
                posted: false,
                status: None,
                reason: None,
                thread_id: None,
                merged_from_indices: None,
            },
            ReviewComment {
                id: "c2".to_string(),
                comment_type: ReviewCommentType::General,
                file: None,
                line: None,
                context: None,
                comment: "Add unit test coverage.".to_string(),
                phase: Some("Quality".to_string()),
                phase_id: Some("qa".to_string()),
                code_lines: None,
                posted: false,
                status: None,
                reason: None,
                thread_id: None,
                merged_from_indices: None,
            },
        ];

        let prompt = build_critic_prompt(
            &comments,
            "Implements JWT login",
            "Focus on security bugs only",
            "Reject style nitpicks aggressively.",
        );

        assert!(prompt.contains("<critic_instructions>\nReject style nitpicks aggressively.\n</critic_instructions>"));
        assert!(prompt.contains("<pr_description>\nImplements JWT login\n</pr_description>"));
        assert!(prompt.contains("<review_focus>"));
        assert!(prompt.contains("Focus on security bugs only"));
        assert!(prompt.contains("Evaluate the 2 draft pull request review comments below"));
        assert!(prompt.contains("<proposed_comments>"));
        assert!(prompt.contains("<comment index=\"1\" type=\"line\" file=\"src/auth.rs\" line=\"42\" phase=\"Security\">\nMissing token expiration check.\n</comment>"));
        assert!(prompt.contains("<comment index=\"2\" type=\"general\" phase=\"Quality\">\nAdd unit test coverage.\n</comment>"));
        assert!(prompt.contains("Read-only:"));
        assert!(prompt.contains("report_intent"));
        assert!(prompt.contains("get_existing_comments"));
        assert!(prompt.contains(r#"Escape double quotes as `\"`"#));
        assert!(prompt.contains("\"action\": \"reply\""));
        assert!(!prompt.contains("You are "));
    }
}
