/// Builds the T-Shirt Size Estimator prompt.
pub fn build_tshirt_prompt(description: &str) -> String {
    format!(
        r#"Inspect the local repository and estimate the implementation effort and complexity of the proposed change below.

<proposed_change>
{description}
</proposed_change>

## Constraints
- Read-only: inspect repository files without modifying, creating, or deleting any files or running mutating commands.
- Ground your estimate in the actual files, components, and architecture you inspect.
- Progress updates: call `report_intent` periodically (e.g., `report_intent(intent: "Analyzing repository structure...")`); do not emit status updates as text or JSON lines.

## Sizing Scale
- `XS` (Extra Small): Tiny change (e.g., config tweak, typo fix, single-line edit). Usually < 1 day.
- `S` (Small): Minor change (e.g., simple helper function, minor UI tweak, simple unit test). Usually < 5 days.
- `M` (Medium): Moderate change (e.g., new feature spanning a few files, API endpoint, schema change). Usually < 10 days.
- `L` (Large): Significant change (e.g., complex feature, module refactoring, third-party integration). Usually < 15 days.
- `XL` (Extra Large): Very large change (e.g., major architectural rewrite, database migration). Usually > 15 days.

## Output Format
Output your final estimate as a single standalone JSON object on one line (strict JSONL), with no wrapping array, markdown code fences, or surrounding prose. Escape newlines as `\n` and double quotes as `\"` inside string values:
`{{"type": "estimate", "size": "XS|S|M|L|XL", "text": "Markdown explanation of the estimate, affected files/components, risks or complexities, and verification steps."}}`"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tshirt_prompt_includes_change_scale_constraints_and_schema() {
        let prompt = build_tshirt_prompt("Add OAuth PKCE flow");
        assert!(prompt.contains("<proposed_change>\nAdd OAuth PKCE flow\n</proposed_change>"));
        for size in ["XS", "S", "M", "L", "XL"] {
            assert!(prompt.contains(size));
        }
        assert!(prompt.contains("Read-only:"));
        assert!(prompt.contains("report_intent"));
        assert!(prompt.contains("\"type\": \"estimate\""));
        assert!(!prompt.contains("You are "));
    }
}
