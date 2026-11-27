use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub const METADATA_FILE_NAME: &str = ".stitch.json";
pub const README_FILE_NAME: &str = "README.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AreaStatus {
    #[default]
    Planned,
    Drafted,
    Finalized,
}

impl AreaStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Planned => "Planned",
            Self::Drafted => "Drafted",
            Self::Finalized => "Finalized",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DesignArea {
    pub id: String,
    pub title: String,
    pub file: String,
    pub summary: String,
    pub status: AreaStatus,
    pub created_at: String,
    pub updated_at: String,
}

impl Default for DesignArea {
    fn default() -> Self {
        let now = now_rfc3339();
        Self {
            id: String::new(),
            title: String::new(),
            file: String::new(),
            summary: String::new(),
            status: AreaStatus::Planned,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DesignProjectMetadata {
    pub version: u32,
    pub name: String,
    pub slug: String,
    pub initial_context: String,
    pub ground_rules_completed: bool,
    pub created_at: String,
    pub updated_at: String,
    pub areas: Vec<DesignArea>,
}

impl Default for DesignProjectMetadata {
    fn default() -> Self {
        let now = now_rfc3339();
        Self {
            version: 1,
            name: String::new(),
            slug: String::new(),
            initial_context: String::new(),
            ground_rules_completed: false,
            created_at: now.clone(),
            updated_at: now,
            areas: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedProject {
    pub dir: PathBuf,
    pub metadata: DesignProjectMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedArea {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub summary: String,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Converts arbitrary text into a lowercase ASCII slug capped at 60 characters.
pub fn slugify(input: &str, fallback: &str) -> String {
    let mut slug = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let trimmed = slug.trim_matches('-');
    let capped: String = trimmed.chars().take(60).collect();
    let final_slug = capped.trim_matches('-');
    if final_slug.is_empty() {
        fallback.to_string()
    } else {
        final_slug.to_string()
    }
}

/// Sanitizes an area markdown filename so it is always a single normal relative `.md` file
/// and never `.stitch.json` or `README.md`.
pub fn sanitize_relative_md_file(raw: &str, fallback_title: &str) -> String {
    let normalized = raw.trim().replace('\\', "/");
    let candidate = Path::new(&normalized)
        .components()
        .rfind(|c| matches!(c, Component::Normal(_)))
        .and_then(|c| c.as_os_str().to_str())
        .map(str::trim)
        .unwrap_or("");

    let cleaned: String = candidate
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let cleaned = cleaned.trim_matches('.');

    let is_reserved = cleaned.is_empty()
        || cleaned.eq_ignore_ascii_case(METADATA_FILE_NAME)
        || cleaned.eq_ignore_ascii_case("stitch.json")
        || cleaned.eq_ignore_ascii_case(README_FILE_NAME);

    if is_reserved {
        return format!("{}.md", slugify(fallback_title, "design-area"));
    }

    if cleaned.to_ascii_lowercase().ends_with(".md") {
        let stem = cleaned[..cleaned.len() - 3].trim_matches('.');
        if stem.is_empty() || stem.eq_ignore_ascii_case("readme") {
            format!("{}.md", slugify(fallback_title, "design-area"))
        } else {
            format!("{stem}.md")
        }
    } else {
        format!("{cleaned}.md")
    }
}

/// Computes the next numbered area filename (e.g. `01-system-architecture.md`)
/// guaranteed unique against existing areas.
pub fn next_area_filename(existing: &[DesignArea], title: &str) -> String {
    let used: HashSet<String> = existing
        .iter()
        .map(|a| a.file.to_ascii_lowercase())
        .collect();
    let base_slug = slugify(title, "design-area");
    let mut index = existing.len() + 1;
    loop {
        let candidate = format!("{index:02}-{base_slug}.md");
        if !used.contains(&candidate.to_ascii_lowercase()) {
            return candidate;
        }
        index += 1;
    }
}

/// Creates a new project subdirectory under `base_dir` and writes `.stitch.json`.
pub fn create_project(
    base_dir: &Path,
    name: &str,
    initial_context: &str,
) -> anyhow::Result<LoadedProject> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        anyhow::bail!("Project name is required.");
    }

    std::fs::create_dir_all(base_dir).with_context(|| {
        format!(
            "Failed to create solution design base directory {}",
            base_dir.display()
        )
    })?;

    let base_slug = slugify(trimmed_name, "solution-design");
    let mut slug = base_slug.clone();
    let mut suffix = 2usize;
    while base_dir.join(&slug).exists() {
        slug = format!("{base_slug}-{suffix}");
        suffix += 1;
    }

    let project_dir = base_dir.join(&slug);
    std::fs::create_dir_all(&project_dir).with_context(|| {
        format!(
            "Failed to create project directory {}",
            project_dir.display()
        )
    })?;

    let now = now_rfc3339();
    let project = LoadedProject {
        dir: project_dir,
        metadata: DesignProjectMetadata {
            version: 1,
            name: trimmed_name.to_string(),
            slug,
            initial_context: initial_context.trim().to_string(),
            ground_rules_completed: false,
            created_at: now.clone(),
            updated_at: now,
            areas: Vec::new(),
        },
    };
    save_project_metadata(&project)?;
    Ok(project)
}

/// Atomically writes `.stitch.json` inside `project.dir`.
pub fn save_project_metadata(project: &LoadedProject) -> anyhow::Result<()> {
    std::fs::create_dir_all(&project.dir).with_context(|| {
        format!(
            "Failed to ensure project directory {}",
            project.dir.display()
        )
    })?;
    let meta_path = project.dir.join(METADATA_FILE_NAME);
    let tmp_path = project.dir.join(".stitch.json.tmp");
    let json = serde_json::to_string_pretty(&project.metadata)
        .context("Failed to serialize .stitch.json metadata")?;
    std::fs::write(&tmp_path, json)
        .with_context(|| format!("Failed to write temporary {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &meta_path)
        .with_context(|| format!("Failed to persist {}", meta_path.display()))?;
    Ok(())
}

/// Lists all valid Stitch solution design projects under `base_dir`, sorted by `updated_at` descending.
pub fn list_projects(base_dir: &Path) -> anyhow::Result<Vec<LoadedProject>> {
    if !base_dir.exists() {
        return Ok(Vec::new());
    }
    if !base_dir.is_dir() {
        anyhow::bail!(
            "Solution design base path is not a directory: {}",
            base_dir.display()
        );
    }

    let mut projects = Vec::new();
    let entries = std::fs::read_dir(base_dir)
        .with_context(|| format!("Failed to read directory {}", base_dir.display()))?;

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join(METADATA_FILE_NAME).is_file() {
            continue;
        }
        match load_project(&path) {
            Ok(project) => projects.push(project),
            Err(err) => {
                eprintln!(
                    "Warning: skipping malformed solution design project at {}: {err}",
                    path.display()
                );
            }
        }
    }

    projects.sort_by(|a, b| b.metadata.updated_at.cmp(&a.metadata.updated_at));
    Ok(projects)
}

/// Loads `.stitch.json` from `project_dir`.
pub fn load_project(project_dir: &Path) -> anyhow::Result<LoadedProject> {
    let meta_path = project_dir.join(METADATA_FILE_NAME);
    let raw = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("Failed to read {}", meta_path.display()))?;
    let metadata: DesignProjectMetadata = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", meta_path.display()))?;
    Ok(LoadedProject {
        dir: project_dir.to_path_buf(),
        metadata,
    })
}

/// Reads a single relative markdown file from `project_dir`, rejecting traversal or `.stitch.json`.
pub fn read_project_file(project_dir: &Path, relative_file: &str) -> anyhow::Result<String> {
    let trimmed = relative_file.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(METADATA_FILE_NAME) {
        anyhow::bail!("Invalid project document filename: {relative_file}");
    }

    let rel_path = Path::new(trimmed);
    let mut components = rel_path.components();
    let Some(Component::Normal(file_name)) = components.next() else {
        anyhow::bail!("Project document must be a single relative filename: {relative_file}");
    };
    if components.next().is_some() {
        anyhow::bail!("Nested or traversal paths are not allowed: {relative_file}");
    }

    let full_path = project_dir.join(file_name);
    std::fs::read_to_string(&full_path)
        .with_context(|| format!("Failed to read {}", full_path.display()))
}

fn extract_heading_or_stem(path: &Path, fallback_stem: &str) -> String {
    if let Ok(content) = std::fs::read_to_string(path)
        && let Some(heading) = content
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with('#'))
    {
        let title = heading.trim_start_matches('#').trim();
        if !title.is_empty() {
            return title.to_string();
        }
    }
    fallback_stem
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reconciles `project.metadata.areas` after a Ground Rules (or README refinement) session,
/// preserving existing area statuses/timestamps and discovering any extra `.md` files written on disk.
pub fn reconcile_ground_rules_areas(
    project: &mut LoadedProject,
    reported: &[ReportedArea],
) -> anyhow::Result<()> {
    let now = now_rfc3339();
    let previous_areas = std::mem::take(&mut project.metadata.areas);
    let mut updated_areas: Vec<DesignArea> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for item in reported {
        let title = item.title.trim();
        let effective_title = if title.is_empty() {
            "Design Area".to_string()
        } else {
            title.to_string()
        };
        let file = sanitize_relative_md_file(&item.file, &effective_title);
        let file_lower = file.to_ascii_lowercase();
        if !seen_files.insert(file_lower.clone()) {
            continue;
        }

        let base_id = item
            .id
            .as_deref()
            .map(|id| slugify(id, "area"))
            .unwrap_or_else(|| slugify(&effective_title, "area"));
        let mut id = base_id.clone();
        let mut idx = 2usize;
        while !seen_ids.insert(id.clone()) {
            id = format!("{base_id}-{idx}");
            idx += 1;
        }

        let existing = previous_areas.iter().find(|prev| {
            prev.file.eq_ignore_ascii_case(&file) || (!prev.id.is_empty() && prev.id == id)
        });

        let (status, created_at, existing_summary) = match existing {
            Some(prev) => (prev.status, prev.created_at.clone(), prev.summary.clone()),
            None => (AreaStatus::Planned, now.clone(), String::new()),
        };

        let summary = {
            let s = item.summary.trim();
            if s.is_empty() {
                existing_summary
            } else {
                s.to_string()
            }
        };

        updated_areas.push(DesignArea {
            id,
            title: effective_title,
            file,
            summary,
            status,
            created_at,
            updated_at: now.clone(),
        });
    }

    // Preserve any previously tracked areas whose files still exist on disk even if omitted from JSONL.
    for prev in previous_areas {
        let file_lower = prev.file.to_ascii_lowercase();
        if seen_files.contains(&file_lower) {
            continue;
        }
        if project.dir.join(&prev.file).is_file() {
            seen_files.insert(file_lower);
            let mut id = prev.id.clone();
            if id.is_empty() || !seen_ids.insert(id.clone()) {
                let base_id = slugify(&prev.title, "area");
                id = base_id.clone();
                let mut idx = 2usize;
                while !seen_ids.insert(id.clone()) {
                    id = format!("{base_id}-{idx}");
                    idx += 1;
                }
            }
            updated_areas.push(DesignArea { id, ..prev });
        }
    }

    // Discover any additional `.md` files created by Copilot in `project.dir` (excluding README.md).
    if let Ok(entries) = std::fs::read_dir(&project.dir) {
        let mut extra_files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            })
            .collect();
        extra_files.sort();

        for path in extra_files {
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if file_name.eq_ignore_ascii_case(README_FILE_NAME) {
                continue;
            }
            let file_lower = file_name.to_ascii_lowercase();
            if !seen_files.insert(file_lower) {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("design-area");
            let title = extract_heading_or_stem(&path, stem);
            let base_id = slugify(&title, "area");
            let mut id = base_id.clone();
            let mut idx = 2usize;
            while !seen_ids.insert(id.clone()) {
                id = format!("{base_id}-{idx}");
                idx += 1;
            }
            updated_areas.push(DesignArea {
                id,
                title,
                file: file_name.to_string(),
                summary: String::new(),
                status: AreaStatus::Planned,
                created_at: now.clone(),
                updated_at: now.clone(),
            });
        }
    }

    project.metadata.ground_rules_completed = true;
    project.metadata.updated_at = now;
    project.metadata.areas = updated_areas;
    save_project_metadata(project)?;
    Ok(())
}

/// Records completion of an area design, new area, or area refinement session and persists `.stitch.json`.
pub fn record_area_completed(
    project: &mut LoadedProject,
    existing_area_id: Option<&str>,
    fallback_title: &str,
    reported_file: Option<&str>,
    summary: Option<&str>,
    preserve_finalized: bool,
) -> anyhow::Result<DesignArea> {
    let now = now_rfc3339();
    let sanitized_reported = reported_file
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw| sanitize_relative_md_file(raw, fallback_title));

    let matched_idx = project.metadata.areas.iter().position(|area| {
        existing_area_id.is_some_and(|id| area.id == id)
            || sanitized_reported
                .as_deref()
                .is_some_and(|f| area.file.eq_ignore_ascii_case(f))
    });

    let result_area = if let Some(idx) = matched_idx {
        let area = &mut project.metadata.areas[idx];
        if let Some(new_file) = sanitized_reported
            && project.dir.join(&new_file).is_file()
        {
            area.file = new_file;
        }
        if let Some(s) = summary.map(str::trim).filter(|s| !s.is_empty()) {
            area.summary = s.to_string();
        }
        if !(preserve_finalized && area.status == AreaStatus::Finalized) {
            area.status = AreaStatus::Drafted;
        }
        area.updated_at = now.clone();
        area.clone()
    } else {
        let title = {
            let t = fallback_title.trim();
            if t.is_empty() {
                "Design Area".to_string()
            } else {
                t.to_string()
            }
        };
        let file = sanitized_reported
            .unwrap_or_else(|| next_area_filename(&project.metadata.areas, &title));
        let base_id = slugify(&title, "area");
        let mut id = base_id.clone();
        let mut idx = 2usize;
        while project.metadata.areas.iter().any(|a| a.id == id) {
            id = format!("{base_id}-{idx}");
            idx += 1;
        }
        let area = DesignArea {
            id,
            title,
            file,
            summary: summary.map(str::trim).unwrap_or("").to_string(),
            status: AreaStatus::Drafted,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        project.metadata.areas.push(area.clone());
        area
    };

    project.metadata.updated_at = now;
    save_project_metadata(project)?;
    Ok(result_area)
}

/// Updates an area's status (e.g. `Drafted` <-> `Finalized`) and persists `.stitch.json`.
pub fn set_area_status(
    project: &mut LoadedProject,
    area_id: &str,
    status: AreaStatus,
) -> anyhow::Result<()> {
    let now = now_rfc3339();
    let Some(area) = project
        .metadata
        .areas
        .iter_mut()
        .find(|a| a.id == area_id)
    else {
        anyhow::bail!("Area '{area_id}' not found in project metadata.");
    };
    area.status = status;
    area.updated_at = now.clone();
    project.metadata.updated_at = now;
    save_project_metadata(project)?;
    Ok(())
}

/// Opens a directory in the OS file manager (Finder / Explorer / xdg-open).
pub fn open_directory_in_os(dir: &Path) -> anyhow::Result<()> {
    if !dir.is_dir() {
        anyhow::bail!("Directory does not exist: {}", dir.display());
    }

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(dir);
        c
    };

    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(dir);
        c
    };

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(dir);
        c
    };

    cmd.spawn()
        .with_context(|| format!("Failed to open directory {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("stitch-sd-test-{name}-{nanos}"))
    }

    #[test]
    fn create_project_handles_slug_collisions_and_round_trips() {
        let base = temp_dir("create");
        let p1 = create_project(&base, "Payments Platform!", "Build a ledger").expect("p1");
        assert_eq!(p1.metadata.slug, "payments-platform");
        assert!(p1.dir.join(METADATA_FILE_NAME).is_file());

        let p2 = create_project(&base, "Payments Platform", "Second one").expect("p2");
        assert_eq!(p2.metadata.slug, "payments-platform-2");

        let loaded = load_project(&p1.dir).expect("load p1");
        assert_eq!(loaded.metadata.name, "Payments Platform!");
        assert_eq!(loaded.metadata.initial_context, "Build a ledger");
        assert!(!loaded.metadata.ground_rules_completed);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn list_projects_sorts_by_updated_desc_and_skips_malformed_dirs() {
        let base = temp_dir("list");
        assert!(list_projects(&base).expect("missing base").is_empty());

        let mut p1 = create_project(&base, "Older Project", "ctx").expect("p1");
        p1.metadata.updated_at = "2026-01-01T00:00:00Z".to_string();
        save_project_metadata(&p1).expect("save p1");

        let mut p2 = create_project(&base, "Newer Project", "ctx").expect("p2");
        p2.metadata.updated_at = "2026-02-01T00:00:00Z".to_string();
        save_project_metadata(&p2).expect("save p2");

        let broken = base.join("broken-dir");
        std::fs::create_dir_all(&broken).expect("broken dir");
        std::fs::write(broken.join(METADATA_FILE_NAME), "{not valid json").expect("broken meta");

        let listed = list_projects(&base).expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].metadata.name, "Newer Project");
        assert_eq!(listed[1].metadata.name, "Older Project");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sanitize_relative_md_file_rejects_traversal_and_reserved_names() {
        assert_eq!(
            sanitize_relative_md_file("../../etc/passwd", "Auth Flow"),
            "passwd.md"
        );
        assert_eq!(
            sanitize_relative_md_file(".stitch.json", "Auth Flow"),
            "auth-flow.md"
        );
        assert_eq!(
            sanitize_relative_md_file("README.md", "Auth Flow"),
            "auth-flow.md"
        );
        assert_eq!(
            sanitize_relative_md_file("01-api-gateway.md", "API Gateway"),
            "01-api-gateway.md"
        );
        assert_eq!(
            sanitize_relative_md_file("02-data-model", "Data Model"),
            "02-data-model.md"
        );
    }

    #[test]
    fn read_project_file_rejects_traversal_and_metadata() {
        let base = temp_dir("read");
        let p = create_project(&base, "Demo", "ctx").expect("project");
        std::fs::write(p.dir.join("README.md"), "# Demo").expect("write readme");

        assert_eq!(
            read_project_file(&p.dir, "README.md").expect("read readme"),
            "# Demo"
        );
        assert!(read_project_file(&p.dir, ".stitch.json").is_err());
        assert!(read_project_file(&p.dir, "../secret.md").is_err());
        assert!(read_project_file(&p.dir, "sub/nested.md").is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reconcile_ground_rules_merges_existing_and_discovers_unreported_md_files() {
        let base = temp_dir("reconcile");
        let mut p = create_project(&base, "Platform", "ctx").expect("project");
        p.metadata.areas.push(DesignArea {
            id: "auth".to_string(),
            title: "Auth".to_string(),
            file: "01-auth.md".to_string(),
            summary: "Old summary".to_string(),
            status: AreaStatus::Drafted,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        });

        std::fs::write(p.dir.join("README.md"), "# Platform").expect("readme");
        std::fs::write(p.dir.join("01-auth.md"), "# Auth").expect("auth");
        std::fs::write(p.dir.join("02-storage.md"), "# Storage Layer\n\nStub").expect("storage");

        let reported = vec![ReportedArea {
            id: Some("auth".to_string()),
            title: "Authentication & AuthZ".to_string(),
            file: "01-auth.md".to_string(),
            summary: "New summary".to_string(),
        }];

        reconcile_ground_rules_areas(&mut p, &reported).expect("reconcile");
        assert!(p.metadata.ground_rules_completed);
        assert_eq!(p.metadata.areas.len(), 2);
        assert_eq!(p.metadata.areas[0].status, AreaStatus::Drafted);
        assert_eq!(p.metadata.areas[0].summary, "New summary");
        assert_eq!(p.metadata.areas[1].title, "Storage Layer");
        assert_eq!(p.metadata.areas[1].file, "02-storage.md");
        assert_eq!(p.metadata.areas[1].status, AreaStatus::Planned);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn record_area_completed_and_set_status_update_and_insert() {
        let base = temp_dir("record-area");
        let mut p = create_project(&base, "Orders", "ctx").expect("project");
        std::fs::write(p.dir.join("01-checkout.md"), "# Checkout").expect("checkout");

        let created = record_area_completed(
            &mut p,
            None,
            "Checkout Flow",
            Some("01-checkout.md"),
            Some("Handles cart to order transition."),
            false,
        )
        .expect("insert area");
        assert_eq!(created.status, AreaStatus::Drafted);
        assert_eq!(p.metadata.areas.len(), 1);

        set_area_status(&mut p, &created.id, AreaStatus::Finalized).expect("finalize");
        assert_eq!(p.metadata.areas[0].status, AreaStatus::Finalized);

        // Refining with preserve_finalized = true keeps Finalized status.
        let refined = record_area_completed(
            &mut p,
            Some(&created.id),
            "Checkout Flow",
            Some("01-checkout.md"),
            None,
            true,
        )
        .expect("refine");
        assert_eq!(refined.status, AreaStatus::Finalized);

        // Re-designing with preserve_finalized = false resets to Drafted.
        let redesigned = record_area_completed(
            &mut p,
            Some(&created.id),
            "Checkout Flow",
            Some("01-checkout.md"),
            None,
            false,
        )
        .expect("redesign");
        assert_eq!(redesigned.status, AreaStatus::Drafted);

        let _ = std::fs::remove_dir_all(&base);
    }
}
