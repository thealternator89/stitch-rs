/// Builds the Code Explainer prompt.
pub fn build_code_explainer_prompt(request: &str, custom_general: &str) -> String {
    let custom = custom_general.trim();
    let custom_block = if custom.is_empty() {
        String::new()
    } else {
        format!("\n<custom_instructions>\n{custom}\n</custom_instructions>\n")
    };

    let request = {
        let t = request.trim();
        if t.is_empty() { "N/A" } else { t }
    };

    format!(
        r##"Investigate the local repository and produce comprehensive technical documentation for the requested area.
{custom_block}
<user_request>
{request}
</user_request>

## Constraints
- Read-only: inspect the codebase without modifying, creating, or deleting any files or running mutating commands.
- Grounding: verify every claim in code you have actually read; cite exact file paths and symbol names.
- Progress updates: call the `report_intent` tool (e.g., `report_intent(intent: "Tracing the pricing calculation...")`); do not emit status updates as text or JSON lines.

## Process
1. Explore the repository structure, entry points, main modules, and related tests for `<user_request>`.
2. If scope, target variant, or depth is ambiguous, ask clarifying questions one at a time (at most about 5 total); skip questions if the request is already clear, and stop asking if the user tells you to proceed.
3. After scope is confirmed, read the relevant implementations, call chains, tests, and configuration thoroughly.
4. Deliver the finished documentation in a `"documentation"` JSONL message.

Every turn where you do not call a tool must emit either a single `"question"` object (and stop to wait for the reply) or the final `"documentation"` object.

## Output Format
Respond strictly in JSON Lines (JSONL): every line must be a single, standalone, valid JSON object, with no wrapping array, markdown code fences, or surrounding prose. Escape newlines as `\n` and double quotes as `\"` inside string values.

Choose one of these schemas:
- Question (one per turn, then stop to wait for the answer; `suggestedAnswers` is optional):
  `{{"type": "question", "text": "I found two pricing paths: PricingService (src/pricing.rs) and LegacyQuote (src/legacy/quote.rs). Which should be documented?", "suggestedAnswers": ["PricingService only", "Both", "LegacyQuote only"]}}`
- Final documentation:
  `{{"type": "documentation", "text": "# Feature Name\n\n## Overview\n..."}}`

## Documentation Structure
Use GitHub-flavoured Markdown with the sections below (omit a section only if truly not applicable, and note why). Prefer precision and completeness over brevity, using file paths and function names throughout without inventing unverified behaviour:
- `# <Title>`
- `## Overview` — purpose, responsibilities, and what is in and out of scope
- `## Architecture & Components` — each module, class, or function involved, its role, and file paths (e.g., `src/foo/bar.rs`), plus a mermaid diagram of the structure
- `## Data Flow` — step-by-step walkthrough from entry point to result, with a mermaid sequence or flowchart diagram
- `## Key Algorithms & Calculations` — every formula, rule, threshold, rounding behaviour, and ordering; quote constants and show worked examples where helpful, referencing the exact file and function
- `## Data Models` — important structs, types, schemas, and tables, their fields and meanings, and invariants
- `## Configuration & Dependencies` — settings, environment variables, feature flags, external services, and libraries
- `## Error Handling & Edge Cases` — failure modes, validation, special cases, and gotchas
- `## Testing` — what is covered by existing tests and notable gaps
- `## Open Questions` — anything you could not determine or that looks suspicious"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_request_and_custom_instructions() {
        let p = build_code_explainer_prompt("How invoices are totalled", "Prefer short sections.");
        assert!(p.contains("<user_request>\nHow invoices are totalled\n</user_request>"));
        assert!(p.contains("<custom_instructions>\nPrefer short sections.\n</custom_instructions>"));
        assert!(p.contains("\"type\": \"question\""));
        assert!(p.contains("\"type\": \"documentation\""));
        assert!(p.contains("Read-only"));
        assert!(p.contains("report_intent"));
        assert!(!p.contains("You are "));
    }

    #[test]
    fn prompt_handles_empty_request_and_no_custom() {
        let p = build_code_explainer_prompt("  ", "");
        assert!(p.contains("<user_request>\nN/A\n</user_request>"));
        assert!(!p.contains("<custom_instructions>"));
    }
}
