use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use github_copilot_sdk::session::Session;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, watch};

use crate::features::code_explainer::prompts::build_code_explainer_prompt;
use crate::features::story_elaborator::service::{merge_usage, sanitize_worktree_ticket_id};
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, create_report_intent_tool,
    resolve_model_override,
};
use crate::infrastructure::git::GitService;
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage};

#[derive(Debug, Clone)]
pub struct ExplainStartRequest {
    pub repo_path: PathBuf,
    /// Optional branch; only used when git worktrees are enabled in settings.
    pub branch: String,
    pub request: String,
    pub model_override: String,
    pub settings: AppSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainMessage {
    Status(String),
    Question {
        text: String,
        suggested_answers: Vec<String>,
    },
    Documentation(String),
}

#[derive(Debug, Clone)]
pub enum ExplainStreamEvent {
    Status(String),
    Question {
        text: String,
        suggested_answers: Vec<String>,
    },
    Documentation(String),
    TurnCompleted {
        received_question: bool,
        received_documentation: bool,
        cumulative_usage: CopilotUsage,
    },
    TimedOut {
        timeout_secs: u64,
    },
    Cancelled,
    Error(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawExplainMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    suggested_answers: Vec<String>,
}

/// Parses a single JSONL object emitted by the Code Explainer agent.
pub fn parse_explain_jsonl_line(line: &str) -> Option<ExplainMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }
    let raw: RawExplainMessage = serde_json::from_str(trimmed).ok()?;
    let text = raw.text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    match raw.kind.trim() {
        "status" => Some(ExplainMessage::Status(text)),
        "question" => {
            let suggested_answers = raw
                .suggested_answers
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(ExplainMessage::Question {
                text,
                suggested_answers,
            })
        }
        "documentation" | "doc" => Some(ExplainMessage::Documentation(text)),
        _ => None,
    }
}

struct ActiveExplainEntry {
    session_id: u64,
    cancel_tx: watch::Sender<bool>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    worktree_info: Arc<Mutex<Option<(PathBuf, PathBuf)>>>,
    cumulative_usage: Arc<Mutex<CopilotUsage>>,
    turn_lock: Arc<Mutex<()>>,
}

/// Manages the single active, multi-turn Code Explainer Copilot session.
pub struct CodeExplainerService {
    copilot: Arc<CopilotService>,
    git: Arc<GitService>,
    next_session_id: AtomicU64,
    active: Mutex<Option<ActiveExplainEntry>>,
}

async fn cleanup_worktree_slot(git: &GitService, slot: &Mutex<Option<(PathBuf, PathBuf)>>) {
    let worktree = { slot.lock().await.take() };
    if let Some((repo_root, worktree_path)) = worktree {
        git.remove_worktree(Some(&repo_root), &worktree_path).await;
    }
}

impl CodeExplainerService {
    pub fn new(copilot: Arc<CopilotService>, git: Arc<GitService>) -> Self {
        Self {
            copilot,
            git,
            next_session_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    pub fn start(
        self: &Arc<Self>,
        req: ExplainStartRequest,
    ) -> mpsc::UnboundedReceiver<ExplainStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let session_id = service.next_session_id.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let session_slot = Arc::new(Mutex::new(None));
            let worktree_slot = Arc::new(Mutex::new(None));
            let cumulative_usage = Arc::new(Mutex::new(CopilotUsage::default()));
            let turn_lock = Arc::new(Mutex::new(()));

            let previous = {
                let mut guard = service.active.lock().await;
                guard.replace(ActiveExplainEntry {
                    session_id,
                    cancel_tx: cancel_tx.clone(),
                    session: Arc::clone(&session_slot),
                    worktree_info: Arc::clone(&worktree_slot),
                    cumulative_usage: Arc::clone(&cumulative_usage),
                    turn_lock: Arc::clone(&turn_lock),
                })
            };
            if let Some(prev) = previous {
                let _ = prev.cancel_tx.send(true);
                if let Some(s) = prev.session.lock().await.take() {
                    let _ = s.disconnect().await;
                }
                cleanup_worktree_slot(&service.git, &prev.worktree_info).await;
            }

            let working_directory = match prepare_repo_directory(
                &service.git,
                &req,
                session_id,
                &worktree_slot,
                &cancel_rx,
                &tx,
            )
            .await
            {
                Ok(dir) => dir,
                Err(terminal) => {
                    cleanup_worktree_slot(&service.git, &worktree_slot).await;
                    service.remove_if_matching(session_id).await;
                    let _ = tx.send(terminal);
                    return;
                }
            };

            let effective_model =
                resolve_model_override(&req.model_override, &req.settings.copilot_model);

            let _ = tx.send(ExplainStreamEvent::Status(
                "Creating Copilot explainer session...".to_string(),
            ));

            let session = match service
                .copilot
                .create_session(SessionOptions {
                    token: req.settings.copilot_token.clone(),
                    model: Some(effective_model),
                    working_directory: Some(working_directory),
                    tools: vec![create_report_intent_tool()],
                    available_tools: None,
                    streaming: true,
                    label: "Code Explainer".to_string(),
                })
                .await
            {
                Ok(s) => Arc::new(s),
                Err(err) => {
                    cleanup_worktree_slot(&service.git, &worktree_slot).await;
                    service.remove_if_matching(session_id).await;
                    let _ = tx.send(ExplainStreamEvent::Error(err.to_string()));
                    return;
                }
            };

            if *cancel_rx.borrow() {
                cleanup_worktree_slot(&service.git, &worktree_slot).await;
                service.remove_if_matching(session_id).await;
                let _ = session.disconnect().await;
                let _ = tx.send(ExplainStreamEvent::Cancelled);
                return;
            }

            *session_slot.lock().await = Some(Arc::clone(&session));

            let prompt = build_code_explainer_prompt(
                &req.request,
                &req.settings.prompts.code_explainer.general,
            );

            let _turn_guard = turn_lock.lock().await;
            let terminal = run_turn(&session, &prompt, cancel_rx, &cumulative_usage, &tx).await;
            let _ = tx.send(terminal);
        });

        rx
    }

    pub fn send_answer(
        self: &Arc<Self>,
        answer: String,
    ) -> mpsc::UnboundedReceiver<ExplainStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let lookup = {
                let guard = service.active.lock().await;
                guard.as_ref().map(|e| {
                    (
                        Arc::clone(&e.session),
                        Arc::clone(&e.cumulative_usage),
                        Arc::clone(&e.turn_lock),
                        e.cancel_tx.subscribe(),
                    )
                })
            };
            let Some((session_slot, cumulative_usage, turn_lock, cancel_rx)) = lookup else {
                let _ = tx.send(ExplainStreamEvent::Error(
                    "No active explainer session found. Please start a new session.".to_string(),
                ));
                return;
            };
            let session = { session_slot.lock().await.clone() };
            let Some(session) = session else {
                let _ = tx.send(ExplainStreamEvent::Error(
                    "Explainer session is not ready yet.".to_string(),
                ));
                return;
            };

            let _turn_guard = turn_lock.lock().await;
            let terminal = run_turn(&session, &answer, cancel_rx, &cumulative_usage, &tx).await;
            let _ = tx.send(terminal);
        });

        rx
    }

    pub async fn stop(&self) -> Option<CopilotUsage> {
        let entry = { self.active.lock().await.take() }?;
        let _ = entry.cancel_tx.send(true);
        if let Some(session) = entry.session.lock().await.take() {
            let _ = session.disconnect().await;
        }
        cleanup_worktree_slot(&self.git, &entry.worktree_info).await;
        let usage = entry.cumulative_usage.lock().await.clone();
        Some(usage)
    }

    async fn remove_if_matching(&self, session_id: u64) {
        let mut guard = self.active.lock().await;
        if guard.as_ref().is_some_and(|e| e.session_id == session_id) {
            *guard = None;
        }
    }
}

async fn prepare_repo_directory(
    git: &GitService,
    req: &ExplainStartRequest,
    session_id: u64,
    worktree_slot: &Mutex<Option<(PathBuf, PathBuf)>>,
    cancel_rx: &watch::Receiver<bool>,
    tx: &mpsc::UnboundedSender<ExplainStreamEvent>,
) -> Result<PathBuf, ExplainStreamEvent> {
    let repo_dir = req.repo_path.as_path();
    if !repo_dir.is_dir() {
        return Err(ExplainStreamEvent::Error(format!(
            "Repository path does not exist or is not a directory: {}",
            repo_dir.display()
        )));
    }

    let branch = req.branch.trim();
    if branch.is_empty() || !req.settings.git_worktree_enabled {
        return Ok(repo_dir.to_path_buf());
    }
    let Some(base_dir) = req.settings.git_worktree_base_dir.clone() else {
        return Ok(repo_dir.to_path_buf());
    };
    if !git.check_git_repo(repo_dir).await {
        return Ok(repo_dir.to_path_buf());
    }

    let repo_root = git
        .get_repo_root(repo_dir)
        .await
        .map_err(|err| ExplainStreamEvent::Error(err.to_string()))?;

    let relative_subdir = match (
        std::fs::canonicalize(repo_dir),
        std::fs::canonicalize(&repo_root),
    ) {
        (Ok(canon_repo), Ok(canon_root)) => canon_repo
            .strip_prefix(&canon_root)
            .map(PathBuf::from)
            .unwrap_or_default(),
        _ => repo_dir
            .strip_prefix(&repo_root)
            .map(PathBuf::from)
            .unwrap_or_default(),
    };

    let _ = tx.send(ExplainStreamEvent::Status(format!(
        "Fetching branch '{branch}' and creating isolated worktree..."
    )));
    if *cancel_rx.borrow() {
        return Err(ExplainStreamEvent::Cancelled);
    }

    let commit_or_ref = git.fetch_branch_ref(&repo_root, branch).await;
    if *cancel_rx.borrow() {
        return Err(ExplainStreamEvent::Cancelled);
    }

    let worktree_name = format!(
        "{}_explain_{}",
        GitService::sanitize_repo_name(&repo_root),
        sanitize_worktree_ticket_id(&session_id.to_string())
    );
    let worktree_path = base_dir.join(worktree_name);

    git.add_worktree(&repo_root, &worktree_path, &commit_or_ref)
        .await
        .map_err(|err| ExplainStreamEvent::Error(err.to_string()))?;

    *worktree_slot.lock().await = Some((repo_root, worktree_path.clone()));

    if *cancel_rx.borrow() {
        return Err(ExplainStreamEvent::Cancelled);
    }

    let candidate = if relative_subdir.as_os_str().is_empty() {
        worktree_path
    } else {
        worktree_path.join(&relative_subdir)
    };
    if !candidate.is_dir() {
        return Err(ExplainStreamEvent::Error(
            "The selected directory does not exist in the checked out branch. Your local repository might be outdated."
                .to_string(),
        ));
    }
    Ok(candidate)
}

async fn run_turn(
    session: &Session,
    prompt: &str,
    cancel_rx: watch::Receiver<bool>,
    cumulative_usage: &Arc<Mutex<CopilotUsage>>,
    tx: &mpsc::UnboundedSender<ExplainStreamEvent>,
) -> ExplainStreamEvent {
    if *cancel_rx.borrow() {
        return ExplainStreamEvent::Cancelled;
    }

    let mut received_question = false;
    let mut received_documentation = false;

    let stream_res = CopilotService::send_and_collect_stream(session, prompt, cancel_rx, |ev| {
        match ev {
            StreamEvent::Status(status) => {
                let _ = tx.send(ExplainStreamEvent::Status(status));
            }
            StreamEvent::JsonLine(line) => {
                if let Some(msg) = parse_explain_jsonl_line(&line) {
                    match msg {
                        ExplainMessage::Status(s) => {
                            let _ = tx.send(ExplainStreamEvent::Status(s));
                        }
                        ExplainMessage::Question {
                            text,
                            suggested_answers,
                        } => {
                            received_question = true;
                            let _ = tx.send(ExplainStreamEvent::Question {
                                text,
                                suggested_answers,
                            });
                        }
                        ExplainMessage::Documentation(text) => {
                            received_documentation = true;
                            let _ = tx.send(ExplainStreamEvent::Documentation(text));
                        }
                    }
                }
            }
        }
    })
    .await;

    match stream_res {
        Ok(turn_usage) => {
            let updated = {
                let mut total = cumulative_usage.lock().await;
                merge_usage(&mut total, &turn_usage);
                total.clone()
            };
            ExplainStreamEvent::TurnCompleted {
                received_question,
                received_documentation,
                cumulative_usage: updated,
            }
        }
        Err(StreamError::Timeout(dur)) => ExplainStreamEvent::TimedOut {
            timeout_secs: dur.as_secs().max(1),
        },
        Err(StreamError::Cancelled) => ExplainStreamEvent::Cancelled,
        Err(StreamError::Other(err)) => ExplainStreamEvent::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question_documentation_status_and_noise() {
        let q = r#"{"type": "question", "text": " Which module? ", "suggestedAnswers": ["A", " ", "B"]}"#;
        assert_eq!(
            parse_explain_jsonl_line(q),
            Some(ExplainMessage::Question {
                text: "Which module?".to_string(),
                suggested_answers: vec!["A".to_string(), "B".to_string()],
            })
        );
        let d = r##"{"type": "documentation", "text": " # Doc "}"##;
        assert_eq!(
            parse_explain_jsonl_line(d),
            Some(ExplainMessage::Documentation("# Doc".to_string()))
        );
        assert_eq!(
            parse_explain_jsonl_line(r#"{"type": "status", "text": "Reading"}"#),
            Some(ExplainMessage::Status("Reading".to_string()))
        );
        assert!(parse_explain_jsonl_line("```json").is_none());
        assert!(parse_explain_jsonl_line("not json").is_none());
        assert!(parse_explain_jsonl_line(r#"{"type": "other", "text": "x"}"#).is_none());
        assert!(parse_explain_jsonl_line(r#"{"type": "documentation", "text": "  "}"#).is_none());
    }
}
