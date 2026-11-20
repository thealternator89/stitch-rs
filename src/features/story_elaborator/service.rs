use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use github_copilot_sdk::session::Session;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, watch};

use crate::features::story_elaborator::prompts::build_story_elaborator_prompt;
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, create_report_intent_tool,
    create_request_documentation_tool, resolve_model_override,
};
use crate::infrastructure::git::GitService;
use crate::infrastructure::providers::{build_documentation_provider, discover_known_docs};
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage, KnownDoc, TicketData};

type TurnSenderSlot = Arc<std::sync::Mutex<Option<mpsc::UnboundedSender<ElaborationStreamEvent>>>>;

#[derive(Debug, Clone)]
pub struct ElaborationStartRequest {
    pub ticket_data: TicketData,
    pub repo_path: Option<PathBuf>,
    pub branch: String,
    pub additional_context: String,
    pub model_override: String,
    pub settings: AppSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElaborationMessage {
    Status(String),
    Question {
        text: String,
        suggested_answers: Vec<String>,
    },
    Plan {
        text: String,
        file_path: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum ElaborationStreamEvent {
    Status(String),
    Question {
        text: String,
        suggested_answers: Vec<String>,
    },
    Plan {
        text: String,
        file_path: Option<String>,
    },
    TurnCompleted {
        received_question: bool,
        received_plan: bool,
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
struct RawElaborationMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    suggested_answers: Vec<String>,
    #[serde(default)]
    file_path: Option<String>,
}

/// Parses a single JSONL object emitted during Story Elaboration.
pub fn parse_elaboration_jsonl_line(line: &str) -> Option<ElaborationMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }

    let raw: RawElaborationMessage = serde_json::from_str(trimmed).ok()?;
    let text = raw.text.trim().to_string();
    if text.is_empty() {
        return None;
    }

    match raw.kind.trim() {
        "status" => Some(ElaborationMessage::Status(text)),
        "question" => {
            let suggested_answers = raw
                .suggested_answers
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(ElaborationMessage::Question {
                text,
                suggested_answers,
            })
        }
        "plan" => {
            let file_path = raw
                .file_path
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
            Some(ElaborationMessage::Plan { text, file_path })
        }
        _ => None,
    }
}

/// Adds a completed turn's token and cost metrics into the cumulative session totals.
pub fn merge_usage(cumulative: &mut CopilotUsage, turn: &CopilotUsage) {
    cumulative.input_tokens = cumulative.input_tokens.saturating_add(turn.input_tokens);
    cumulative.output_tokens = cumulative.output_tokens.saturating_add(turn.output_tokens);
    cumulative.cache_read_tokens = cumulative
        .cache_read_tokens
        .saturating_add(turn.cache_read_tokens);
    cumulative.cost += turn.cost;
    if let Some(model) = &turn.model {
        cumulative.model = Some(model.clone());
    }
}

/// Sanitizes a ticket ID so GitHub composite IDs (`owner/repo/42`) produce flat worktree names.
pub fn sanitize_worktree_ticket_id(ticket_id: &str) -> String {
    let sanitized: String = ticket_id
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

/// Returns `(working_directory, available_tools)` for dual codebase vs. ticket-only session mode.
pub fn resolve_session_tool_config(
    effective_repo_path: Option<PathBuf>,
) -> (Option<PathBuf>, Option<Vec<String>>) {
    match effective_repo_path {
        Some(dir) => (Some(dir), None),
        None => (
            None,
            Some(vec![
                "custom:request_documentation".to_string(),
                "custom:report_intent".to_string(),
            ]),
        ),
    }
}

fn normalize_ticket_key(ticket_id: Option<&str>) -> String {
    ticket_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string()
}

async fn cleanup_worktree_slot(git: &GitService, slot: &Mutex<Option<(PathBuf, PathBuf)>>) {
    let worktree = { slot.lock().await.take() };
    if let Some((repo_root, worktree_path)) = worktree {
        git.remove_worktree(Some(&repo_root), &worktree_path).await;
    }
}

struct TurnTxGuard {
    slot: TurnSenderSlot,
}

impl TurnTxGuard {
    fn install(slot: TurnSenderSlot, tx: &mpsc::UnboundedSender<ElaborationStreamEvent>) -> Self {
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(tx.clone());
        }
        Self { slot }
    }
}

impl Drop for TurnTxGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.slot.lock() {
            *guard = None;
        }
    }
}

struct ActiveElaborationEntry {
    session_id: u64,
    cancel_tx: watch::Sender<bool>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    worktree_info: Arc<Mutex<Option<(PathBuf, PathBuf)>>>,
    cumulative_usage: Arc<Mutex<CopilotUsage>>,
    turn_lock: Arc<Mutex<()>>,
    current_tx: TurnSenderSlot,
}

/// Manages stateful, multi-turn Copilot elaboration sessions keyed by ticket ID.
pub struct StoryElaboratorService {
    copilot: Arc<CopilotService>,
    git: Arc<GitService>,
    next_session_id: AtomicU64,
    active_sessions: Mutex<HashMap<String, ActiveElaborationEntry>>,
}

impl StoryElaboratorService {
    pub fn new(copilot: Arc<CopilotService>, git: Arc<GitService>) -> Self {
        Self {
            copilot,
            git,
            next_session_id: AtomicU64::new(1),
            active_sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn start_elaboration(
        self: &Arc<Self>,
        req: ElaborationStartRequest,
    ) -> mpsc::UnboundedReceiver<ElaborationStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let ticket_key = normalize_ticket_key(req.ticket_data.id.as_deref());
            let session_id = service.next_session_id.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let session_slot = Arc::new(Mutex::new(None));
            let worktree_slot = Arc::new(Mutex::new(None));
            let cumulative_usage = Arc::new(Mutex::new(CopilotUsage::default()));
            let turn_lock = Arc::new(Mutex::new(()));
            let current_tx_slot: TurnSenderSlot = Arc::new(std::sync::Mutex::new(None));

            let previous = {
                let mut guard = service.active_sessions.lock().await;
                guard.insert(
                    ticket_key.clone(),
                    ActiveElaborationEntry {
                        session_id,
                        cancel_tx: cancel_tx.clone(),
                        session: Arc::clone(&session_slot),
                        worktree_info: Arc::clone(&worktree_slot),
                        cumulative_usage: Arc::clone(&cumulative_usage),
                        turn_lock: Arc::clone(&turn_lock),
                        current_tx: Arc::clone(&current_tx_slot),
                    },
                )
            };

            if let Some(prev) = previous {
                let _ = prev.cancel_tx.send(true);
                let prev_session = prev.session.lock().await.take();
                if let Some(s) = prev_session {
                    let _ = s.disconnect().await;
                }
                cleanup_worktree_slot(&service.git, &prev.worktree_info).await;
            }

            if *cancel_rx.borrow() {
                service.remove_if_matching(&ticket_key, session_id).await;
                let _ = tx.send(ElaborationStreamEvent::Cancelled);
                return;
            }

            let effective_repo_path = match prepare_repo_directory(
                &service.git,
                req.repo_path.as_deref(),
                &req.branch,
                &ticket_key,
                &req.settings,
                &worktree_slot,
                &cancel_rx,
                &tx,
            )
            .await
            {
                Ok(dir) => dir,
                Err(terminal) => {
                    cleanup_worktree_slot(&service.git, &worktree_slot).await;
                    service.remove_if_matching(&ticket_key, session_id).await;
                    let _ = tx.send(terminal);
                    return;
                }
            };

            let doc_provider = build_documentation_provider(&req.settings).ok();
            let known_docs: Vec<KnownDoc> = if let Some(ref provider) = doc_provider {
                let ac = req.ticket_data.acceptance_criteria.as_deref().unwrap_or("");
                let discovered =
                    discover_known_docs(provider.as_ref(), &[&req.ticket_data.description, ac])
                        .await;
                if !discovered.is_empty() {
                    let _ = tx.send(ElaborationStreamEvent::Status(format!(
                        "Discovered {} Confluence document(s) linked in ticket.",
                        discovered.len()
                    )));
                }
                discovered
            } else {
                Vec::new()
            };

            if *cancel_rx.borrow() {
                cleanup_worktree_slot(&service.git, &worktree_slot).await;
                service.remove_if_matching(&ticket_key, session_id).await;
                let _ = tx.send(ElaborationStreamEvent::Cancelled);
                return;
            }

            let status_slot = Arc::clone(&current_tx_slot);
            let status_callback: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |status| {
                if let Ok(guard) = status_slot.lock() {
                    if let Some(turn_tx) = guard.as_ref() {
                        let _ = turn_tx.send(ElaborationStreamEvent::Status(status));
                    }
                }
            });

            let (working_directory, available_tools) =
                resolve_session_tool_config(effective_repo_path);
            let has_repo = working_directory.is_some();
            let effective_model =
                resolve_model_override(&req.model_override, &req.settings.copilot_model);

            let _ = tx.send(ElaborationStreamEvent::Status(
                "Creating Copilot elaboration session...".to_string(),
            ));

            let session = match service
                .copilot
                .create_session(SessionOptions {
                    token: req.settings.copilot_token.clone(),
                    model: Some(effective_model),
                    working_directory,
                    tools: vec![
                        create_request_documentation_tool(doc_provider, status_callback),
                        create_report_intent_tool(),
                    ],
                    available_tools,
                    streaming: true,
                    label: "Story Elaborator".to_string(),
                })
                .await
            {
                Ok(session) => Arc::new(session),
                Err(err) => {
                    cleanup_worktree_slot(&service.git, &worktree_slot).await;
                    service.remove_if_matching(&ticket_key, session_id).await;
                    let _ = tx.send(ElaborationStreamEvent::Error(err.to_string()));
                    return;
                }
            };

            if *cancel_rx.borrow() {
                cleanup_worktree_slot(&service.git, &worktree_slot).await;
                service.remove_if_matching(&ticket_key, session_id).await;
                let _ = session.disconnect().await;
                let _ = tx.send(ElaborationStreamEvent::Cancelled);
                return;
            }

            {
                let mut slot = session_slot.lock().await;
                *slot = Some(Arc::clone(&session));
            }

            let prompt = build_story_elaborator_prompt(
                &req.ticket_data,
                &req.additional_context,
                &req.settings.prompts.story_elaborator.general,
                has_repo,
                &known_docs,
            );

            let _turn_guard = turn_lock.lock().await;
            let _tx_guard = TurnTxGuard::install(current_tx_slot, &tx);
            let terminal =
                run_elaboration_turn(&session, &prompt, cancel_rx, &cumulative_usage, &tx).await;
            let _ = tx.send(terminal);
        });

        rx
    }

    pub fn send_answer(
        self: &Arc<Self>,
        ticket_id: String,
        answer: String,
    ) -> mpsc::UnboundedReceiver<ElaborationStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let ticket_key = normalize_ticket_key(Some(&ticket_id));
            let lookup = {
                let guard = service.active_sessions.lock().await;
                guard.get(&ticket_key).map(|entry| {
                    (
                        Arc::clone(&entry.session),
                        Arc::clone(&entry.cumulative_usage),
                        Arc::clone(&entry.turn_lock),
                        Arc::clone(&entry.current_tx),
                        entry.cancel_tx.subscribe(),
                    )
                })
            };

            let Some((session_slot, cumulative_usage, turn_lock, current_tx_slot, cancel_rx)) =
                lookup
            else {
                let _ = tx.send(ElaborationStreamEvent::Error(
                    "No active story elaboration session found for this ticket. Please start a new session."
                        .to_string(),
                ));
                return;
            };

            let session = { session_slot.lock().await.clone() };
            let Some(session) = session else {
                let _ = tx.send(ElaborationStreamEvent::Error(
                    "Elaboration session is not ready yet.".to_string(),
                ));
                return;
            };

            let _turn_guard = turn_lock.lock().await;
            let _tx_guard = TurnTxGuard::install(current_tx_slot, &tx);
            let terminal =
                run_elaboration_turn(&session, &answer, cancel_rx, &cumulative_usage, &tx).await;
            let _ = tx.send(terminal);
        });

        rx
    }

    pub async fn stop_elaboration(&self, ticket_id: &str) -> Option<CopilotUsage> {
        let ticket_key = normalize_ticket_key(Some(ticket_id));
        let entry = {
            let mut guard = self.active_sessions.lock().await;
            guard.remove(&ticket_key)
        }?;

        let _ = entry.cancel_tx.send(true);
        let session = { entry.session.lock().await.take() };
        if let Some(session) = session {
            let _ = session.disconnect().await;
        }
        cleanup_worktree_slot(&self.git, &entry.worktree_info).await;
        let usage = entry.cumulative_usage.lock().await.clone();
        Some(usage)
    }

    async fn remove_if_matching(&self, ticket_key: &str, session_id: u64) {
        let mut guard = self.active_sessions.lock().await;
        if guard
            .get(ticket_key)
            .is_some_and(|entry| entry.session_id == session_id)
        {
            guard.remove(ticket_key);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_repo_directory(
    git: &GitService,
    repo_path: Option<&std::path::Path>,
    branch: &str,
    ticket_key: &str,
    settings: &AppSettings,
    worktree_slot: &Mutex<Option<(PathBuf, PathBuf)>>,
    cancel_rx: &watch::Receiver<bool>,
    tx: &mpsc::UnboundedSender<ElaborationStreamEvent>,
) -> Result<Option<PathBuf>, ElaborationStreamEvent> {
    let Some(repo_dir) = repo_path else {
        return Ok(None);
    };

    if !repo_dir.is_dir() {
        return Err(ElaborationStreamEvent::Error(format!(
            "Repository path does not exist or is not a directory: {}",
            repo_dir.display()
        )));
    }

    if !settings.git_worktree_enabled {
        return Ok(Some(repo_dir.to_path_buf()));
    }

    let Some(base_dir) = settings.git_worktree_base_dir.clone() else {
        return Ok(Some(repo_dir.to_path_buf()));
    };

    if !git.check_git_repo(repo_dir).await {
        return Ok(Some(repo_dir.to_path_buf()));
    }

    let repo_root = git
        .get_repo_root(repo_dir)
        .await
        .map_err(|err| ElaborationStreamEvent::Error(err.to_string()))?;

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

    let target_branch = {
        let b = branch.trim();
        if b.is_empty() { "develop" } else { b }
    };

    let _ = tx.send(ElaborationStreamEvent::Status(format!(
        "Fetching branch '{target_branch}' and creating isolated worktree..."
    )));

    if *cancel_rx.borrow() {
        return Err(ElaborationStreamEvent::Cancelled);
    }

    let commit_or_ref = git.fetch_branch_ref(&repo_root, target_branch).await;
    if *cancel_rx.borrow() {
        return Err(ElaborationStreamEvent::Cancelled);
    }

    let sanitized_ticket = sanitize_worktree_ticket_id(ticket_key);
    let worktree_name = format!(
        "{}_ticket_{}",
        GitService::sanitize_repo_name(&repo_root),
        sanitized_ticket
    );
    let worktree_path = base_dir.join(worktree_name);

    git.add_worktree(&repo_root, &worktree_path, &commit_or_ref)
        .await
        .map_err(|err| ElaborationStreamEvent::Error(err.to_string()))?;

    {
        let mut slot = worktree_slot.lock().await;
        *slot = Some((repo_root, worktree_path.clone()));
    }

    if *cancel_rx.borrow() {
        return Err(ElaborationStreamEvent::Cancelled);
    }

    let candidate_path = if relative_subdir.as_os_str().is_empty() {
        worktree_path
    } else {
        worktree_path.join(&relative_subdir)
    };

    if !candidate_path.is_dir() {
        return Err(ElaborationStreamEvent::Error(
            "The selected directory does not exist in the checked out branch. Your local repository might be outdated. Please pull the branch and try again."
                .to_string(),
        ));
    }

    Ok(Some(candidate_path))
}

async fn run_elaboration_turn(
    session: &Session,
    prompt: &str,
    cancel_rx: watch::Receiver<bool>,
    cumulative_usage: &Arc<Mutex<CopilotUsage>>,
    tx: &mpsc::UnboundedSender<ElaborationStreamEvent>,
) -> ElaborationStreamEvent {
    if *cancel_rx.borrow() {
        return ElaborationStreamEvent::Cancelled;
    }

    let mut received_question = false;
    let mut received_plan = false;

    let stream_res = CopilotService::send_and_collect_stream(session, prompt, cancel_rx, |ev| {
        match ev {
            StreamEvent::Status(status) => {
                let _ = tx.send(ElaborationStreamEvent::Status(status));
            }
            StreamEvent::JsonLine(line) => {
                if let Some(msg) = parse_elaboration_jsonl_line(&line) {
                    match msg {
                        ElaborationMessage::Status(status) => {
                            let _ = tx.send(ElaborationStreamEvent::Status(status));
                        }
                        ElaborationMessage::Question {
                            text,
                            suggested_answers,
                        } => {
                            received_question = true;
                            let _ = tx.send(ElaborationStreamEvent::Question {
                                text,
                                suggested_answers,
                            });
                        }
                        ElaborationMessage::Plan { text, file_path } => {
                            received_plan = true;
                            let _ = tx.send(ElaborationStreamEvent::Plan { text, file_path });
                        }
                    }
                }
            }
        }
    })
    .await;

    match stream_res {
        Ok(turn_usage) => {
            let updated_usage = {
                let mut total = cumulative_usage.lock().await;
                merge_usage(&mut total, &turn_usage);
                total.clone()
            };
            ElaborationStreamEvent::TurnCompleted {
                received_question,
                received_plan,
                cumulative_usage: updated_usage,
            }
        }
        Err(StreamError::Timeout(dur)) => ElaborationStreamEvent::TimedOut {
            timeout_secs: dur.as_secs().max(1),
        },
        Err(StreamError::Cancelled) => ElaborationStreamEvent::Cancelled,
        Err(StreamError::Other(err)) => ElaborationStreamEvent::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_elaboration_jsonl_line_handles_question_plan_status_and_noise() {
        let q = r#"{"type": "question", "text": "  Which cache store?  ", "suggestedAnswers": ["Redis", "  ", "In-memory"]}"#;
        assert_eq!(
            parse_elaboration_jsonl_line(q),
            Some(ElaborationMessage::Question {
                text: "Which cache store?".to_string(),
                suggested_answers: vec!["Redis".to_string(), "In-memory".to_string()],
            })
        );

        let p = r#"{"type": "plan", "text": " # Plan\n\n* Step 1 ", "filePath": " /tmp/plan.md "}"#;
        assert_eq!(
            parse_elaboration_jsonl_line(p),
            Some(ElaborationMessage::Plan {
                text: "# Plan\n\n* Step 1".to_string(),
                file_path: Some("/tmp/plan.md".to_string()),
            })
        );

        let s = r#"{"type": "status", "text": "Analyzing ticket"}"#;
        assert_eq!(
            parse_elaboration_jsonl_line(s),
            Some(ElaborationMessage::Status("Analyzing ticket".to_string()))
        );

        assert!(parse_elaboration_jsonl_line("```json").is_none());
        assert!(parse_elaboration_jsonl_line("   ").is_none());
        assert!(parse_elaboration_jsonl_line(r#"{"type": "question", "text": "  "}"#).is_none());
        assert!(parse_elaboration_jsonl_line(r#"{"type": "unknown", "text": "hi"}"#).is_none());
        assert!(parse_elaboration_jsonl_line("not json").is_none());
    }

    #[test]
    fn merge_usage_accumulates_tokens_cost_and_latest_model() {
        let mut total = CopilotUsage::default();
        merge_usage(
            &mut total,
            &CopilotUsage {
                input_tokens: 1000,
                output_tokens: 250,
                cache_read_tokens: 400,
                cost: 0.25,
                model: Some("gpt-5.4".to_string()),
                phases: None,
            },
        );
        merge_usage(
            &mut total,
            &CopilotUsage {
                input_tokens: 500,
                output_tokens: 150,
                cache_read_tokens: 100,
                cost: 0.75,
                model: None,
                phases: None,
            },
        );

        assert_eq!(total.input_tokens, 1500);
        assert_eq!(total.output_tokens, 400);
        assert_eq!(total.cache_read_tokens, 500);
        assert!((total.cost - 1.0).abs() < 1e-9);
        assert_eq!(total.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn sanitize_worktree_ticket_id_replaces_slashes_and_special_chars() {
        assert_eq!(sanitize_worktree_ticket_id("421"), "421");
        assert_eq!(
            sanitize_worktree_ticket_id("my-org/web-app/108"),
            "my-org_web-app_108"
        );
        assert_eq!(sanitize_worktree_ticket_id("   "), "unknown");
    }

    #[test]
    fn resolve_session_tool_config_switches_between_codebase_and_ticket_only_modes() {
        let repo = PathBuf::from("/tmp/repo");
        let (dir, tools) = resolve_session_tool_config(Some(repo.clone()));
        assert_eq!(dir, Some(repo));
        assert_eq!(tools, None);

        let (no_dir, locked_tools) = resolve_session_tool_config(None);
        assert_eq!(no_dir, None);
        assert_eq!(
            locked_tools,
            Some(vec![
                "custom:request_documentation".to_string(),
                "custom:report_intent".to_string(),
            ])
        );
    }
}
