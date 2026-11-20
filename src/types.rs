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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoryElaboratorPrompts {
    pub general: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrReviewerSettingsConfig {
    pub personas: Vec<ReviewPersona>,
    pub critic_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReviewPersona {
    pub name: String,
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
#[allow(dead_code)]
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
