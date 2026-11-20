use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Application theme preference (`Auto` follows OS appearance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::Auto,
        }
    }
}

/// Persisted application configuration (secrets are stored separately in the OS Keychain).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub version: u32,
    pub feature_type: String,
    pub story_type: String,
    pub task_type: String,
    pub test_task_title: String,
    pub connectors: ConnectorsConfig,
    pub sources: SourcesConfig,
    #[serde(skip)]
    pub copilot_token: Option<String>,
    pub copilot_model: String,
    pub theme: ThemeMode,
    pub git_worktree_enabled: bool,
    pub git_worktree_base_dir: Option<PathBuf>,
    pub max_parallelism: Option<usize>,
    pub prompts: CustomPromptsConfig,
    pub pr_reviewer: PrReviewerSettingsConfig,
    pub repo_paths: HashMap<String, PathBuf>,
    pub author_personas: HashMap<String, String>,
    pub last_run_version: Option<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: 1,
            feature_type: "Feature".to_string(),
            story_type: "Product Backlog Item".to_string(),
            task_type: "Task".to_string(),
            test_task_title: "Testing".to_string(),
            connectors: ConnectorsConfig::default(),
            sources: SourcesConfig::default(),
            copilot_token: None,
            copilot_model: "auto".to_string(),
            theme: ThemeMode::Auto,
            git_worktree_enabled: false,
            git_worktree_base_dir: None,
            max_parallelism: None,
            prompts: CustomPromptsConfig::default(),
            pr_reviewer: PrReviewerSettingsConfig::default(),
            repo_paths: HashMap::new(),
            author_personas: HashMap::new(),
            last_run_version: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectorsConfig {
    pub atlassian: AtlassianConfig,
    pub azure_dev_ops: AzureDevOpsConfig,
    pub github: GitHubConfig,
}

impl ConnectorsConfig {
    pub fn is_azure_configured(&self) -> bool {
        !self.azure_dev_ops.org.trim().is_empty()
            && self
                .azure_dev_ops
                .pat
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    pub fn is_atlassian_configured(&self) -> bool {
        !self.atlassian.url.trim().is_empty()
            && self
                .atlassian
                .token
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }

    pub fn is_github_configured(&self) -> bool {
        self.github
            .token
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AtlassianConfig {
    pub url: String,
    pub username: String,
    #[serde(skip)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AzureDevOpsConfig {
    pub org: String,
    pub project: String,
    #[serde(skip)]
    pub pat: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GitHubConfig {
    pub owner: String,
    #[serde(skip)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourcesConfig {
    pub issues: String,
    pub code: String,
    pub docs: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomPromptsConfig {
    pub story_writer: StoryWriterPrompts,
    pub test_case_writer: TestCaseWriterPrompts,
    pub story_elaborator: StoryElaboratorPrompts,
    pub code_explainer: CodeExplainerPrompts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoryWriterPrompts {
    pub general: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    pub notes: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TestCaseWriterPrompts {
    pub general: String,
    pub description: String,
    pub pre_conditions: String,
    pub steps: String,
    pub expected_result: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoryElaboratorPrompts {
    pub general: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CodeExplainerPrompts {
    pub general: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrReviewerSettingsConfig {
    pub personas: Vec<ReviewPersona>,
    #[serde(default = "default_true")]
    pub critic_enabled: bool,
}

impl Default for PrReviewerSettingsConfig {
    fn default() -> Self {
        Self {
            personas: Vec::new(),
            critic_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReviewPersona {
    pub name: String,
    #[serde(alias = "content")]
    pub guidelines: String,
}

/// Detailed authentication and runtime metadata returned by `CopilotService::check_auth_status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CopilotAuthInfo {
    pub is_authenticated: bool,
    pub login: Option<String>,
    pub auth_type: Option<String>,
    pub host: Option<String>,
    pub status_message: Option<String>,
    pub cli_version: Option<String>,
    pub protocol_version: Option<String>,
}

/// Available GitHub Copilot model metadata returned by `CopilotService::list_models`.
#[derive(Debug, Clone, PartialEq)]
pub struct CopilotModelInfo {
    pub id: String,
    pub name: String,
    pub billing_multiplier: Option<f64>,
}

impl CopilotModelInfo {
    pub fn multiplier_label(&self) -> String {
        match self.billing_multiplier {
            Some(m) if (m - 0.0).abs() < f64::EPSILON => "0x".to_string(),
            Some(m) if (m.fract()).abs() < 1e-6 => format!("{}x", m as i64),
            Some(m) => {
                let s = format!("{m:.2}");
                let trimmed = s.trim_end_matches('0').trim_end_matches('.');
                format!("{trimmed}x")
            }
            None => "—".to_string(),
        }
    }
}

/// Connection state displayed in the Footer Status Bar and Settings > GitHub Copilot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopilotAuthStatus {
    Checking,
    Connected(CopilotAuthInfo),
    Disconnected {
        info: Option<CopilotAuthInfo>,
        error: Option<String>,
    },
}

impl Default for CopilotAuthStatus {
    fn default() -> Self {
        Self::Disconnected {
            info: None,
            error: None,
        }
    }
}

/// Lifecycle stage grouping used in the Left Activity Rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleGroup {
    Ideation,
    SolutionDesign,
    Build,
    Review,
}

impl LifecycleGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ideation => "IDEATION",
            Self::SolutionDesign => "SOLUTION DESIGN",
            Self::Build => "BUILD",
            Self::Review => "REVIEW",
        }
    }
}

/// Token and billing usage accumulated across a Copilot session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    pub model: Option<String>,
    pub phases: Option<Vec<PhaseUsage>>,
}

/// Per-phase token and billing breakdown (used by multi-phase tools such as PR Reviewer).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseUsage {
    pub phase_title: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    pub multiplier: f64,
}

/// Generic wrapper pairing a tool result with token usage and optional database session ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct CopilotResult<T> {
    pub result: T,
    pub usage: CopilotUsage,
    pub db_session_id: Option<i64>,
}

/// Result of scanning the worktree base directory for orphaned worktrees.
#[derive(Debug, Clone, Default)]
pub struct WorktreeCheckResult {
    pub directories: Vec<PathBuf>,
}

/// Result of cleaning up orphaned worktree directories.
#[derive(Debug, Clone, Default)]
pub struct WorktreeCleanResult {
    pub removed: usize,
    pub errors: Vec<String>,
}

/// Issue / Work Item payload returned by `IssueTrackerProvider`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketData {
    pub id: Option<String>,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Option<String>,
}

/// Documentation page payload returned by `DocumentationProvider`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocPageData {
    pub id: String,
    pub title: String,
    pub body: String,
}

/// User story payload streamed from Copilot or edited inline in `StoryWriterView`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoryData {
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Test case payload streamed from Copilot and managed in `TestCaseWriterView`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCase {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub pre_conditions: String,
    #[serde(default)]
    pub steps: String,
    #[serde(default)]
    pub expected_result: String,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub deleted: bool,
}

/// Metadata for a documentation page discovered in a ticket or PR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownDoc {
    pub id: String,
    pub title: String,
}

/// Filter category for listing active pull requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrSearchType {
    Assigned,
    Created,
    All,
}

/// Hosting provider kind for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrHostType {
    #[default]
    Azure,
    GitHub,
}

/// Components parsed from a Pull Request web URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPrUrl {
    pub org: String,
    pub project: String,
    pub repo_name: String,
    pub pr_number: u64,
}

/// Components parsed from a Git remote origin URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRemoteUrl {
    pub org: String,
    pub project: String,
    pub repo_name: String,
}

/// Pull Request metadata retrieved by `CodeReviewProvider`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrMetadata {
    pub id: String,
    pub title: String,
    pub description: String,
    pub source_branch: String,
    pub target_branch: String,
    pub author: String,
    pub author_unique_name: String,
    pub repository_name: String,
    pub repository_id: Option<String>,
    pub host_type: PrHostType,
    pub url: Option<String>,
}

/// Git file change status parsed from `git diff --name-status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrDiffStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    TypeChanged,
    #[default]
    Unknown,
}

impl PrDiffStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
            Self::TypeChanged => "type_changed",
            Self::Unknown => "unknown",
        }
    }
}

/// Changed file entry returned by `GitService::get_diff_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDiffFile {
    pub path: String,
    pub status: PrDiffStatus,
}

/// Parsed `~/.stitch/pr-reviewer/config.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrReviewerDiskConfig {
    pub groups: Vec<String>,
    pub critic_instruction: Option<String>,
}

/// A review phase markdown file loaded from `~/.stitch/pr-reviewer/phases/*.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPhase {
    pub id: String,
    pub title: String,
    pub group: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub attach: Vec<String>,
    pub body: String,
    pub template: Option<String>,
    pub template_error: Option<String>,
    pub model: Option<String>,
}

/// Source line slice surrounding a line-specific PR review comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeContextLine {
    pub line: usize,
    pub text: String,
    pub is_target: bool,
}

/// Kind of Pull Request review comment produced by Copilot phases or posted to a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewCommentType {
    #[default]
    General,
    Line,
}

/// Evaluation status assigned to a draft comment by the AI Critic pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewCommentStatus {
    Approved,
    Edited,
    Rejected,
    Merged,
    Reply,
}

/// A draft or posted PR review comment generated during a review phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    pub id: String,
    #[serde(rename = "type")]
    pub comment_type: ReviewCommentType,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub context: Option<usize>,
    pub comment: String,
    pub phase: Option<String>,
    pub phase_id: Option<String>,
    pub code_lines: Option<Vec<CodeContextLine>>,
    pub posted: bool,
    #[serde(default)]
    pub status: Option<ReviewCommentStatus>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub thread_id: Option<u64>,
    #[serde(default)]
    pub merged_from_indices: Option<Vec<usize>>,
}

/// Line span for an existing PR comment thread anchored to a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingPrCommentLineRange {
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

/// Individual comment inside an existing Pull Request discussion thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingPrThreadComment {
    pub id: Option<u64>,
    pub author: String,
    pub content: String,
    pub published_date: Option<String>,
}

/// Existing Pull Request comment thread fetched by `CodeReviewProvider::get_pr_comment_threads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingPrCommentThread {
    pub id: u64,
    pub status: String,
    pub is_resolved: bool,
    #[serde(rename = "type")]
    pub comment_type: ReviewCommentType,
    pub file: Option<String>,
    pub line_range: Option<ExistingPrCommentLineRange>,
    pub comments: Vec<ExistingPrThreadComment>,
}

/// Payload used when posting a review comment (or reply) to Azure DevOps / GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostPrCommentRequest {
    pub comment_type: ReviewCommentType,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub comment: String,
    pub edited: bool,
    pub thread_id: Option<u64>,
}

/// Execution state for an individual PR review phase in the progress checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseRunStatus {
    Pending,
    Running,
    Completed,
    Skipped,
}

/// Live UI progress record for a scheduled PR review phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseProgress {
    pub id: String,
    pub title: String,
    pub group: String,
    pub status: PhaseRunStatus,
    pub status_text: Option<String>,
    pub reason: Option<String>,
    pub comment_count: usize,
}

/// Returns the logical CPU count on the host (minimum `1`).
pub fn system_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Returns the maximum allowed review agent parallelism for `cpu_count` (`1` when `< 4` cores, else `cpu_count - 2`).
pub fn max_parallelism_limit(cpu_count: usize) -> usize {
    if cpu_count < 4 { 1 } else { cpu_count - 2 }
}

/// Returns the default review agent parallelism (`1` when `< 4` cores, else `(cpu_count / 2).clamp(1, cpu_count - 2)`).
pub fn default_parallelism(cpu_count: usize) -> usize {
    if cpu_count < 4 {
        1
    } else {
        (cpu_count / 2).clamp(1, cpu_count - 2)
    }
}

/// Resolves effective parallelism from optional override/settings within host limits.
pub fn resolve_effective_parallelism(
    preferred: Option<usize>,
    settings_limit: Option<usize>,
    cpu_count: usize,
) -> usize {
    if cpu_count < 4 {
        return 1;
    }
    let limit = max_parallelism_limit(cpu_count);
    if let Some(val) = preferred.or(settings_limit) {
        if (1..=limit).contains(&val) {
            return val;
        }
    }
    default_parallelism(cpu_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_helpers_enforce_bounds_and_defaults() {
        assert_eq!(max_parallelism_limit(1), 1);
        assert_eq!(max_parallelism_limit(2), 1);
        assert_eq!(max_parallelism_limit(3), 1);
        assert_eq!(max_parallelism_limit(4), 2);
        assert_eq!(max_parallelism_limit(12), 10);

        assert_eq!(default_parallelism(2), 1);
        assert_eq!(default_parallelism(4), 2);
        assert_eq!(default_parallelism(8), 4);
        assert_eq!(default_parallelism(9), 4);

        assert_eq!(resolve_effective_parallelism(Some(5), Some(3), 2), 1);
        assert_eq!(resolve_effective_parallelism(Some(5), Some(3), 8), 5);
        assert_eq!(resolve_effective_parallelism(Some(0), Some(3), 8), 4);
        assert_eq!(resolve_effective_parallelism(Some(99), Some(3), 8), 4);
        assert_eq!(resolve_effective_parallelism(None, Some(3), 8), 3);
        assert_eq!(resolve_effective_parallelism(None, Some(10), 8), 4);
        assert_eq!(resolve_effective_parallelism(None, None, 8), 4);
    }

    #[test]
    fn review_persona_deserializes_guidelines_and_content_alias() {
        let with_guidelines: ReviewPersona =
            serde_json::from_str(r#"{"name":"Security","guidelines":"Check auth boundaries."}"#)
                .expect("guidelines json");
        assert_eq!(with_guidelines.name, "Security");
        assert_eq!(with_guidelines.guidelines, "Check auth boundaries.");

        let with_content: ReviewPersona =
            serde_json::from_str(r#"{"name":"Perf","content":"Watch allocations."}"#)
                .expect("content alias json");
        assert_eq!(with_content.name, "Perf");
        assert_eq!(with_content.guidelines, "Watch allocations.");
    }
}
