use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinSet;

use crate::features::pr_reviewer::phase_loader::{
    load_config_from_dir, load_phases_from_disk, pr_reviewer_root_dir,
};
use crate::features::pr_reviewer::prompts::{build_critic_prompt, build_phase_review_prompt};
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, create_report_intent_tool,
    create_request_documentation_tool, create_view_existing_comments_tool, resolve_model_override,
};
use crate::infrastructure::git::GitService;
use crate::infrastructure::providers::{
    build_code_review_provider, build_documentation_provider, build_issue_tracker_provider,
    discover_known_docs,
};
use crate::infrastructure::sleep_guard::SleepGuard;
use crate::types::{
    AppSettings, CodeContextLine, CopilotModelInfo, CopilotUsage, KnownDoc, PhaseUsage,
    PostPrCommentRequest, PrDiffFile, PrHostType, PrMetadata, ReviewComment, ReviewCommentStatus,
    ReviewCommentType, ReviewPhase, TicketData, resolve_effective_parallelism, system_cpu_count,
};

const MAX_CONTEXT_LINES: usize = 25;

/// Normalized repository path resolved from a user-selected directory or subdirectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRepoPath {
    pub path: PathBuf,
    pub was_modified: bool,
}

/// Output of checking out a PR branch or detached worktree and computing its changed files.
#[derive(Debug, Clone)]
pub struct CheckoutResult {
    pub commit_sha: String,
    pub effective_repo_path: PathBuf,
    pub diff_files: Vec<PrDiffFile>,
    pub using_worktree: bool,
}

#[derive(Debug, Clone)]
pub enum CheckoutError {
    DirtyRepo(String),
    Cancelled,
    Other(String),
}

impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirtyRepo(msg) | Self::Other(msg) => f.write_str(msg),
            Self::Cancelled => f.write_str("Checkout cancelled."),
        }
    }
}

/// Parameters for executing a parallel multi-phase PR code review.
#[derive(Debug, Clone)]
pub struct PrReviewRequest {
    pub repo_path: PathBuf,
    pub pr: PrMetadata,
    pub enabled_phase_ids: Vec<String>,
    pub model_override: String,
    pub custom_instructions: String,
    pub max_parallelism: Option<usize>,
    pub persona_name: String,
    pub settings: AppSettings,
    pub available_models: Vec<CopilotModelInfo>,
    pub skip_cleanup: bool,
    pub prefetched_diff_files: Option<Vec<PrDiffFile>>,
}

/// Streaming progress, draft comment, and completion events from `PrReviewerService::review_pr`.
#[derive(Debug, Clone)]
pub enum PrReviewStreamEvent {
    PhaseSkipped {
        phase_id: String,
        phase_title: String,
        reason: String,
    },
    PhaseStarted {
        phase_id: String,
        phase_title: String,
    },
    PhaseStatus {
        phase_id: String,
        phase_title: String,
        status: String,
    },
    Comment(ReviewComment),
    PhaseCompleted {
        phase_id: String,
        phase_title: String,
        comment_count: usize,
    },
    Finished {
        comments: Vec<ReviewComment>,
        usage: CopilotUsage,
        cleaned_up: bool,
    },
    Cancelled {
        cleaned_up: bool,
    },
    Error {
        message: String,
        cleaned_up: bool,
    },
}

/// Parameters for executing the AI Critic evaluation pass over draft review comments.
#[derive(Debug, Clone)]
pub struct PrCriticRequest {
    pub repo_path: PathBuf,
    pub pr: PrMetadata,
    pub pr_url_or_id: String,
    pub comments: Vec<ReviewComment>,
    pub model_override: String,
    pub persona_name: String,
    pub settings: AppSettings,
    pub available_models: Vec<CopilotModelInfo>,
}

/// Streaming status and completion events from `PrReviewerService::critique_comments`.
#[derive(Debug, Clone)]
pub enum PrCriticStreamEvent {
    Status(String),
    Finished {
        comments: Vec<ReviewComment>,
        usage: CopilotUsage,
        cleaned_up: bool,
    },
    Cancelled {
        cleaned_up: bool,
    },
    Error {
        message: String,
        cleaned_up: bool,
    },
}

/// Result of parsing a single streamed JSONL line from a PR review phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedReviewLine {
    Status(String),
    Comment(ReviewComment),
}

/// Verifies `selected` is inside a Git repository and normalizes it to the repository top-level root.
pub async fn verify_repo_path(git: &GitService, selected: &Path) -> VerifiedRepoPath {
    if git.check_git_repo(selected).await {
        if let Ok(root) = git.get_repo_root(selected).await {
            let differs = match (
                std::fs::canonicalize(selected),
                std::fs::canonicalize(&root),
            ) {
                (Ok(canon_sel), Ok(canon_root)) => canon_sel != canon_root,
                _ => selected != root,
            };
            return VerifiedRepoPath {
                path: root,
                was_modified: differs,
            };
        }
    }
    VerifiedRepoPath {
        path: selected.to_path_buf(),
        was_modified: false,
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn build_glob_set(patterns: &[String], label: &str) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut added = 0usize;
    for raw in patterns {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        match GlobBuilder::new(trimmed).literal_separator(true).build() {
            Ok(glob) => {
                builder.add(glob);
                added += 1;
            }
            Err(err) => {
                eprintln!("Warning: invalid {label} glob '{trimmed}': {err}");
            }
        }
    }
    if added == 0 {
        return None;
    }
    match builder.build() {
        Ok(set) => Some(set),
        Err(err) => {
            eprintln!("Warning: failed to compile {label} globset: {err}");
            None
        }
    }
}

/// Filters diff files using optional `include` and `exclude` glob patterns (matching dotfiles).
pub fn filter_phase_files(
    files: &[PrDiffFile],
    include: &[String],
    exclude: &[String],
) -> Vec<PrDiffFile> {
    let include_set = build_glob_set(include, "include");
    let exclude_set = build_glob_set(exclude, "exclude");

    files
        .iter()
        .filter(|file| {
            let normalized = file.path.replace('\\', "/");
            if let Some(ref inc) = include_set {
                if !inc.is_match(&normalized) {
                    return false;
                }
            }
            if let Some(ref exc) = exclude_set {
                if exc.is_match(&normalized) {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// Safely reads `[target_line - context_size ..= target_line + context_size]` from a repository file,
/// blocking path traversal and leading-root paths.
pub fn extract_file_context(
    repo_path: &Path,
    file_path: &str,
    target_line: usize,
    context_size: usize,
) -> Option<Vec<CodeContextLine>> {
    if target_line == 0 {
        return None;
    }

    let normalized = file_path.trim().replace('\\', "/");
    let stripped = normalized.trim_start_matches("./").trim_start_matches('/');
    if stripped.is_empty() {
        return None;
    }

    let rel = Path::new(stripped);
    for comp in rel.components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return None;
        }
    }

    let full_path = repo_path.join(rel);
    let canon_repo = std::fs::canonicalize(repo_path).ok()?;
    let canon_full = std::fs::canonicalize(&full_path).ok()?;
    if !canon_full.starts_with(&canon_repo) {
        return None;
    }

    let content = std::fs::read_to_string(&canon_full).ok()?;
    let all_lines: Vec<&str> = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    if all_lines.is_empty() {
        return None;
    }

    let clamped_context = context_size.min(MAX_CONTEXT_LINES);
    let start_line = target_line.saturating_sub(clamped_context).max(1);
    let end_line = target_line
        .saturating_add(clamped_context)
        .min(all_lines.len());
    if start_line > end_line {
        return None;
    }

    Some(
        (start_line..=end_line)
            .map(|ln| CodeContextLine {
                line: ln,
                text: all_lines[ln - 1].to_string(),
                is_target: ln == target_line,
            })
            .collect(),
    )
}

/// Parses a streamed JSONL object emitted during a PR review phase and enriches line comments with code context.
pub fn parse_review_jsonl_line(
    line: &str,
    phase_id: &str,
    phase_title: &str,
    comment_id: &str,
    effective_repo_path: &Path,
) -> Option<ParsedReviewLine> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }

    let val: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let kind = val.get("type").and_then(|v| v.as_str())?.trim();

    match kind {
        "status" => {
            let status_text = val
                .get("status")
                .or_else(|| val.get("text"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            Some(ParsedReviewLine::Status(status_text.to_string()))
        }
        "general" => {
            let comment = val
                .get("comment")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .to_string();
            Some(ParsedReviewLine::Comment(ReviewComment {
                id: comment_id.to_string(),
                comment_type: ReviewCommentType::General,
                file: None,
                line: None,
                context: None,
                comment,
                phase: Some(phase_title.to_string()),
                phase_id: Some(phase_id.to_string()),
                code_lines: None,
                posted: false,
                status: None,
                reason: None,
                thread_id: None,
                merged_from_indices: None,
            }))
        }
        "line" => {
            let comment = val
                .get("comment")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .to_string();
            let file = val
                .get("file")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let target_line = val
                .get("line")
                .and_then(|v| v.as_u64())
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| n >= 1);
            let context_size = val
                .get("context")
                .and_then(|v| v.as_u64())
                .and_then(|n| usize::try_from(n).ok());

            let code_lines = match (file.as_deref(), target_line) {
                (Some(f), Some(ln)) => {
                    extract_file_context(effective_repo_path, f, ln, context_size.unwrap_or(0))
                }
                _ => None,
            };

            Some(ParsedReviewLine::Comment(ReviewComment {
                id: comment_id.to_string(),
                comment_type: ReviewCommentType::Line,
                file,
                line: target_line,
                context: context_size,
                comment,
                phase: Some(phase_title.to_string()),
                phase_id: Some(phase_id.to_string()),
                code_lines,
                posted: false,
                status: None,
                reason: None,
                thread_id: None,
                merged_from_indices: None,
            }))
        }
        _ => None,
    }
}

fn parse_1_based_indices(val: &serde_json::Value, max_len: usize) -> Vec<usize> {
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_u64())
        .filter(|&n| n >= 1)
        .filter_map(|n| usize::try_from(n - 1).ok())
        .filter(|&idx| idx < max_len)
        .collect()
}

/// Applies streamed JSONL Critic decisions (`approve`, `edit`, `reject`, `merge`, `reply`)
/// to the raw draft comments, re-extracting `code_lines` when line comments are relocated
/// and defaulting any unhandled comments to `Approved`.
pub fn apply_critic_decisions(
    original_comments: &[ReviewComment],
    decision_lines: &[String],
    effective_repo_path: Option<&Path>,
) -> Vec<ReviewComment> {
    if original_comments.is_empty() {
        return Vec::new();
    }

    let mut handled_original_indices: HashSet<usize> = HashSet::new();
    let mut critiqued: Vec<ReviewComment> = Vec::new();
    let mut seq = 0usize;
    let mut next_id = || {
        seq += 1;
        format!("critic-{seq}")
    };

    for raw_line in decision_lines {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(action) = obj.get("action").and_then(|v| v.as_str()).map(str::trim) else {
            continue;
        };

        match action {
            "approve" => {
                let Some(idx) = obj
                    .get("commentIndex")
                    .and_then(|v| v.as_u64())
                    .filter(|&n| n >= 1)
                    .and_then(|n| usize::try_from(n - 1).ok())
                    .filter(|&i| i < original_comments.len())
                else {
                    continue;
                };
                if handled_original_indices.insert(idx) {
                    let mut c = original_comments[idx].clone();
                    c.id = next_id();
                    c.status = Some(ReviewCommentStatus::Approved);
                    critiqued.push(c);
                }
            }
            "edit" => {
                let Some(idx) = obj
                    .get("commentIndex")
                    .and_then(|v| v.as_u64())
                    .filter(|&n| n >= 1)
                    .and_then(|n| usize::try_from(n - 1).ok())
                    .filter(|&i| i < original_comments.len())
                else {
                    continue;
                };
                let Some(updated_comment) = obj
                    .get("comment")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    continue;
                };
                if handled_original_indices.insert(idx) {
                    let original = &original_comments[idx];
                    let target_type = match obj.get("type").and_then(|v| v.as_str()).map(str::trim)
                    {
                        Some("general") => ReviewCommentType::General,
                        Some("line") => ReviewCommentType::Line,
                        _ => original.comment_type,
                    };
                    let is_general = target_type == ReviewCommentType::General;
                    let target_file = if is_general {
                        None
                    } else {
                        obj.get("file")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .or_else(|| original.file.clone())
                    };
                    let target_line = if is_general {
                        None
                    } else {
                        obj.get("line")
                            .and_then(|v| v.as_u64())
                            .and_then(|n| usize::try_from(n).ok())
                            .filter(|&n| n >= 1)
                            .or(original.line)
                    };

                    let mut code_lines = if is_general {
                        None
                    } else {
                        original.code_lines.clone()
                    };

                    if !is_general {
                        if let (Some(repo), Some(tf), Some(tl)) =
                            (effective_repo_path, target_file.as_deref(), target_line)
                        {
                            if target_file != original.file || target_line != original.line {
                                let ctx_size = original.context.unwrap_or(5);
                                if let Some(extracted) =
                                    extract_file_context(repo, tf, tl, ctx_size)
                                {
                                    code_lines = Some(extracted);
                                }
                            }
                        }
                    }

                    critiqued.push(ReviewComment {
                        id: next_id(),
                        comment_type: target_type,
                        file: target_file,
                        line: target_line,
                        context: if is_general { None } else { original.context },
                        comment: updated_comment,
                        phase: original.phase.clone(),
                        phase_id: original.phase_id.clone(),
                        code_lines,
                        posted: false,
                        status: Some(ReviewCommentStatus::Edited),
                        reason: None,
                        thread_id: None,
                        merged_from_indices: None,
                    });
                }
            }
            "reject" => {
                let Some(idx) = obj
                    .get("commentIndex")
                    .and_then(|v| v.as_u64())
                    .filter(|&n| n >= 1)
                    .and_then(|n| usize::try_from(n - 1).ok())
                    .filter(|&i| i < original_comments.len())
                else {
                    continue;
                };
                if handled_original_indices.insert(idx) {
                    let reason = obj
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Rejected by Critic")
                        .to_string();
                    let mut c = original_comments[idx].clone();
                    c.id = next_id();
                    c.status = Some(ReviewCommentStatus::Rejected);
                    c.reason = Some(reason);
                    critiqued.push(c);
                }
            }
            "merge" => {
                let Some(indices_val) = obj.get("commentIndices") else {
                    continue;
                };
                let Some(merged_comment) = obj
                    .get("comment")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    continue;
                };
                let indices = parse_1_based_indices(indices_val, original_comments.len());
                if indices.is_empty() {
                    continue;
                }
                let unhandled: Vec<usize> = indices
                    .iter()
                    .copied()
                    .filter(|i| !handled_original_indices.contains(i))
                    .collect();
                if unhandled.is_empty() {
                    continue;
                }
                for i in unhandled {
                    handled_original_indices.insert(i);
                }

                let first = &original_comments[indices[0]];
                let explicit_file = obj
                    .get("file")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let target_type = match obj.get("type").and_then(|v| v.as_str()).map(str::trim) {
                    Some("general") => ReviewCommentType::General,
                    Some("line") => ReviewCommentType::Line,
                    _ => {
                        if explicit_file.is_some() {
                            ReviewCommentType::Line
                        } else {
                            first.comment_type
                        }
                    }
                };
                let is_general = target_type == ReviewCommentType::General;
                let target_file = if is_general {
                    None
                } else {
                    explicit_file.or_else(|| first.file.clone())
                };
                let target_line = if is_general {
                    None
                } else {
                    obj.get("line")
                        .and_then(|v| v.as_u64())
                        .and_then(|n| usize::try_from(n).ok())
                        .filter(|&n| n >= 1)
                        .or(first.line)
                };

                let mut code_lines = if is_general {
                    None
                } else {
                    first.code_lines.clone()
                };

                if !is_general {
                    if let (Some(repo), Some(tf), Some(tl)) =
                        (effective_repo_path, target_file.as_deref(), target_line)
                    {
                        if target_file != first.file || target_line != first.line {
                            let ctx_size = first.context.unwrap_or(5);
                            if let Some(extracted) = extract_file_context(repo, tf, tl, ctx_size) {
                                code_lines = Some(extracted);
                            }
                        }
                    }
                }

                critiqued.push(ReviewComment {
                    id: next_id(),
                    comment_type: target_type,
                    file: target_file,
                    line: target_line,
                    context: if is_general { None } else { first.context },
                    comment: merged_comment,
                    phase: first.phase.clone(),
                    phase_id: first.phase_id.clone(),
                    code_lines,
                    posted: false,
                    status: Some(ReviewCommentStatus::Merged),
                    reason: None,
                    thread_id: None,
                    merged_from_indices: Some(indices),
                });
            }
            "reply" => {
                let Some(thread_id) = obj.get("threadId").and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                }) else {
                    continue;
                };
                let custom_reply_text = obj
                    .get("comment")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);

                if let Some(idx) = obj
                    .get("commentIndex")
                    .and_then(|v| v.as_u64())
                    .filter(|&n| n >= 1)
                    .and_then(|n| usize::try_from(n - 1).ok())
                    .filter(|&i| i < original_comments.len())
                {
                    if handled_original_indices.insert(idx) {
                        let mut c = original_comments[idx].clone();
                        c.id = next_id();
                        if let Some(text) = custom_reply_text {
                            c.comment = text;
                        }
                        c.status = Some(ReviewCommentStatus::Reply);
                        c.thread_id = Some(thread_id);
                        critiqued.push(c);
                    }
                } else if let Some(indices_val) = obj.get("commentIndices") {
                    let indices = parse_1_based_indices(indices_val, original_comments.len());
                    if indices.is_empty() {
                        continue;
                    }
                    let unhandled: Vec<usize> = indices
                        .iter()
                        .copied()
                        .filter(|i| !handled_original_indices.contains(i))
                        .collect();
                    if unhandled.is_empty() {
                        continue;
                    }
                    for i in unhandled {
                        handled_original_indices.insert(i);
                    }
                    let mut c = original_comments[indices[0]].clone();
                    c.id = next_id();
                    if let Some(text) = custom_reply_text {
                        c.comment = text;
                    }
                    c.status = Some(ReviewCommentStatus::Reply);
                    c.thread_id = Some(thread_id);
                    c.merged_from_indices = Some(indices);
                    critiqued.push(c);
                }
            }
            _ => {}
        }
    }

    for (idx, original) in original_comments.iter().enumerate() {
        if !handled_original_indices.contains(&idx) {
            let mut c = original.clone();
            c.id = next_id();
            c.status = Some(ReviewCommentStatus::Approved);
            critiqued.push(c);
        }
    }

    critiqued
}

fn phase_attaches(phase: &ReviewPhase, target: &str) -> bool {
    phase
        .attach
        .iter()
        .any(|item| item.trim().eq_ignore_ascii_case(target))
}

fn resolve_model_display_name(raw_model: &str, available_models: &[CopilotModelInfo]) -> String {
    let trimmed = raw_model.trim();
    if trimmed.is_empty() {
        return "Unknown".to_string();
    }
    available_models
        .iter()
        .find(|m| {
            m.id.eq_ignore_ascii_case(trimmed) || m.name.eq_ignore_ascii_case(trimmed)
        })
        .map(|m| m.name.clone())
        .unwrap_or_else(|| trimmed.to_string())
}

/// Manages PR repository checkout, worktree isolation, parallel phase reviews, and comment posting.
pub struct PrReviewerService {
    git: Arc<GitService>,
    copilot: Arc<CopilotService>,
    active_checkouts: Mutex<HashMap<PathBuf, String>>,
    active_worktrees: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl PrReviewerService {
    pub fn new(git: Arc<GitService>, copilot: Arc<CopilotService>) -> Self {
        Self {
            git,
            copilot,
            active_checkouts: Mutex::new(HashMap::new()),
            active_worktrees: Mutex::new(HashMap::new()),
        }
    }

    pub async fn effective_repo_path(&self, repo_path: &Path) -> PathBuf {
        let repo_root = self
            .git
            .get_repo_root(repo_path)
            .await
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let repo_key = canonical_or_raw(&repo_root);
        let map = self.active_worktrees.lock().await;
        map.get(&repo_key)
            .or_else(|| map.get(repo_path))
            .or_else(|| map.get(&repo_root))
            .cloned()
            .unwrap_or(repo_root)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn checkout_pr_with_progress<F>(
        &self,
        repo_path: &Path,
        pr_number: u64,
        target_branch: &str,
        expected_repo_name: Option<&str>,
        code_provider: Option<&dyn crate::infrastructure::providers::CodeReviewProvider>,
        settings: &AppSettings,
        cancel_rx: Option<watch::Receiver<bool>>,
        on_status: F,
    ) -> Result<CheckoutResult, CheckoutError>
    where
        F: Fn(String) + Send + Sync,
    {
        let is_cancelled = || cancel_rx.as_ref().is_some_and(|rx| *rx.borrow());
        if is_cancelled() {
            return Err(CheckoutError::Cancelled);
        }

        on_status("Verifying Git repository...".to_string());
        if !self.git.check_git_repo(repo_path).await {
            return Err(CheckoutError::Other(
                "The selected path is not a valid Git repository.".to_string(),
            ));
        }

        let repo_root = self
            .git
            .get_repo_root(repo_path)
            .await
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let repo_key = canonical_or_raw(&repo_root);

        if let Some(expected) = expected_repo_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            on_status("Checking repository remote...".to_string());
            if let (Some(provider), Some(remote_url)) =
                (code_provider, self.git.get_remote_url(&repo_root).await)
            {
                if let Some(parsed_remote) = provider.parse_remote_url(&remote_url) {
                    if !parsed_remote.repo_name.eq_ignore_ascii_case(expected) {
                        return Err(CheckoutError::Other(format!(
                            "The selected repository \"{}\" does not match the Pull Request repository \"{}\".",
                            parsed_remote.repo_name, expected
                        )));
                    }
                }
            }
        }

        // Restore any previously checked-out ref or remove any stale worktree for this repo first.
        if let Err(err) = self.cleanup_checkout(&repo_root).await {
            eprintln!("Warning: cleanup before PR checkout failed: {err}");
        }

        if is_cancelled() {
            return Err(CheckoutError::Cancelled);
        }

        if settings.git_worktree_enabled {
            if let Some(base_dir) = settings.git_worktree_base_dir.clone() {
                if !base_dir.exists() {
                    std::fs::create_dir_all(&base_dir).map_err(|err| {
                        CheckoutError::Other(format!(
                            "Failed to create Git worktree base directory {}: {err}",
                            base_dir.display()
                        ))
                    })?;
                }

                on_status(format!("Fetching PR #{pr_number} from origin..."));
                let commit_sha = self
                    .git
                    .fetch_pr(&repo_root, pr_number)
                    .await
                    .map_err(|err| CheckoutError::Other(err.to_string()))?;

                if is_cancelled() {
                    return Err(CheckoutError::Cancelled);
                }

                let repo_slug = expected_repo_name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|name| GitService::sanitize_name(name, "repo"))
                    .unwrap_or_else(|| GitService::sanitize_repo_name(&repo_root));

                let worktree_name = format!("{repo_slug}_pr_{pr_number}");
                let worktree_path = base_dir.join(worktree_name);

                on_status("Creating temporary Git worktree...".to_string());
                self.git
                    .add_worktree(&repo_root, &worktree_path, &commit_sha)
                    .await
                    .map_err(|err| CheckoutError::Other(err.to_string()))?;

                {
                    let mut map = self.active_worktrees.lock().await;
                    map.insert(repo_key, worktree_path.clone());
                }

                if is_cancelled() {
                    let _ = self.cleanup_checkout(&repo_root).await;
                    return Err(CheckoutError::Cancelled);
                }

                on_status("Computing changed files...".to_string());
                let diff_files = match self.git.get_diff_files(&worktree_path, target_branch).await {
                    Ok(files) => files,
                    Err(err) => {
                        let _ = self.cleanup_checkout(&repo_root).await;
                        return Err(CheckoutError::Other(err.to_string()));
                    }
                };

                return Ok(CheckoutResult {
                    commit_sha,
                    effective_repo_path: worktree_path,
                    diff_files,
                    using_worktree: true,
                });
            }
        }

        on_status("Checking working tree status...".to_string());
        if self.git.has_uncommitted_changes(&repo_root).await {
            return Err(CheckoutError::DirtyRepo(
                "The local git repository has uncommitted changes. Please commit, stash, or revert them first."
                    .to_string(),
            ));
        }

        let original_ref = self
            .git
            .get_current_ref(&repo_root)
            .await
            .map_err(|err| CheckoutError::Other(err.to_string()))?;

        if is_cancelled() {
            return Err(CheckoutError::Cancelled);
        }

        on_status(format!("Fetching & checking out PR #{pr_number}..."));
        let commit_sha = self
            .git
            .fetch_and_checkout_pr(&repo_root, pr_number)
            .await
            .map_err(|err| CheckoutError::Other(err.to_string()))?;

        {
            let mut map = self.active_checkouts.lock().await;
            map.insert(repo_key, original_ref);
        }

        if is_cancelled() {
            let _ = self.cleanup_checkout(&repo_root).await;
            return Err(CheckoutError::Cancelled);
        }

        on_status("Computing changed files...".to_string());
        let diff_files = match self.git.get_diff_files(&repo_root, target_branch).await {
            Ok(files) => files,
            Err(err) => {
                let _ = self.cleanup_checkout(&repo_root).await;
                return Err(CheckoutError::Other(err.to_string()));
            }
        };

        Ok(CheckoutResult {
            commit_sha,
            effective_repo_path: repo_root,
            diff_files,
            using_worktree: false,
        })
    }

    pub async fn cleanup_checkout(&self, repo_path: &Path) -> anyhow::Result<()> {
        let repo_root = self
            .git
            .get_repo_root(repo_path)
            .await
            .unwrap_or_else(|_| repo_path.to_path_buf());
        let repo_key = canonical_or_raw(&repo_root);

        let wt = {
            let mut map = self.active_worktrees.lock().await;
            map.remove(&repo_key)
                .or_else(|| map.remove(repo_path))
                .or_else(|| map.remove(&repo_root))
        };
        if let Some(worktree_path) = wt {
            self.git
                .remove_worktree(Some(&repo_root), &worktree_path)
                .await;
        }

        let prev_ref = {
            let mut map = self.active_checkouts.lock().await;
            map.remove(&repo_key)
                .or_else(|| map.remove(repo_path))
                .or_else(|| map.remove(&repo_root))
        };
        if let Some(original_ref) = prev_ref {
            if let Err(err) = self.git.restore_ref(&repo_root, &original_ref).await {
                // Re-insert so the user can retry restoring once changes are cleaned up.
                let mut map = self.active_checkouts.lock().await;
                map.insert(repo_key, original_ref);
                return Err(err);
            }
        }

        Ok(())
    }

    async fn resolve_story_context(
        &self,
        req: &PrReviewRequest,
        effective_repo_path: &Path,
        doc_provider: Option<&Arc<dyn crate::infrastructure::providers::DocumentationProvider>>,
    ) -> (String, Vec<KnownDoc>) {
        let Ok(code_provider) = build_code_review_provider(&req.settings) else {
            return (String::new(), Vec::new());
        };

        let remote_url = self.git.get_remote_url(effective_repo_path).await;
        let pr_ref = if req.pr.host_type == PrHostType::GitHub {
            req.pr.url.as_deref().unwrap_or(&req.pr.id)
        } else {
            req.pr.id.as_str()
        };
        let repo_hint = req.pr.repository_id.as_deref().or(remote_url.as_deref());

        let work_item_refs = match code_provider.get_linked_tickets(pr_ref, repo_hint).await {
            Ok(refs) => refs,
            Err(err) => {
                eprintln!("Warning: failed to retrieve associated work items: {err}");
                return (String::new(), Vec::new());
            }
        };

        if work_item_refs.is_empty() {
            return (String::new(), Vec::new());
        }

        let issue_tracker = build_issue_tracker_provider(&req.settings).ok();
        let mut stories = Vec::new();
        let mut known_docs: Vec<KnownDoc> = Vec::new();
        let mut seen_tickets = HashSet::new();

        for raw_ref in work_item_refs {
            let ref_id = raw_ref.trim().to_string();
            if ref_id.is_empty() || !seen_tickets.insert(ref_id.clone()) {
                continue;
            }

            let ticket = match issue_tracker.as_ref() {
                Some(tracker) => match tracker.fetch_ticket(&ref_id).await {
                    Ok(t) => t,
                    Err(err) => {
                        eprintln!("Warning: failed to fetch details for work item {ref_id}: {err}");
                        continue;
                    }
                },
                None => TicketData {
                    id: Some(ref_id.clone()),
                    title: format!("Work Item {ref_id}"),
                    description: String::new(),
                    acceptance_criteria: None,
                },
            };

            stories.push(format!(
                "Ticket ID: {}\nTitle: {}\nDescription: {}\nAcceptance Criteria: {}",
                ticket.id.as_deref().unwrap_or("N/A"),
                ticket.title,
                ticket.description,
                ticket.acceptance_criteria.as_deref().unwrap_or("N/A"),
            ));

            if let Some(doc_prov) = doc_provider {
                let ac_text = ticket.acceptance_criteria.as_deref().unwrap_or("");
                let discovered =
                    discover_known_docs(doc_prov.as_ref(), &[&ticket.description, ac_text]).await;
                for doc in discovered {
                    if !known_docs.iter().any(|existing| existing.id == doc.id) {
                        known_docs.push(doc);
                    }
                }
            }
        }

        (stories.join("\n\n"), known_docs)
    }

    pub async fn review_pr(
        &self,
        req: PrReviewRequest,
        cancel_rx: watch::Receiver<bool>,
        tx: mpsc::UnboundedSender<PrReviewStreamEvent>,
    ) {
        let _sleep_guard = SleepGuard::acquire();

        let outcome = self.execute_review_phases(&req, cancel_rx, &tx).await;

        let should_cleanup = !req.skip_cleanup || outcome.is_error || outcome.was_cancelled;
        let mut cleaned_up = false;
        if should_cleanup {
            match self.cleanup_checkout(&req.repo_path).await {
                Ok(()) => cleaned_up = true,
                Err(err) => eprintln!(
                    "Warning: failed to automatically clean up checkout at {}: {err}",
                    req.repo_path.display()
                ),
            }
        }

        if outcome.was_cancelled {
            let _ = tx.send(PrReviewStreamEvent::Cancelled { cleaned_up });
        } else if let Some(message) = outcome.error_message {
            let _ = tx.send(PrReviewStreamEvent::Error {
                message,
                cleaned_up,
            });
        } else {
            let usage = CopilotUsage {
                input_tokens: outcome.total_input_tokens,
                output_tokens: outcome.total_output_tokens,
                cache_read_tokens: outcome.total_cache_read_tokens,
                cost: outcome.total_cost,
                model: None,
                phases: Some(outcome.phase_usages),
            };
            let _ = tx.send(PrReviewStreamEvent::Finished {
                comments: outcome.comments,
                usage,
                cleaned_up,
            });
        }
    }

    async fn execute_review_phases(
        &self,
        req: &PrReviewRequest,
        cancel_rx: watch::Receiver<bool>,
        tx: &mpsc::UnboundedSender<PrReviewStreamEvent>,
    ) -> ReviewExecutionOutcome {
        if *cancel_rx.borrow() {
            return ReviewExecutionOutcome::cancelled();
        }

        if req.enabled_phase_ids.is_empty() {
            return ReviewExecutionOutcome::error(
                "No review phases selected. Please select at least one phase to start the review."
                    .to_string(),
            );
        }

        let phases = load_phases_from_disk();
        if phases.is_empty() {
            return ReviewExecutionOutcome::error(
                "No custom review phases found on disk. Please configure them in ~/.stitch/pr-reviewer/phases"
                    .to_string(),
            );
        }

        let enabled_set: HashSet<&str> = req.enabled_phase_ids.iter().map(String::as_str).collect();
        let enabled_phases: Vec<ReviewPhase> = phases
            .into_iter()
            .filter(|phase| enabled_set.contains(phase.id.as_str()))
            .collect();

        if enabled_phases.is_empty() {
            return ReviewExecutionOutcome::error(
                "None of the selected review phases were found on disk.".to_string(),
            );
        }

        for phase in &enabled_phases {
            if let Some(ref err) = phase.template_error {
                return ReviewExecutionOutcome::error(err.clone());
            }
        }

        let persona_guidelines = if !req.persona_name.trim().is_empty()
            && !req.persona_name.trim().eq_ignore_ascii_case("none")
        {
            req.settings
                .pr_reviewer
                .personas
                .iter()
                .find(|p| p.name == req.persona_name)
                .map(|p| p.guidelines.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let effective_repo_path = self.effective_repo_path(&req.repo_path).await;
        let target_branch = if req.pr.target_branch.trim().is_empty() {
            "main"
        } else {
            req.pr.target_branch.trim()
        };
        let diff_files = if let Some(ref pre) = req.prefetched_diff_files {
            pre.clone()
        } else {
            match self
                .git
                .get_diff_files(&effective_repo_path, target_branch)
                .await
            {
                Ok(files) => files,
                Err(err) => {
                    return ReviewExecutionOutcome::error(format!(
                        "Failed to collect PR diff files: {err}"
                    ));
                }
            }
        };

        let doc_provider = build_documentation_provider(&req.settings).ok();
        let any_phase_needs_story = enabled_phases
            .iter()
            .any(|phase| phase_attaches(phase, "story"));

        let (linked_stories_content, known_docs) = if any_phase_needs_story {
            self.resolve_story_context(req, &effective_repo_path, doc_provider.as_ref())
                .await
        } else {
            (String::new(), Vec::new())
        };

        let mut active_tasks: Vec<(ReviewPhase, Vec<PrDiffFile>)> = Vec::new();
        for phase in enabled_phases {
            let eligible = filter_phase_files(&diff_files, &phase.include, &phase.exclude);
            if eligible.is_empty() {
                let _ = tx.send(PrReviewStreamEvent::PhaseSkipped {
                    phase_id: phase.id.clone(),
                    phase_title: phase.title.clone(),
                    reason: "No matching files found".to_string(),
                });
                continue;
            }
            if phase_attaches(&phase, "story") && linked_stories_content.trim().is_empty() {
                let _ = tx.send(PrReviewStreamEvent::PhaseSkipped {
                    phase_id: phase.id.clone(),
                    phase_title: phase.title.clone(),
                    reason: "No linked stories found for this Pull Request".to_string(),
                });
                continue;
            }
            active_tasks.push((phase, eligible));
        }

        if active_tasks.is_empty() {
            return ReviewExecutionOutcome::default();
        }

        let worker_count = resolve_effective_parallelism(
            req.max_parallelism,
            req.settings.max_parallelism,
            system_cpu_count(),
        )
        .min(active_tasks.len())
        .max(1);

        let tasks = Arc::new(active_tasks);
        let queue_cursor = Arc::new(AtomicUsize::new(0));
        let comment_counter = Arc::new(AtomicU64::new(1));
        let comments_acc = Arc::new(Mutex::new(Vec::<ReviewComment>::new()));
        let usages_acc = Arc::new(Mutex::new(Vec::<PhaseUsage>::new()));
        let first_error = Arc::new(Mutex::new(None::<String>));
        let was_cancelled = Arc::new(AtomicBool::new(false));
        let linked_stories_arc = Arc::new(linked_stories_content);
        let known_docs_arc = Arc::new(known_docs);
        let persona_arc = Arc::new(persona_guidelines);
        let custom_instructions_arc = Arc::new(req.custom_instructions.clone());
        let pr_description_arc = Arc::new(req.pr.description.clone());
        let available_models_arc = Arc::new(req.available_models.clone());

        let mut join_set = JoinSet::new();

        for _ in 0..worker_count {
            let copilot = Arc::clone(&self.copilot);
            let tasks = Arc::clone(&tasks);
            let queue_cursor = Arc::clone(&queue_cursor);
            let comment_counter = Arc::clone(&comment_counter);
            let comments_acc = Arc::clone(&comments_acc);
            let usages_acc = Arc::clone(&usages_acc);
            let first_error = Arc::clone(&first_error);
            let was_cancelled = Arc::clone(&was_cancelled);
            let doc_provider = doc_provider.clone();
            let linked_stories = Arc::clone(&linked_stories_arc);
            let known_docs = Arc::clone(&known_docs_arc);
            let persona = Arc::clone(&persona_arc);
            let custom_instructions = Arc::clone(&custom_instructions_arc);
            let pr_description = Arc::clone(&pr_description_arc);
            let available_models = Arc::clone(&available_models_arc);
            let settings = req.settings.clone();
            let model_override = req.model_override.clone();
            let effective_repo = effective_repo_path.clone();
            let cancel_rx = cancel_rx.clone();
            let tx = tx.clone();

            join_set.spawn(async move {
                loop {
                    if *cancel_rx.borrow() {
                        was_cancelled.store(true, Ordering::Relaxed);
                        break;
                    }
                    if first_error.lock().await.is_some() {
                        break;
                    }

                    let idx = queue_cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((phase, eligible_files)) = tasks.get(idx) else {
                        break;
                    };

                    let _ = tx.send(PrReviewStreamEvent::PhaseStarted {
                        phase_id: phase.id.clone(),
                        phase_title: phase.title.clone(),
                    });

                    let attach_story = phase_attaches(phase, "story");
                    let attach_description = phase_attaches(phase, "description");

                    let mut tools = vec![create_report_intent_tool()];
                    if attach_story {
                        let tx_doc = tx.clone();
                        let pid = phase.id.clone();
                        let ptitle = phase.title.clone();
                        let status_cb: Arc<dyn Fn(String) + Send + Sync> =
                            Arc::new(move |status| {
                                let _ = tx_doc.send(PrReviewStreamEvent::PhaseStatus {
                                    phase_id: pid.clone(),
                                    phase_title: ptitle.clone(),
                                    status,
                                });
                            });
                        tools.push(create_request_documentation_tool(
                            doc_provider.clone(),
                            status_cb,
                        ));
                    }

                    let resolved_model = match phase
                        .model
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        Some(m) => m.to_string(),
                        None => resolve_model_override(&model_override, &settings.copilot_model),
                    };

                    let prompt = build_phase_review_prompt(
                        eligible_files,
                        &phase.title,
                        &phase.body,
                        &custom_instructions,
                        if attach_description {
                            &pr_description
                        } else {
                            ""
                        },
                        if attach_story { &linked_stories } else { "" },
                        if attach_story { &known_docs } else { &[] },
                        &persona,
                    );

                    let session = match copilot
                        .create_session(SessionOptions {
                            token: settings.copilot_token.clone(),
                            model: Some(resolved_model.clone()),
                            working_directory: Some(effective_repo.clone()),
                            tools,
                            available_tools: None,
                            streaming: true,
                            label: format!("PR Reviewer Phase: {}", phase.title),
                        })
                        .await
                    {
                        Ok(s) => s,
                        Err(err) => {
                            let mut err_guard = first_error.lock().await;
                            if err_guard.is_none() {
                                *err_guard = Some(err.to_string());
                            }
                            break;
                        }
                    };

                    let mut phase_comments = Vec::new();
                    let stream_res = CopilotService::send_and_collect_stream(
                        &session,
                        &prompt,
                        cancel_rx.clone(),
                        |event| match event {
                            StreamEvent::Status(status) => {
                                let _ = tx.send(PrReviewStreamEvent::PhaseStatus {
                                    phase_id: phase.id.clone(),
                                    phase_title: phase.title.clone(),
                                    status,
                                });
                            }
                            StreamEvent::JsonLine(line) => {
                                let seq = comment_counter.fetch_add(1, Ordering::Relaxed);
                                let comment_id = format!("raw-{seq}-{}", phase.id);
                                match parse_review_jsonl_line(
                                    &line,
                                    &phase.id,
                                    &phase.title,
                                    &comment_id,
                                    &effective_repo,
                                ) {
                                    Some(ParsedReviewLine::Status(status)) => {
                                        let _ = tx.send(PrReviewStreamEvent::PhaseStatus {
                                            phase_id: phase.id.clone(),
                                            phase_title: phase.title.clone(),
                                            status,
                                        });
                                    }
                                    Some(ParsedReviewLine::Comment(comment)) => {
                                        phase_comments.push(comment.clone());
                                        let _ = tx.send(PrReviewStreamEvent::Comment(comment));
                                    }
                                    None => {}
                                }
                            }
                        },
                    )
                    .await;

                    let _ = session.disconnect().await;

                    let comment_count = phase_comments.len();
                    if !phase_comments.is_empty() {
                        comments_acc.lock().await.extend(phase_comments);
                    }

                    match stream_res {
                        Ok(usage) => {
                            let raw_model = usage.model.clone().unwrap_or(resolved_model);
                            let display_model =
                                resolve_model_display_name(&raw_model, &available_models);
                            usages_acc.lock().await.push(PhaseUsage {
                                phase_title: phase.title.clone(),
                                model: display_model,
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_read_tokens: usage.cache_read_tokens,
                                cost: usage.cost,
                                multiplier: usage.cost,
                            });
                            let _ = tx.send(PrReviewStreamEvent::PhaseCompleted {
                                phase_id: phase.id.clone(),
                                phase_title: phase.title.clone(),
                                comment_count,
                            });
                        }
                        Err(StreamError::Cancelled) => {
                            was_cancelled.store(true, Ordering::Relaxed);
                            break;
                        }
                        Err(StreamError::Timeout(dur)) => {
                            let mut err_guard = first_error.lock().await;
                            if err_guard.is_none() {
                                *err_guard = Some(format!(
                                    "Phase '{}' timed out after {} seconds waiting for Copilot response.",
                                    phase.title,
                                    dur.as_secs()
                                ));
                            }
                            break;
                        }
                        Err(StreamError::Other(err)) => {
                            let mut err_guard = first_error.lock().await;
                            if err_guard.is_none() {
                                *err_guard = Some(err.to_string());
                            }
                            break;
                        }
                    }
                }
            });
        }

        while let Some(res) = join_set.join_next().await {
            if let Err(join_err) = res {
                let mut err_guard = first_error.lock().await;
                if err_guard.is_none() {
                    *err_guard = Some(format!("Review worker failed: {join_err}"));
                }
            }
        }

        let phase_usages = usages_acc.lock().await.clone();
        let total_input_tokens: u64 = phase_usages.iter().map(|p| p.input_tokens).sum();
        let total_output_tokens: u64 = phase_usages.iter().map(|p| p.output_tokens).sum();
        let total_cache_read_tokens: u64 = phase_usages.iter().map(|p| p.cache_read_tokens).sum();
        let total_cost: f64 = phase_usages.iter().map(|p| p.cost).sum();
        let comments = comments_acc.lock().await.clone();
        let err_msg = first_error.lock().await.clone();
        let cancelled = was_cancelled.load(Ordering::Relaxed);

        ReviewExecutionOutcome {
            comments,
            phase_usages,
            total_input_tokens,
            total_output_tokens,
            total_cache_read_tokens,
            total_cost,
            is_error: err_msg.is_some(),
            error_message: err_msg,
            was_cancelled: cancelled,
        }
    }

    pub async fn post_pr_comment(
        &self,
        repo_path: &Path,
        pr_url_or_id: &str,
        request: &PostPrCommentRequest,
        settings: &AppSettings,
    ) -> anyhow::Result<()> {
        let provider = build_code_review_provider(settings)?;
        let remote_url = self.git.get_remote_url(repo_path).await;
        provider
            .post_pr_comment(pr_url_or_id, request, remote_url.as_deref())
            .await
    }

    pub async fn critique_comments(
        &self,
        req: PrCriticRequest,
        cancel_rx: watch::Receiver<bool>,
        tx: mpsc::UnboundedSender<PrCriticStreamEvent>,
    ) {
        let _sleep_guard = SleepGuard::acquire();

        let outcome = self.execute_critic_pass(&req, cancel_rx, &tx).await;

        let mut cleaned_up = false;
        match self.cleanup_checkout(&req.repo_path).await {
            Ok(()) => cleaned_up = true,
            Err(err) => eprintln!(
                "Warning: failed to clean up checkout after Critic pass at {}: {err}",
                req.repo_path.display()
            ),
        }

        match outcome {
            CriticOutcome::Finished { comments, usage } => {
                let _ = tx.send(PrCriticStreamEvent::Finished {
                    comments,
                    usage,
                    cleaned_up,
                });
            }
            CriticOutcome::Cancelled => {
                let _ = tx.send(PrCriticStreamEvent::Cancelled { cleaned_up });
            }
            CriticOutcome::Error(message) => {
                let _ = tx.send(PrCriticStreamEvent::Error {
                    message,
                    cleaned_up,
                });
            }
        }
    }

    async fn execute_critic_pass(
        &self,
        req: &PrCriticRequest,
        cancel_rx: watch::Receiver<bool>,
        tx: &mpsc::UnboundedSender<PrCriticStreamEvent>,
    ) -> CriticOutcome {
        if *cancel_rx.borrow() {
            return CriticOutcome::Cancelled;
        }
        if req.comments.is_empty() {
            return CriticOutcome::Finished {
                comments: Vec::new(),
                usage: CopilotUsage::default(),
            };
        }

        let effective_repo_path = self.effective_repo_path(&req.repo_path).await;
        let mut tools = vec![create_report_intent_tool()];

        if !req.pr_url_or_id.trim().is_empty() {
            if let Ok(provider) = build_code_review_provider(&req.settings) {
                let remote_url = self.git.get_remote_url(&req.repo_path).await;
                let tx_status = tx.clone();
                let status_cb: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |status| {
                    let _ = tx_status.send(PrCriticStreamEvent::Status(status));
                });
                tools.push(create_view_existing_comments_tool(
                    provider,
                    req.pr_url_or_id.clone(),
                    remote_url,
                    status_cb,
                ));
            }
        }

        let persona_guidelines = if !req.persona_name.trim().is_empty()
            && !req.persona_name.trim().eq_ignore_ascii_case("none")
        {
            req.settings
                .pr_reviewer
                .personas
                .iter()
                .find(|p| p.name == req.persona_name)
                .map(|p| p.guidelines.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let disk_config = load_config_from_dir(&pr_reviewer_root_dir());
        let critic_instruction = disk_config.critic_instruction.unwrap_or_default();

        let prompt = build_critic_prompt(
            &req.comments,
            &req.pr.description,
            &persona_guidelines,
            &critic_instruction,
        );

        let resolved_model =
            resolve_model_override(&req.model_override, &req.settings.copilot_model);

        let session = match self
            .copilot
            .create_session(SessionOptions {
                token: req.settings.copilot_token.clone(),
                model: Some(resolved_model.clone()),
                working_directory: Some(effective_repo_path.clone()),
                tools,
                available_tools: None,
                streaming: true,
                label: "PR Reviewer Phase: Critic".to_string(),
            })
            .await
        {
            Ok(s) => s,
            Err(err) => return CriticOutcome::Error(err.to_string()),
        };

        let mut decision_lines: Vec<String> = Vec::new();
        let stream_res = CopilotService::send_and_collect_stream(
            &session,
            &prompt,
            cancel_rx,
            |event| match event {
                StreamEvent::Status(status) => {
                    let _ = tx.send(PrCriticStreamEvent::Status(status));
                }
                StreamEvent::JsonLine(line) => {
                    decision_lines.push(line);
                }
            },
        )
        .await;

        let _ = session.disconnect().await;

        match stream_res {
            Ok(mut usage) => {
                let critiqued = apply_critic_decisions(
                    &req.comments,
                    &decision_lines,
                    Some(&effective_repo_path),
                );
                let raw_model = usage.model.clone().unwrap_or(resolved_model);
                let display_model = resolve_model_display_name(&raw_model, &req.available_models);
                usage.phases = Some(vec![PhaseUsage {
                    phase_title: "Critic".to_string(),
                    model: display_model,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cost: usage.cost,
                    multiplier: usage.cost,
                }]);
                CriticOutcome::Finished {
                    comments: critiqued,
                    usage,
                }
            }
            Err(StreamError::Cancelled) => CriticOutcome::Cancelled,
            Err(StreamError::Timeout(dur)) => CriticOutcome::Error(format!(
                "Critic phase timed out after {} seconds waiting for Copilot response.",
                dur.as_secs()
            )),
            Err(StreamError::Other(err)) => CriticOutcome::Error(err.to_string()),
        }
    }
}

enum CriticOutcome {
    Finished {
        comments: Vec<ReviewComment>,
        usage: CopilotUsage,
    },
    Cancelled,
    Error(String),
}

#[derive(Default)]
struct ReviewExecutionOutcome {
    comments: Vec<ReviewComment>,
    phase_usages: Vec<PhaseUsage>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_tokens: u64,
    total_cost: f64,
    is_error: bool,
    error_message: Option<String>,
    was_cancelled: bool,
}

impl ReviewExecutionOutcome {
    fn error(message: String) -> Self {
        Self {
            is_error: true,
            error_message: Some(message),
            ..Self::default()
        }
    }

    fn cancelled() -> Self {
        Self {
            was_cancelled: true,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PrDiffStatus;
    use std::process::Command as StdCommand;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    #[test]
    fn verify_repo_path_resolves_subdirectory_to_git_root() {
        let tmp = unique_temp_dir("stitch-rs-verify-repo");
        let sub = tmp.join("packages").join("app");
        std::fs::create_dir_all(&sub).expect("create sub");

        let out = StdCommand::new("git")
            .current_dir(&tmp)
            .arg("init")
            .output()
            .expect("git init");
        assert!(out.status.success());

        crate::infrastructure::runtime::runtime().block_on(async {
            let git = GitService::new();
            let verified_sub = verify_repo_path(&git, &sub).await;
            assert!(verified_sub.was_modified);
            assert_eq!(
                canonical_or_raw(&verified_sub.path),
                canonical_or_raw(&tmp)
            );

            let verified_root = verify_repo_path(&git, &verified_sub.path).await;
            assert!(!verified_root.was_modified);
        });

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filter_phase_files_matches_include_exclude_and_dotfiles() {
        let files = vec![
            PrDiffFile {
                path: "src/auth.rs".to_string(),
                status: PrDiffStatus::Modified,
            },
            PrDiffFile {
                path: "src/auth_test.rs".to_string(),
                status: PrDiffStatus::Added,
            },
            PrDiffFile {
                path: ".github/workflows/ci.yml".to_string(),
                status: PrDiffStatus::Modified,
            },
            PrDiffFile {
                path: "README.md".to_string(),
                status: PrDiffStatus::Modified,
            },
        ];

        let rust_non_test = filter_phase_files(
            &files,
            &["src/**/*.rs".to_string()],
            &["**/*_test.rs".to_string()],
        );
        assert_eq!(rust_non_test.len(), 1);
        assert_eq!(rust_non_test[0].path, "src/auth.rs");

        let workflows = filter_phase_files(&files, &[".github/**/*.yml".to_string()], &[]);
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0].path, ".github/workflows/ci.yml");

        let top_level_only = filter_phase_files(&files, &["*.rs".to_string()], &[]);
        assert!(top_level_only.is_empty());
    }

    #[test]
    fn extract_file_context_slices_lines_handles_leading_slash_and_blocks_traversal() {
        let tmp = unique_temp_dir("stitch-rs-context");
        let src_dir = tmp.join("src");
        std::fs::create_dir_all(&src_dir).expect("mkdir");
        let sample = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";
        std::fs::write(src_dir.join("main.rs"), sample).expect("write sample");

        let slice = extract_file_context(&tmp, "/src/main.rs", 3, 1).expect("slice");
        assert_eq!(
            slice,
            vec![
                CodeContextLine {
                    line: 2,
                    text: "line 2".to_string(),
                    is_target: false,
                },
                CodeContextLine {
                    line: 3,
                    text: "line 3".to_string(),
                    is_target: true,
                },
                CodeContextLine {
                    line: 4,
                    text: "line 4".to_string(),
                    is_target: false,
                },
            ]
        );

        assert!(extract_file_context(&tmp, "../etc/passwd", 1, 1).is_none());
        assert!(extract_file_context(&tmp, "src/main.rs", 0, 1).is_none());
        assert!(extract_file_context(&tmp, "src/missing.rs", 1, 1).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_review_jsonl_line_handles_general_line_status_and_enriches_code_context() {
        let tmp = unique_temp_dir("stitch-rs-parse-jsonl");
        std::fs::create_dir_all(tmp.join("src")).expect("mkdir");
        std::fs::write(
            tmp.join("src").join("lib.rs"),
            "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
        )
        .expect("write");

        let status = parse_review_jsonl_line(
            r#"{"type":"status","text":"Reading lib.rs"}"#,
            "sec",
            "Security",
            "c1",
            &tmp,
        );
        assert_eq!(
            status,
            Some(ParsedReviewLine::Status("Reading lib.rs".to_string()))
        );

        let general = parse_review_jsonl_line(
            r#"{"type":"general","comment":"Split module"}"#,
            "sec",
            "Security",
            "c2",
            &tmp,
        );
        match general {
            Some(ParsedReviewLine::Comment(c)) => {
                assert_eq!(c.comment_type, ReviewCommentType::General);
                assert_eq!(c.comment, "Split module");
                assert_eq!(c.phase.as_deref(), Some("Security"));
                assert_eq!(c.phase_id.as_deref(), Some("sec"));
                assert!(!c.posted);
            }
            other => panic!("unexpected parsed general: {other:?}"),
        }

        let line = parse_review_jsonl_line(
            r#"{"type":"line","file":"src/lib.rs","line":2,"context":1,"comment":"Document public fn"}"#,
            "sec",
            "Security",
            "c3",
            &tmp,
        );
        match line {
            Some(ParsedReviewLine::Comment(c)) => {
                assert_eq!(c.comment_type, ReviewCommentType::Line);
                assert_eq!(c.file.as_deref(), Some("src/lib.rs"));
                assert_eq!(c.line, Some(2));
                let ctx = c.code_lines.expect("code context");
                assert_eq!(ctx.len(), 3);
                assert_eq!(ctx[0].line, 1);
                assert!(ctx[1].is_target);
                assert_eq!(ctx[1].text, "pub fn b() {}");
                assert_eq!(ctx[2].line, 3);
            }
            other => panic!("unexpected parsed line: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn apply_critic_decisions_handles_approve_edit_relocate_reject_merge_reply_and_fallback() {
        let tmp = unique_temp_dir("stitch-rs-critic-decisions");
        std::fs::create_dir_all(tmp.join("src")).expect("mkdir");
        std::fs::write(
            tmp.join("src").join("auth.rs"),
            "fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n",
        )
        .expect("write");

        let make_line = |id: &str, line: usize, text: &str| ReviewComment {
            id: id.to_string(),
            comment_type: ReviewCommentType::Line,
            file: Some("src/auth.rs".to_string()),
            line: Some(line),
            context: Some(1),
            comment: text.to_string(),
            phase: Some("Security".to_string()),
            phase_id: Some("sec".to_string()),
            code_lines: extract_file_context(&tmp, "src/auth.rs", line, 1),
            posted: false,
            status: None,
            reason: None,
            thread_id: None,
            merged_from_indices: None,
        };

        let originals = vec![
            make_line("raw-1", 1, "Approve me"),
            make_line("raw-2", 1, "Relocate me to line 3"),
            make_line("raw-3", 2, "Reject me"),
            make_line("raw-4", 2, "Merge part A"),
            make_line("raw-5", 2, "Merge part B"),
            make_line("raw-6", 3, "Reply to existing thread"),
            make_line("raw-7", 4, "Unhandled fallback"),
        ];

        let decisions = vec![
            r#"{"action":"approve","commentIndex":1}"#.to_string(),
            r#"{"action":"edit","commentIndex":2,"type":"line","file":"src/auth.rs","line":3,"comment":"Edited and relocated"}"#.to_string(),
            r#"{"action":"reject","commentIndex":3,"reason":"False positive"}"#.to_string(),
            r#"{"action":"merge","commentIndices":[4,5],"type":"line","file":"src/auth.rs","line":4,"comment":"Combined A+B"}"#.to_string(),
            r#"{"action":"reply","commentIndex":6,"threadId":889,"comment":"Issue still present"}"#.to_string(),
        ];

        let out = apply_critic_decisions(&originals, &decisions, Some(&tmp));
        assert_eq!(out.len(), 6);

        // 1: Approved
        assert_eq!(out[0].status, Some(ReviewCommentStatus::Approved));
        assert_eq!(out[0].comment, "Approve me");

        // 2: Edited + relocated to line 3 (code_lines re-extracted)
        assert_eq!(out[1].status, Some(ReviewCommentStatus::Edited));
        assert_eq!(out[1].comment, "Edited and relocated");
        assert_eq!(out[1].line, Some(3));
        let relocated_ctx = out[1].code_lines.as_ref().expect("relocated context");
        assert!(relocated_ctx.iter().any(|l| l.line == 3 && l.is_target && l.text == "fn three() {}"));

        // 3: Rejected with terse reason
        assert_eq!(out[2].status, Some(ReviewCommentStatus::Rejected));
        assert_eq!(out[2].reason.as_deref(), Some("False positive"));

        // 4+5: Merged + relocated to line 4
        assert_eq!(out[3].status, Some(ReviewCommentStatus::Merged));
        assert_eq!(out[3].comment, "Combined A+B");
        assert_eq!(out[3].line, Some(4));
        assert_eq!(out[3].merged_from_indices, Some(vec![3, 4]));
        let merged_ctx = out[3].code_lines.as_ref().expect("merged context");
        assert!(merged_ctx.iter().any(|l| l.line == 4 && l.is_target && l.text == "fn four() {}"));

        // 6: Reply to thread #889
        assert_eq!(out[4].status, Some(ReviewCommentStatus::Reply));
        assert_eq!(out[4].thread_id, Some(889));
        assert_eq!(out[4].comment, "Issue still present");

        // 7: Unhandled fallback -> Approved
        assert_eq!(out[5].status, Some(ReviewCommentStatus::Approved));
        assert_eq!(out[5].comment, "Unhandled fallback");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn checkout_pr_with_progress_handles_cancel_non_git_and_dirty_repo_with_status() {
        let tmp = unique_temp_dir("stitch-rs-checkout-progress");
        let non_git = tmp.join("not-a-repo");
        let dirty_repo = tmp.join("dirty-repo");
        std::fs::create_dir_all(&non_git).expect("mkdir non-git");
        std::fs::create_dir_all(&dirty_repo).expect("mkdir dirty");

        let run_git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .current_dir(&dirty_repo)
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {:?} failed", args);
        };
        run_git(&["init"]);
        run_git(&["config", "user.name", "Test"]);
        run_git(&["config", "user.email", "test@stitch.local"]);
        std::fs::write(dirty_repo.join("base.txt"), "base").expect("write");
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "init"]);
        std::fs::write(dirty_repo.join("dirty.txt"), "uncommitted").expect("dirty");

        crate::infrastructure::runtime::runtime().block_on(async {
            let git = Arc::new(GitService::new());
            let copilot = Arc::new(CopilotService::new());
            let service = PrReviewerService::new(git, copilot);
            let mut settings = AppSettings::default();
            settings.git_worktree_enabled = false;

            // 1. Pre-cancelled returns Cancelled immediately.
            let (_tx, rx) = watch::channel(true);
            let res = service
                .checkout_pr_with_progress(&dirty_repo, 1, "main", None, None, &settings, Some(rx), |_| {})
                .await;
            assert!(matches!(res, Err(CheckoutError::Cancelled)));

            // 2. Non-git directory returns Other.
            let res = service
                .checkout_pr_with_progress(&non_git, 1, "main", None, None, &settings, None, |_| {})
                .await;
            assert!(matches!(res, Err(CheckoutError::Other(_))));

            // 3. Dirty repo emits expected statuses and returns DirtyRepo without mutating HEAD.
            let statuses = Arc::new(std::sync::Mutex::new(Vec::new()));
            let statuses_cb = Arc::clone(&statuses);
            let res = service
                .checkout_pr_with_progress(
                    &dirty_repo,
                    42,
                    "main",
                    None,
                    None,
                    &settings,
                    None,
                    move |msg| {
                        statuses_cb.lock().expect("lock").push(msg);
                    },
                )
                .await;
            assert!(matches!(res, Err(CheckoutError::DirtyRepo(_))));
            let recorded = statuses.lock().expect("lock").clone();
            assert_eq!(
                recorded,
                vec![
                    "Verifying Git repository...".to_string(),
                    "Checking working tree status...".to_string(),
                ]
            );
        });

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
