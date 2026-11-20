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
    resolve_model_override,
};
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage, TicketData};

#[derive(Debug, Clone)]
pub struct ElaborationStartRequest {
    pub ticket_data: TicketData,
    #[allow(dead_code)] // Wired to Git worktrees in Phase 8
    pub repo_path: Option<PathBuf>,
    #[allow(dead_code)] // Wired to Git worktrees in Phase 8
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

fn normalize_ticket_key(ticket_id: Option<&str>) -> String {
    ticket_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string()
}

struct ActiveElaborationEntry {
    session_id: u64,
    cancel_tx: watch::Sender<bool>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    cumulative_usage: Arc<Mutex<CopilotUsage>>,
    turn_lock: Arc<Mutex<()>>,
}

/// Manages stateful, multi-turn Copilot elaboration sessions keyed by ticket ID.
pub struct StoryElaboratorService {
    copilot: Arc<CopilotService>,
    next_session_id: AtomicU64,
    active_sessions: Mutex<HashMap<String, ActiveElaborationEntry>>,
}

impl StoryElaboratorService {
    pub fn new(copilot: Arc<CopilotService>) -> Self {
        Self {
            copilot,
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
            let cumulative_usage = Arc::new(Mutex::new(CopilotUsage::default()));
            let turn_lock = Arc::new(Mutex::new(()));

            let previous = {
                let mut guard = service.active_sessions.lock().await;
                guard.insert(
                    ticket_key.clone(),
                    ActiveElaborationEntry {
                        session_id,
                        cancel_tx: cancel_tx.clone(),
                        session: Arc::clone(&session_slot),
                        cumulative_usage: Arc::clone(&cumulative_usage),
                        turn_lock: Arc::clone(&turn_lock),
                    },
                )
            };

            if let Some(prev) = previous {
                let _ = prev.cancel_tx.send(true);
                let prev_session = prev.session.lock().await.take();
                if let Some(s) = prev_session {
                    let _ = s.disconnect().await;
                }
            }

            if *cancel_rx.borrow() {
                service.remove_if_matching(&ticket_key, session_id).await;
                let _ = tx.send(ElaborationStreamEvent::Cancelled);
                return;
            }

            let effective_model =
                resolve_model_override(&req.model_override, &req.settings.copilot_model);

            let session = match service
                .copilot
                .create_session(SessionOptions {
                    token: req.settings.copilot_token.clone(),
                    model: Some(effective_model),
                    working_directory: None,
                    tools: vec![create_report_intent_tool()],
                    available_tools: Some(vec!["custom:report_intent".to_string()]),
                    streaming: true,
                    label: "Story Elaborator".to_string(),
                })
                .await
            {
                Ok(session) => Arc::new(session),
                Err(err) => {
                    service.remove_if_matching(&ticket_key, session_id).await;
                    let _ = tx.send(ElaborationStreamEvent::Error(err.to_string()));
                    return;
                }
            };

            if *cancel_rx.borrow() {
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
                false,
                &[],
            );

            let _turn_guard = turn_lock.lock().await;
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
                        entry.cancel_tx.subscribe(),
                    )
                })
            };

            let Some((session_slot, cumulative_usage, turn_lock, cancel_rx)) = lookup else {
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
        let session = entry.session.lock().await.take();
        if let Some(session) = session {
            let _ = session.disconnect().await;
        }
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
}
