/// Builds the initial prompt for generating a Mermaid diagram.
pub fn build_initial_diagram_prompt(description: &str, has_project_dir: bool) -> String {
    let request = {
        let trimmed = description.trim();
        if trimmed.is_empty() { "N/A" } else { trimmed }
    };

    let project_block = if has_project_dir {
        r#"
## Codebase Investigation (Read-Only)
- A local project directory is attached as your working directory.
- Investigate the relevant files, modules, data models, and call chains before designing the diagram so that every component, label, and relationship reflects the actual codebase.
- Strictly read-only: do NOT modify, create, or delete any files, and do NOT run mutating commands.
"#
    } else {
        ""
    };

    format!(
        r##"Create a Mermaid diagram that fulfills the user's request.

<user_request>
{request}
</user_request>
{project_block}
## Behavior & Constraints
- One-sided execution: NEVER ask clarifying questions and NEVER output explanatory prose or commentary. If any detail is unspecified, make sensible architectural defaults.
- Progress updates: call the `report_intent` tool (e.g., `report_intent(intent: "Investigating request routing...")`) to report progress; do not emit status text outside JSON.

## Mermaid Syntax Hygiene
- Start with a valid Mermaid diagram header (e.g., `flowchart TD`, `flowchart LR`, `sequenceDiagram`, `classDiagram`, `stateDiagram-v2`, `erDiagram`, `C4Context`, etc.).
- Quote node labels that contain spaces, parentheses, brackets, colons, slashes, or special characters: e.g., `api["API Gateway (HTTPS)"]` instead of `api[API Gateway (HTTPS)]`.
- Keep node IDs simple alphanumeric/underscore identifiers (`auth_svc`, `db_1`).
- Do not include markdown code fences (` ``` `) inside the Mermaid text.

## Output Format
Respond with a single, valid JSON object on one line (JSONL) with no markdown code fences or surrounding prose. Escape newlines as `\n` and double quotes as `\"` inside the string value:
{{"type": "diagram", "mermaid": "flowchart TD\n    A[\"Start\"] --> B[\"End\"]"}}"##
    )
}

/// Builds a follow-up change-request prompt, embedding the active Mermaid snapshot when available.
pub fn build_change_request_prompt(change_request: &str, current_mermaid: Option<&str>) -> String {
    let change = {
        let trimmed = change_request.trim();
        if trimmed.is_empty() { "N/A" } else { trimmed }
    };

    let current_block = match current_mermaid.map(str::trim).filter(|s| !s.is_empty()) {
        Some(src) => format!(
            "\n<current_diagram>\n{src}\n</current_diagram>\n\nApply `<requested_change>` directly to `<current_diagram>` (the user may have undone/reverted earlier changes, so `<current_diagram>` is the authoritative starting point)."
        ),
        None => String::new(),
    };

    format!(
        r##"Update the Mermaid diagram according to the user's requested change.
{current_block}

<requested_change>
{change}
</requested_change>

## Behavior & Constraints
- One-sided execution: NEVER ask clarifying questions and NEVER output explanatory prose or commentary.
- Progress updates: call the `report_intent` tool if investigating files or making multi-step updates.
- If a local project directory is attached, keep all file access strictly read-only.

## Output Format
Return the complete updated Mermaid diagram as a single, valid JSON object on one line (JSONL) with no markdown code fences or surrounding prose. Escape newlines as `\n` and double quotes as `\"`:
{{"type": "diagram", "mermaid": "flowchart TD\n    A[\"Start\"] --> B[\"Updated\"]"}}"##
    )
}

/// Builds an automatic syntax-repair prompt when `rusty-mermaid` fails to parse or render.
pub fn build_syntax_fix_prompt(
    broken_mermaid: &str,
    error: &str,
    attempt: usize,
    max_attempts: usize,
) -> String {
    format!(
        r##"The Mermaid diagram you produced failed syntax validation/rendering (auto-fix attempt {attempt} of {max_attempts}). Fix the Mermaid syntax so it parses and renders cleanly.

<parser_error>
{error}
</parser_error>

<invalid_mermaid>
{broken_mermaid}
</invalid_mermaid>

## Instructions
- Preserve the diagram's intent and content while fixing the syntax/layout error.
- Quote any node or edge labels containing parentheses, brackets, colons, or special punctuation.
- NEVER ask questions or output prose.

## Output Format
Return ONLY the corrected Mermaid diagram as a single, valid JSON object on one line (JSONL) with no markdown code fences or surrounding prose:
{{"type": "diagram", "mermaid": "..."}}"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_prompt_toggles_project_instructions() {
        let with_dir = build_initial_diagram_prompt("Auth flow", true);
        assert!(with_dir.contains("<user_request>\nAuth flow\n</user_request>"));
        assert!(with_dir.contains("Strictly read-only"));
        assert!(with_dir.contains("\"type\": \"diagram\""));

        let without_dir = build_initial_diagram_prompt("Auth flow", false);
        assert!(without_dir.contains("<user_request>\nAuth flow\n</user_request>"));
        assert!(!without_dir.contains("Strictly read-only"));
    }

    #[test]
    fn change_prompt_embeds_current_diagram_when_present() {
        let with_cur =
            build_change_request_prompt("Add Redis cache", Some("flowchart LR\nA-->B"));
        assert!(with_cur.contains("<current_diagram>\nflowchart LR\nA-->B\n</current_diagram>"));
        assert!(with_cur.contains("<requested_change>\nAdd Redis cache\n</requested_change>"));

        let without_cur = build_change_request_prompt("Add Redis cache", None);
        assert!(!without_cur.contains("<current_diagram>"));
        assert!(without_cur.contains("<requested_change>\nAdd Redis cache\n</requested_change>"));
    }

    #[test]
    fn syntax_fix_prompt_includes_error_attempt_and_source() {
        let p = build_syntax_fix_prompt("flowchart\nA->", "unexpected token", 2, 3);
        assert!(p.contains("attempt 2 of 3"));
        assert!(p.contains("<parser_error>\nunexpected token\n</parser_error>"));
        assert!(p.contains("<invalid_mermaid>\nflowchart\nA->\n</invalid_mermaid>"));
    }
}
