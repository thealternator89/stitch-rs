use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::{Hsla, rgb};
use tokio::sync::{mpsc, watch};

use crate::features::tshirt_estimator::prompts::build_tshirt_prompt;
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, create_report_intent_tool,
};
use crate::infrastructure::git::GitService;
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TShirtSize {
    Xs,
    S,
    M,
    L,
    Xl,
}

impl TShirtSize {
    pub fn from_str_loose(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "XS" => Some(Self::Xs),
            "S" => Some(Self::S),
            "M" => Some(Self::M),
            "L" => Some(Self::L),
            "XL" => Some(Self::Xl),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Xs => "XS",
            Self::S => "S",
            Self::M => "M",
            Self::L => "L",
            Self::Xl => "XL",
        }
    }

    pub fn effort_hint(self) -> &'static str {
        match self {
            Self::Xs => "Extra Small • Usually < 1 day",
            Self::S => "Small • Usually < 5 days",
            Self::M => "Medium • Usually < 10 days",
            Self::L => "Large • Usually < 15 days",
            Self::Xl => "Extra Large • Usually > 15 days",
        }
    }

    pub fn badge_color(self) -> Hsla {
        match self {
            Self::Xs => rgb(0x10b981).into(),
            Self::S => rgb(0x06b6d4).into(),
            Self::M => rgb(0xf59e0b).into(),
            Self::L => rgb(0xf97316).into(),
            Self::Xl => rgb(0xef4444).into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EstimationRequest {
    pub description: String,
    pub repo_path: PathBuf,
    pub branch: String,
    pub model_override: String,
    pub settings: AppSettings,
}

#[derive(Debug, Clone)]
pub struct EstimationResult {
    pub size: TShirtSize,
    pub reasoning: String,
    pub usage: CopilotUsage,
}

#[derive(Debug, Clone)]
pub enum EstimationStreamEvent {
    Status(String),
    Completed(EstimationResult),
    TimedOut { timeout_secs: u64 },
    Cancelled,
    Error(String),
}

pub struct ActiveEstimationHandle {
    cancel_tx: watch::Sender<bool>,
}

impl ActiveEstimationHandle {
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }
}

/// Starts a background T-Shirt Size Estimation run on the Tokio runtime and streams events to the UI.
pub fn start_estimation(
    copilot: Arc<CopilotService>,
    git: Arc<GitService>,
    req: EstimationRequest,
) -> (
    ActiveEstimationHandle,
    mpsc::UnboundedReceiver<EstimationStreamEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);

    runtime().spawn(async move {
        let terminal = run_estimation_task(copilot, git, req, cancel_rx, &tx).await;
        let _ = tx.send(terminal);
    });

    (ActiveEstimationHandle { cancel_tx }, rx)
}

async fn run_estimation_task(
    copilot: Arc<CopilotService>,
    git: Arc<GitService>,
    req: EstimationRequest,
    cancel_rx: watch::Receiver<bool>,
    tx: &mpsc::UnboundedSender<EstimationStreamEvent>,
) -> EstimationStreamEvent {
    if !req.repo_path.is_dir() {
        return EstimationStreamEvent::Error(format!(
            "Repository directory does not exist: {}",
            req.repo_path.display()
        ));
    }

    let mut effective_repo_path = req.repo_path.clone();
    let mut created_worktree: Option<(PathBuf, PathBuf)> = None;

    if req.settings.git_worktree_enabled {
        if let Some(base_dir) = req.settings.git_worktree_base_dir.clone() {
            if !git.check_git_repo(&req.repo_path).await {
                return EstimationStreamEvent::Error(
                    "Git Worktree Support is enabled, but the selected path is not inside a Git repository."
                        .to_string(),
                );
            }

            let repo_root = match git.get_repo_root(&req.repo_path).await {
                Ok(root) => root,
                Err(err) => return EstimationStreamEvent::Error(err.to_string()),
            };

            let relative_subdir = match (
                std::fs::canonicalize(&req.repo_path),
                std::fs::canonicalize(&repo_root),
            ) {
                (Ok(canon_repo), Ok(canon_root)) => canon_repo
                    .strip_prefix(&canon_root)
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                _ => req
                    .repo_path
                    .strip_prefix(&repo_root)
                    .map(PathBuf::from)
                    .unwrap_or_default(),
            };

            let branch = {
                let b = req.branch.trim();
                if b.is_empty() {
                    "develop".to_string()
                } else {
                    b.to_string()
                }
            };

            let _ = tx.send(EstimationStreamEvent::Status(format!(
                "Fetching branch '{branch}' and creating isolated worktree..."
            )));

            if *cancel_rx.borrow() {
                return EstimationStreamEvent::Cancelled;
            }

            let commit_or_ref = git.fetch_branch_ref(&repo_root, &branch).await;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let session_id = format!("tshirt_{now_ms}");
            let worktree_name = format!("{}_{}", GitService::sanitize_repo_name(&repo_root), session_id);
            let worktree_path = base_dir.join(worktree_name);

            if let Err(err) = git
                .add_worktree(&repo_root, &worktree_path, &commit_or_ref)
                .await
            {
                return EstimationStreamEvent::Error(err.to_string());
            }

            created_worktree = Some((repo_root.clone(), worktree_path.clone()));

            let candidate_path = if relative_subdir.as_os_str().is_empty() {
                worktree_path.clone()
            } else {
                worktree_path.join(&relative_subdir)
            };

            if !candidate_path.is_dir() {
                git.remove_worktree(Some(&repo_root), &worktree_path).await;
                return EstimationStreamEvent::Error(format!(
                    "Subdirectory '{}' does not exist in branch '{}'.",
                    relative_subdir.display(),
                    branch
                ));
            }
            effective_repo_path = candidate_path;
        }
    }

    if *cancel_rx.borrow() {
        if let Some((root, wt)) = created_worktree {
            git.remove_worktree(Some(&root), &wt).await;
        }
        return EstimationStreamEvent::Cancelled;
    }

    let effective_model = resolve_model(&req.model_override, &req.settings.copilot_model);

    let _ = tx.send(EstimationStreamEvent::Status(
        "Starting Copilot estimation session...".to_string(),
    ));

    let session = match copilot
        .create_session(SessionOptions {
            token: req.settings.copilot_token.clone(),
            model: Some(effective_model),
            working_directory: Some(effective_repo_path),
            tools: vec![create_report_intent_tool()],
            available_tools: None,
            streaming: true,
            label: "T-Shirt Size Estimator".to_string(),
        })
        .await
    {
        Ok(session) => session,
        Err(err) => {
            if let Some((root, wt)) = created_worktree {
                git.remove_worktree(Some(&root), &wt).await;
            }
            return EstimationStreamEvent::Error(err.to_string());
        }
    };

    let prompt = build_tshirt_prompt(&req.description);
    let mut final_estimate: Option<(TShirtSize, String)> = None;

    let stream_res = CopilotService::send_and_collect_stream(&session, &prompt, cancel_rx, |ev| {
        match ev {
            StreamEvent::Status(status) => {
                let _ = tx.send(EstimationStreamEvent::Status(status));
            }
            StreamEvent::JsonLine(line) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                    match val.get("type").and_then(|v| v.as_str()) {
                        Some("estimate") => {
                            let size = val
                                .get("size")
                                .and_then(|v| v.as_str())
                                .and_then(TShirtSize::from_str_loose);
                            let text = val
                                .get("text")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                                .unwrap_or_default();
                            if let Some(size) = size {
                                final_estimate = Some((size, text));
                            }
                        }
                        Some("status") => {
                            if let Some(text) = val
                                .get("text")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                let _ = tx.send(EstimationStreamEvent::Status(text.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    })
    .await;

    let _ = session.disconnect().await;
    if let Some((root, wt)) = created_worktree {
        git.remove_worktree(Some(&root), &wt).await;
    }

    match stream_res {
        Ok(usage) => match final_estimate {
            Some((size, reasoning)) => EstimationStreamEvent::Completed(EstimationResult {
                size,
                reasoning,
                usage,
            }),
            None => EstimationStreamEvent::Error(
                "Copilot completed without returning a structured T-shirt size estimate. Please try again."
                    .to_string(),
            ),
        },
        Err(StreamError::Timeout(dur)) => EstimationStreamEvent::TimedOut {
            timeout_secs: dur.as_secs().max(1),
        },
        Err(StreamError::Cancelled) => EstimationStreamEvent::Cancelled,
        Err(StreamError::Other(err)) => EstimationStreamEvent::Error(err.to_string()),
    }
}

fn resolve_model(override_model: &str, default_model: &str) -> String {
    let over = override_model.trim();
    if !over.is_empty() {
        return over.to_string();
    }
    let def = default_model.trim();
    if !def.is_empty() {
        return def.to_string();
    }
    "auto".to_string()
}

#[cfg(test)]
mod tests {
    use super::resolve_model;

    #[test]
    fn resolve_model_preserves_explicit_auto_and_falls_back_cleanly() {
        assert_eq!(resolve_model("auto", "gpt-5.4"), "auto");
        assert_eq!(resolve_model("", "gpt-5.4"), "gpt-5.4");
        assert_eq!(resolve_model("", ""), "auto");
        assert_eq!(resolve_model("auto", "auto"), "auto");
    }
}
