use crate::features::solution_designer::storage::{DesignArea, LoadedProject, README_FILE_NAME};

fn custom_instructions_block(custom_general: &str) -> String {
    let trimmed = custom_general.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("\n<custom_instructions>\n{trimmed}\n</custom_instructions>\n")
    }
}

fn format_areas_overview(project: &LoadedProject) -> String {
    if project.metadata.areas.is_empty() {
        return "None defined yet.".to_string();
    }
    project
        .metadata
        .areas
        .iter()
        .map(|area| {
            let summary = if area.summary.trim().is_empty() {
                "No summary yet".to_string()
            } else {
                area.summary.trim().to_string()
            };
            format!(
                "- **{}** (`{}`) [{}] — {}",
                area.title,
                area.file,
                area.status.label(),
                summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const SHARED_WORKSPACE_AND_FORMAT_RULES: &str = r##"## Context & Interview Mechanics
- Purely greenfield / freeform context: base your questions and design on the user's context, your existing project markdown files, and your conversation — do not assume or reference any external codebase or issue tracker.
- Ask **one question per turn**, then stop and wait for the user's answer.
- For each question, proactively recommend an option with concise rationale, and provide clickable `suggestedAnswers` (putting your recommended option first, prefixed with `(Recommended)`).
- If the user asks you to proceed with what you know, wrap up immediately using reasonable recommendations and note any unconfirmed choices under `## Risks & Open Questions`.

## Workspace & File Rules
- Your working directory is the project's documentation folder. Use your file tools to read and write Markdown files directly in this directory.
- **NEVER create, edit, or delete `.stitch.json`** — Stitch manages `.stitch.json` automatically from your completion message.
- Progress updates: call `report_intent` (e.g., `report_intent(intent: "Writing 01-system-architecture.md...")`); do not emit status updates as plain text.

## Output Format
Respond strictly in JSON Lines (JSONL): every non-tool turn must emit a single, standalone, valid JSON object with no wrapping array, markdown code fences, or surrounding prose. Escape double quotes as `\"` and newlines as `\n` inside string values."##;

const AREA_DESIGN_INTERVIEW_RULES: &str = r##"## Design Focus & Interview Style
- Stay high-level and solution-oriented: focus on architecture, system boundaries, key decisions & trade-offs, interfaces/contracts, data/control flows (use Mermaid diagrams where helpful), and risks/open questions.
- Use an opinionated `/grill-me` interview style: walk down each branch of the design tree, resolving dependencies between decisions before moving on."##;

/// Builds the Ground Rules interview prompt for a newly created project.
pub fn build_ground_rules_prompt(
    project_name: &str,
    initial_context: &str,
    custom_general: &str,
) -> String {
    let custom_block = custom_instructions_block(custom_general);
    let context_str = {
        let trimmed = initial_context.trim();
        if trimmed.is_empty() {
            "None provided".to_string()
        } else {
            trimmed.to_string()
        }
    };

    format!(
        r##"Lead a Ground Rules scoping session for a new project and scaffold its initial documentation files.
{custom_block}
<project>
Name: {project_name}
</project>

<initial_working_context>
{context_str}
</initial_working_context>

{SHARED_WORKSPACE_AND_FORMAT_RULES}

## Ground Rules Scope & Guardrails
- **Purpose of this phase**: Establish the framing, shared ground rules, and the **list of design stages/areas** to dig into during later sessions.
- **Stay out of the weeds**: Do **NOT** ask about technology choices, schemas, APIs, deployment topology, SLOs, or implementation details in this phase. Whenever a deeper technical question or decision surfaces, defer it by adding it as a bullet point under "Key Topics & Questions to Resolve" inside the relevant area's stub file.
- **Keep it concise**: Aim for roughly 3–6 high-level scoping questions total:
  1. Clarify the core problem, boundaries/non-goals, and any overarching ground rules or constraints (only what is still unclear from `<initial_working_context>`).
  2. Propose a clear breakdown of solution design **areas/stages** (each becoming its own Markdown file) and confirm with the user what to add, split, combine, or drop.
- Once the ground rules and area list are agreed, use your file tools to write:
  - `{README_FILE_NAME}` containing Project Overview, Goals & Non-Goals, Ground Rules & Shared Principles, and a Section Index linking to each area `.md` file.
  - An initial stub `.md` file for every planned area (using numbered kebab-case filenames like `01-system-architecture.md`, `02-data-model.md`, etc.), each with `# <Area Title>`, a short scope summary, and a bulleted list of key topics & questions to resolve in its dedicated session.
- Only **after** `{README_FILE_NAME}` and all area stub files are written to disk, emit the `"complete"` JSONL object.

Choose one of these JSONL schemas:
- Question (one per turn, then stop to wait for the answer; `suggestedAnswers` is optional):
  `{{"type": "question", "text": "Based on your context, I recommend splitting this into 4 design areas: 1) System Architecture & Boundaries, 2) Core Data & State Model, 3) External Integrations, and 4) Operations & Reliability. Should we keep this breakdown or adjust any areas?", "suggestedAnswers": ["(Recommended) Keep these 4 areas", "Merge Integrations into System Architecture", "Add a Security & Access area"]}}`
- Complete (emit only after writing `{README_FILE_NAME}` and stub `.md` files):
  `{{"type": "complete", "summary": "Established ground rules and 4 initial design areas.", "areas": [{{"id": "system-architecture", "title": "System Architecture", "file": "01-system-architecture.md", "summary": "High-level components, boundaries, and responsibilities"}}]}}`"##
    )
}

/// Builds the `/grill-me` prompt for designing (or re-designing) a specific area.
pub fn build_area_design_prompt(
    project: &LoadedProject,
    area: &DesignArea,
    extra_context: &str,
    custom_general: &str,
    is_redesign: bool,
) -> String {
    let custom_block = custom_instructions_block(custom_general);
    let areas_overview = format_areas_overview(project);
    let extra_str = {
        let trimmed = extra_context.trim();
        if trimmed.is_empty() {
            "None provided".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let mode_note = if is_redesign {
        "This area already has a draft on disk; read it first, challenge existing assumptions where needed, and rewrite/update the file with the newly agreed design."
    } else {
        "Read the existing stub file (if present) and flesh it out into a complete high-level solution design document."
    };

    format!(
        r##"Lead a focused solution design interview for the **{area_title}** area of **{project_name}**, then write the finalized high-level design document to `{area_file}`.
{custom_block}
<project>
Name: {project_name}
Initial Context: {initial_context}
</project>

<target_area>
ID: {area_id}
Title: {area_title}
File: {area_file}
Summary: {area_summary}
</target_area>

<additional_area_context>
{extra_str}
</additional_area_context>

<existing_project_areas>
{areas_overview}
</existing_project_areas>

{AREA_DESIGN_INTERVIEW_RULES}

{SHARED_WORKSPACE_AND_FORMAT_RULES}

## Area Design Process & Scope Guardrails
1. First, read `{README_FILE_NAME}`, `{area_file}` (if it exists), and any relevant drafted sibling area `.md` files so your questions and design stay consistent with the project's ground rules.
2. {mode_note}
3. Conduct a `/grill-me` interview one question at a time until all key architectural decisions, interfaces, trade-offs, and failure modes for **{area_title}** are resolved.
4. Write the design to `{area_file}` using flexible high-level sections adapted to the topic (typically: `## Overview`, `## Key Decisions & Trade-offs`, `## Architecture & Flow` with Mermaid diagrams where helpful, `## Interfaces & Boundaries`, `## Risks & Open Questions`).
5. **Scope guardrail**: Keep your edits focused on `{area_file}` (and optionally update `{README_FILE_NAME}`'s section index/status if needed). **Never modify sibling area `.md` files**; if this design has cross-cutting impacts on other areas, list them under a `## Cross-Cutting Notes` or `## Risks & Open Questions` section in `{area_file}`.
6. Only **after** `{area_file}` is written on disk, emit the `"complete"` JSONL object.

Choose one of these JSONL schemas:
- Question (one per turn, then stop to wait for the answer):
  `{{"type": "question", "text": "...", "suggestedAnswers": ["(Recommended) ...", "..."]}}`
- Complete (emit only after writing `{area_file}`):
  `{{"type": "complete", "file": "{area_file}", "summary": "Concise 1-sentence summary of what was designed in this area."}}"##,
        area_title = area.title,
        area_file = area.file,
        area_id = area.id,
        area_summary = if area.summary.trim().is_empty() {
            "N/A"
        } else {
            area.summary.trim()
        },
        project_name = project.metadata.name,
        initial_context = if project.metadata.initial_context.trim().is_empty() {
            "None provided"
        } else {
            project.metadata.initial_context.trim()
        },
    )
}

/// Builds the `/grill-me` prompt for adding and designing a brand-new area inside an existing project.
pub fn build_new_area_prompt(
    project: &LoadedProject,
    title: &str,
    suggested_file: &str,
    extra_context: &str,
    custom_general: &str,
) -> String {
    let custom_block = custom_instructions_block(custom_general);
    let areas_overview = format_areas_overview(project);
    let extra_str = {
        let trimmed = extra_context.trim();
        if trimmed.is_empty() {
            "None provided".to_string()
        } else {
            trimmed.to_string()
        }
    };

    format!(
        r##"Lead a focused solution design interview for a **new design area** titled **{title}** in **{project_name}**, then write its high-level design document to `{suggested_file}`.
{custom_block}
<project>
Name: {project_name}
Initial Context: {initial_context}
</project>

<new_area>
Title: {title}
Target File: {suggested_file}
</new_area>

<additional_area_context>
{extra_str}
</additional_area_context>

<existing_project_areas>
{areas_overview}
</existing_project_areas>

{AREA_DESIGN_INTERVIEW_RULES}

{SHARED_WORKSPACE_AND_FORMAT_RULES}

## New Area Process & Scope Guardrails
1. Read `{README_FILE_NAME}` and any relevant drafted sibling `.md` files first to align with existing ground rules and boundaries.
2. Run a `/grill-me` interview one question at a time to resolve the design of **{title}**.
3. Write the high-level design document to `{suggested_file}` (Overview, Key Decisions & Trade-offs, Architecture & Flow with Mermaid where helpful, Interfaces & Boundaries, Risks & Open Questions).
4. Add **{title}** (`{suggested_file}`) to the Section Index in `{README_FILE_NAME}` if `{README_FILE_NAME}` exists. **Never modify sibling area `.md` files**; surface any cross-cutting impacts as notes in `{suggested_file}`.
5. Only **after** `{suggested_file}` is written on disk, emit the `"complete"` JSONL object.

Choose one of these JSONL schemas:
- Question (one per turn, then stop to wait for the answer):
  `{{"type": "question", "text": "...", "suggestedAnswers": ["(Recommended) ...", "..."]}}`
- Complete (emit only after writing `{suggested_file}`):
  `{{"type": "complete", "file": "{suggested_file}", "summary": "Concise 1-sentence summary of this new area."}}"##,
        project_name = project.metadata.name,
        initial_context = if project.metadata.initial_context.trim().is_empty() {
            "None provided"
        } else {
            project.metadata.initial_context.trim()
        },
    )
}

/// Builds a standalone refinement prompt when no live in-memory Copilot session exists for the target document.
pub fn build_standalone_refinement_prompt(
    project: &LoadedProject,
    target_title: &str,
    target_file: &str,
    is_readme: bool,
    instructions: &str,
    custom_general: &str,
) -> String {
    let custom_block = custom_instructions_block(custom_general);
    let areas_overview = format_areas_overview(project);

    let readme_rule = if is_readme {
        "If the refinement adds new design areas, also create initial stub `.md` files for them and include the full updated `areas` array in your `\"complete\"` message. Do not modify existing drafted/finalized area `.md` files.".to_string()
    } else {
        format!(
            "Modify only `{target_file}` (and optionally update `{README_FILE_NAME}`'s index/summary if needed). **Never modify sibling area `.md` files**; note any cross-cutting impacts inside `{target_file}`."
        )
    };

    let complete_schema = if is_readme {
        format!(
            r#"`{{"type": "complete", "file": "{README_FILE_NAME}", "summary": "Updated ground rules.", "areas": [{{"id": "...", "title": "...", "file": "01-....md", "summary": "..."}}]}}`"#
        )
    } else {
        format!(
            r#"`{{"type": "complete", "file": "{target_file}", "summary": "Concise summary of the updated design."}}`"#
        )
    };

    format!(
        r##"Refine the existing solution design document `{target_file}` ({target_title}) for **{project_name}** according to the user's instructions.
{custom_block}
<project>
Name: {project_name}
</project>

<existing_project_areas>
{areas_overview}
</existing_project_areas>

<refinement_instructions>
{instructions}
</refinement_instructions>

{AREA_DESIGN_INTERVIEW_RULES}

{SHARED_WORKSPACE_AND_FORMAT_RULES}

## Refinement Process
1. Read `{README_FILE_NAME}` and `{target_file}` from disk (plus relevant sibling docs if context is needed).
2. Ask clarifying questions one at a time **only** if the refinement instructions introduce genuine ambiguity; otherwise apply the changes directly to `{target_file}`.
3. {readme_rule}
4. Only **after** the updated file(s) are saved on disk, emit the `"complete"` JSONL object.

Choose one of these JSONL schemas:
- Question (one per turn, then stop to wait for the answer):
  `{{"type": "question", "text": "...", "suggestedAnswers": ["(Recommended) ...", "..."]}}`
- Complete (emit only after writing `{target_file}`):
  {complete_schema}"##,
        project_name = project.metadata.name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::solution_designer::storage::{AreaStatus, DesignProjectMetadata};
    use std::path::PathBuf;

    fn sample_project() -> LoadedProject {
        LoadedProject {
            dir: PathBuf::from("/tmp/demo-project"),
            metadata: DesignProjectMetadata {
                version: 1,
                name: "Demo Platform".to_string(),
                slug: "demo-platform".to_string(),
                initial_context: "Event-driven billing".to_string(),
                ground_rules_completed: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                areas: vec![DesignArea {
                    id: "ingestion".to_string(),
                    title: "Event Ingestion".to_string(),
                    file: "01-event-ingestion.md".to_string(),
                    summary: "Webhook and queue intake".to_string(),
                    status: AreaStatus::Planned,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                }],
            },
        }
    }

    #[test]
    fn ground_rules_prompt_includes_context_custom_instructions_and_stitch_json_guardrail() {
        let p = build_ground_rules_prompt("Billing", "Multi-tenant SaaS", "Keep diagrams simple.");
        assert!(p.contains("Name: Billing"));
        assert!(p.contains("Multi-tenant SaaS"));
        assert!(p.contains("<custom_instructions>\nKeep diagrams simple.\n</custom_instructions>"));
        assert!(p.contains("NEVER create, edit, or delete `.stitch.json`"));
        assert!(p.contains("Stay out of the weeds"));
        assert!(p.contains("Do **NOT** ask about technology choices"));
        assert!(!p.contains("active-active"));
        assert!(p.contains("\"type\": \"question\""));
        assert!(p.contains("\"type\": \"complete\""));
    }

    #[test]
    fn area_design_prompt_includes_target_area_and_sibling_guardrail() {
        let proj = sample_project();
        let area = proj.metadata.areas[0].clone();
        let p = build_area_design_prompt(&proj, &area, "Focus on idempotency", "", false);
        assert!(p.contains("01-event-ingestion.md"));
        assert!(p.contains("Focus on idempotency"));
        assert!(p.contains("Never modify sibling area `.md` files"));
    }

    #[test]
    fn new_area_and_standalone_refinement_prompts_include_target_files() {
        let proj = sample_project();
        let new_p = build_new_area_prompt(&proj, "Reporting", "02-reporting.md", "", "");
        assert!(new_p.contains("Target File: 02-reporting.md"));

        let ref_p = build_standalone_refinement_prompt(
            &proj,
            "Event Ingestion",
            "01-event-ingestion.md",
            false,
            "Add dead-letter queue retry policy",
            "",
        );
        assert!(ref_p.contains("01-event-ingestion.md"));
        assert!(ref_p.contains("Add dead-letter queue retry policy"));
    }
}
