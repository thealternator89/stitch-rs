use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::types::{PrReviewerDiskConfig, ReviewPhase};

/// Returns the root PR Reviewer configuration directory (`~/.stitch/pr-reviewer`).
pub fn pr_reviewer_root_dir() -> PathBuf {
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".stitch").join("pr-reviewer")
}

/// Ensures `phases/` and `templates/` exist inside `root_dir`, returning `(phases_dir, templates_dir)`.
pub fn ensure_pr_reviewer_dirs(root_dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let phases_dir = root_dir.join("phases");
    let templates_dir = root_dir.join("templates");
    std::fs::create_dir_all(&phases_dir)?;
    std::fs::create_dir_all(&templates_dir)?;
    Ok((phases_dir, templates_dir))
}

/// Reads optional `config.json` from `root_dir` (`groups`, `criticInstruction`).
pub fn load_config_from_dir(root_dir: &Path) -> PrReviewerDiskConfig {
    let config_path = root_dir.join("config.json");
    if !config_path.is_file() {
        return PrReviewerDiskConfig::default();
    }
    match std::fs::read_to_string(&config_path) {
        Ok(content) => match serde_json::from_str::<PrReviewerDiskConfig>(&content) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!(
                    "Warning: failed to parse PR Reviewer config {}: {err}",
                    config_path.display()
                );
                PrReviewerDiskConfig::default()
            }
        },
        Err(err) => {
            eprintln!(
                "Warning: failed to read PR Reviewer config {}: {err}",
                config_path.display()
            );
            PrReviewerDiskConfig::default()
        }
    }
}

fn unquote_yaml_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_inline_yaml_list(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in inner.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                current.push(ch);
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            None if ch == ',' => {
                let cleaned = unquote_yaml_value(&current);
                if !cleaned.is_empty() {
                    items.push(cleaned);
                }
                current.clear();
            }
            None => current.push(ch),
        }
    }

    let tail = unquote_yaml_value(&current);
    if !tail.is_empty() {
        items.push(tail);
    }
    items
}

fn parse_frontmatter_value(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") || trimmed == "~" {
        return Vec::new();
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return parse_inline_yaml_list(&trimmed[1..trimmed.len() - 1]);
    }
    let val = unquote_yaml_value(trimmed);
    if val.is_empty() {
        Vec::new()
    } else {
        vec![val]
    }
}

/// Parses optional YAML `---` frontmatter from a Markdown file, lowercasing field keys.
pub fn parse_frontmatter(content: &str) -> (HashMap<String, Vec<String>>, String) {
    let normalized = content.strip_prefix('\u{feff}').unwrap_or(content);
    let normalized_lines = normalized.replace("\r\n", "\n");
    let mut lines = normalized_lines.split('\n');

    let Some(first) = lines.next() else {
        return (HashMap::new(), String::new());
    };
    if first.trim() != "---" {
        return (HashMap::new(), normalized.trim().to_string());
    }

    let mut fm_lines = Vec::new();
    let mut found_closing = false;
    for line in lines.by_ref() {
        let t = line.trim();
        if t == "---" || t == "..." {
            found_closing = true;
            break;
        }
        fm_lines.push(line);
    }

    if !found_closing {
        return (HashMap::new(), normalized.trim().to_string());
    }

    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    let mut frontmatter: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_key: Option<String> = None;

    for raw_line in fm_lines {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(after_dash) = trimmed.strip_prefix('-') {
            if after_dash.is_empty() || after_dash.starts_with(char::is_whitespace) {
                if let Some(ref key) = current_key {
                    let item = unquote_yaml_value(after_dash);
                    if !item.is_empty() {
                        frontmatter.entry(key.clone()).or_default().push(item);
                    }
                }
                continue;
            }
        }

        if let Some((raw_key, raw_val)) = raw_line.split_once(':') {
            let key = raw_key.trim().to_ascii_lowercase();
            if key.is_empty() {
                current_key = None;
                continue;
            }
            let values = parse_frontmatter_value(raw_val);
            frontmatter.insert(key.clone(), values);
            current_key = Some(key);
        }
    }

    (frontmatter, body)
}

fn apply_template_file(
    templates_dir: &Path,
    template_name: &str,
    parsed_body: &str,
) -> Result<String, String> {
    let clean_name = template_name.trim();
    let rel = Path::new(clean_name);
    if clean_name.is_empty()
        || rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "Directory traversal detected in template path: {template_name}"
        ));
    }

    let candidate_path = templates_dir.join(rel);
    if !candidate_path.exists() {
        return Err(format!(
            "Template file not found: {}",
            candidate_path.display()
        ));
    }

    if let (Ok(canon_tpl_dir), Ok(canon_candidate)) = (
        std::fs::canonicalize(templates_dir),
        std::fs::canonicalize(&candidate_path),
    ) {
        if !canon_candidate.starts_with(&canon_tpl_dir) {
            return Err(format!(
                "Directory traversal detected in template path: {template_name}"
            ));
        }
    }

    let template_content = std::fs::read_to_string(&candidate_path).map_err(|err| {
        format!(
            "Failed to read template file {}: {err}",
            candidate_path.display()
        )
    })?;

    if !template_content.contains("<%content%>") {
        return Err(format!(
            "Template \"{clean_name}\" is missing the required <%content%> placeholder."
        ));
    }

    Ok(template_content.replace("<%content%>", parsed_body))
}

/// Loads and deterministically sorts PR Review phases from `root_dir` (`phases/*.md`, `templates/*`, `config.json`).
pub fn load_phases_from_dir(root_dir: &Path) -> Vec<ReviewPhase> {
    let Ok((phases_dir, templates_dir)) = ensure_pr_reviewer_dirs(root_dir) else {
        return Vec::new();
    };
    if !phases_dir.is_dir() {
        return Vec::new();
    }

    let config = load_config_from_dir(root_dir);
    let group_order = &config.groups;

    let Ok(entries) = std::fs::read_dir(&phases_dir) else {
        return Vec::new();
    };

    let mut md_files: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    md_files.sort();

    let mut phases = Vec::with_capacity(md_files.len());
    for file in md_files {
        let full_path = phases_dir.join(&file);
        let Ok(content) = std::fs::read_to_string(&full_path) else {
            eprintln!(
                "Warning: failed to read phase file {}",
                full_path.display()
            );
            continue;
        };

        let (fm, parsed_body) = parse_frontmatter(&content);
        let first_val = |key: &str| -> Option<String> {
            fm.get(key)
                .and_then(|vals| vals.first())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let template = first_val("template");
        let mut body = parsed_body.clone();
        let mut template_error = None;

        if let Some(ref tpl_name) = template {
            match apply_template_file(&templates_dir, tpl_name, &parsed_body) {
                Ok(rendered) => body = rendered,
                Err(err) => template_error = Some(err),
            }
        }

        let title = first_val("title").unwrap_or_else(|| file.clone());
        let group = first_val("group").unwrap_or_else(|| "Ungrouped".to_string());
        let include = fm.get("include").cloned().unwrap_or_default();
        let exclude = fm.get("exclude").cloned().unwrap_or_default();
        let attach = fm.get("attach").cloned().unwrap_or_default();
        let model = first_val("model");

        phases.push(ReviewPhase {
            id: file,
            title,
            group,
            include,
            exclude,
            attach,
            body,
            template,
            template_error,
            model,
        });
    }

    phases.sort_by(|a, b| {
        let ga = if a.group.trim().is_empty() {
            "Ungrouped"
        } else {
            a.group.as_str()
        };
        let gb = if b.group.trim().is_empty() {
            "Ungrouped"
        } else {
            b.group.as_str()
        };

        if ga != gb {
            if ga == "Ungrouped" {
                return std::cmp::Ordering::Less;
            }
            if gb == "Ungrouped" {
                return std::cmp::Ordering::Greater;
            }

            let idx_a = group_order.iter().position(|g| g == ga);
            let idx_b = group_order.iter().position(|g| g == gb);
            match (idx_a, idx_b) {
                (Some(ia), Some(ib)) => return ia.cmp(&ib),
                (Some(_), None) => return std::cmp::Ordering::Less,
                (None, Some(_)) => return std::cmp::Ordering::Greater,
                (None, None) => {
                    let cmp = ga.to_lowercase().cmp(&gb.to_lowercase()).then_with(|| ga.cmp(gb));
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
            }
        }

        a.id.to_lowercase()
            .cmp(&b.id.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });

    phases
}

/// Loads PR Review phases from `~/.stitch/pr-reviewer/`.
pub fn load_phases_from_disk() -> Vec<ReviewPhase> {
    let root = pr_reviewer_root_dir();
    load_phases_from_dir(&root)
}

/// Ensures `dir` exists and opens it in the host OS file manager.
pub fn open_directory_in_os(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;

    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(dir).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer").arg(dir).spawn()?;
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        Command::new("xdg-open").arg(dir).spawn()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(suffix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("stitch-rs-phase-loader-{suffix}-{nanos}"))
    }

    #[test]
    fn parse_frontmatter_handles_scalars_inline_arrays_and_yaml_lists() {
        let md = "\
---
title: \"Security Audit\"
group: Security
include: [\"src/**/*.rs\", 'tests/*.ts']
exclude:
  - \"**/target/**\"
  - '**/generated/**'
attach: [description, story]
model: gpt-5.4
---

Check auth boundaries cleanly.
";
        let (fm, body) = parse_frontmatter(md);
        assert_eq!(fm.get("title"), Some(&vec!["Security Audit".to_string()]));
        assert_eq!(fm.get("group"), Some(&vec!["Security".to_string()]));
        assert_eq!(
            fm.get("include"),
            Some(&vec!["src/**/*.rs".to_string(), "tests/*.ts".to_string()])
        );
        assert_eq!(
            fm.get("exclude"),
            Some(&vec![
                "**/target/**".to_string(),
                "**/generated/**".to_string()
            ])
        );
        assert_eq!(
            fm.get("attach"),
            Some(&vec!["description".to_string(), "story".to_string()])
        );
        assert_eq!(fm.get("model"), Some(&vec!["gpt-5.4".to_string()]));
        assert_eq!(body, "Check auth boundaries cleanly.");
    }

    #[test]
    fn load_phases_from_dir_applies_templates_rejects_traversal_and_missing_placeholder() {
        let root = temp_test_dir("tpl");
        let (phases_dir, templates_dir) = ensure_pr_reviewer_dirs(&root).expect("dirs");

        std::fs::write(
            templates_dir.join("base.md"),
            "HEADER\n<%content%>\nFOOTER",
        )
        .unwrap();
        std::fs::write(templates_dir.join("broken.md"), "NO PLACEHOLDER HERE").unwrap();
        std::fs::write(root.join("secret.md"), "OUTSIDE TEMPLATES <%content%>").unwrap();

        std::fs::write(
            phases_dir.join("01-valid.md"),
            "---\ntitle: Valid Phase\ntemplate: base.md\n---\nInner rules",
        )
        .unwrap();
        std::fs::write(
            phases_dir.join("02-missing-tpl.md"),
            "---\ntemplate: missing.md\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            phases_dir.join("03-traversal.md"),
            "---\ntemplate: ../secret.md\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            phases_dir.join("04-no-placeholder.md"),
            "---\ntemplate: broken.md\n---\nBody",
        )
        .unwrap();

        let loaded = load_phases_from_dir(&root);
        assert_eq!(loaded.len(), 4);

        assert_eq!(loaded[0].title, "Valid Phase");
        assert_eq!(loaded[0].body, "HEADER\nInner rules\nFOOTER");
        assert_eq!(loaded[0].template_error, None);

        assert!(
            loaded[1]
                .template_error
                .as_deref()
                .is_some_and(|e| e.contains("Template file not found"))
        );
        assert!(
            loaded[2]
                .template_error
                .as_deref()
                .is_some_and(|e| e.contains("Directory traversal detected"))
        );
        assert!(
            loaded[3]
                .template_error
                .as_deref()
                .is_some_and(|e| e.contains("missing the required <%content%> placeholder"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_phases_from_dir_sorts_ungrouped_configured_groups_alphabetical_and_filename() {
        let root = temp_test_dir("sort");
        let (phases_dir, _) = ensure_pr_reviewer_dirs(&root).expect("dirs");

        std::fs::write(
            root.join("config.json"),
            r#"{"groups": ["Performance", "Security"]}"#,
        )
        .unwrap();

        std::fs::write(phases_dir.join("z-alpha.md"), "---\ngroup: Alpha\n---\nA").unwrap();
        std::fs::write(phases_dir.join("b-sec.md"), "---\ngroup: Security\n---\nS2").unwrap();
        std::fs::write(phases_dir.join("a-sec.md"), "---\ngroup: Security\n---\nS1").unwrap();
        std::fs::write(phases_dir.join("p-perf.md"), "---\ngroup: Performance\n---\nP").unwrap();
        std::fs::write(phases_dir.join("u-default.md"), "Ungrouped default body").unwrap();

        let loaded = load_phases_from_dir(&root);
        let ids: Vec<&str> = loaded.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "u-default.md",
                "p-perf.md",
                "a-sec.md",
                "b-sec.md",
                "z-alpha.md",
            ]
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
