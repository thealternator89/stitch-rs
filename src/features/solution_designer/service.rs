use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use github_copilot_sdk::session::Session;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, watch};

use crate::features::solution_designer::prompts::{
    build_area_design_prompt, build_ground_rules_prompt, build_new_area_prompt,
    build_standalone_refinement_prompt,
};
use crate::features::solution_designer::storage::{DesignArea, LoadedProject, ReportedArea};
use crate::features::story_elaborator::service::merge_usage;
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, create_report_intent_tool,
    resolve_model_override,
};
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage};

#[derive(Debug, Clone)]
pub enum DesignSessionKind {
    GroundRules {
        project_name: String,
        initial_context: String,
    },
    AreaDesign {
        project: LoadedProject,
        area: DesignArea,
        extra_context: String,
        is_redesign: bool,
    },
    NewArea {
        project: LoadedProject,
        title: String,
        suggested_file: String,
        extra_context: String,
    },
    StandaloneRefine {
        project: LoadedProject,
        target_title: String,
        target_file: String,
        is_readme: bool,
        instructions: String,
    },
}

#[derive(Debug, Clone)]
pub struct DesignStartRequest {
    pub project_dir: PathBuf,
    pub kind: DesignSessionKind,
    pub model_override: String,
    pub settings: AppSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesignMessage {
    Status(String),
    Question {
        text: String,
        suggested_answers: Vec<String>,
    },
    Complete {
        file: Option<String>,
        summary: Option<String>,
        areas: Vec<ReportedArea>,
    },
}

#[derive(Debug, Clone)]
pub enum DesignStreamEvent {
    Status(String),
    Question {
        text: String,
        suggested_answers: Vec<String>,
    },
    Complete {
        file: Option<String>,
        summary: Option<String>,
        areas: Vec<ReportedArea>,
    },
    TurnCompleted {
        received_question: bool,
        received_complete: bool,
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
struct RawDesignMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    suggested_answers: Vec<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    areas: Vec<ReportedArea>,
}

/// Parses a single JSONL object emitted by the Solution Designer agent.
pub fn parse_design_jsonl_line(line: &str) -> Option<DesignMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }
    let raw: RawDesignMessage = serde_json::from_str(trimmed).ok()?;

    match raw.kind.trim() {
        "status" => {
            let text = raw.text.trim().to_string();
            (!text.is_empty()).then_some(DesignMessage::Status(text))
        }
        "question" => {
            let text = raw.text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let suggested_answers = raw
                .suggested_answers
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Some(DesignMessage::Question {
                text,
                suggested_answers,
            })
        }
        "complete" | "done" => {
            let file = raw
                .file
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty());
            let summary = raw
                .summary
                .or_else(|| (!raw.text.trim().is_empty()).then_some(raw.text))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(DesignMessage::Complete {
                file,
                summary,
                areas: raw.areas,
            })
        }
        _ => None,
    }
}

struct ActiveDesignEntry {
    session_id: u64,
    cancel_tx: watch::Sender<bool>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    cumulative_usage: Arc<Mutex<CopilotUsage>>,
    turn_lock: Arc<Mutex<()>>,
}

/// Manages the single active, multi-turn Solution Designer Copilot session.
pub struct SolutionDesignerService {
    copilot: Arc<CopilotService>,
    next_session_id: AtomicU64,
    active: Mutex<Option<ActiveDesignEntry>>,
}

impl SolutionDesignerService {
    pub fn new(copilot: Arc<CopilotService>) -> Self {
        Self {
            copilot,
            next_session_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    pub fn start(
        self: &Arc<Self>,
        req: DesignStartRequest,
    ) -> mpsc::UnboundedReceiver<DesignStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let session_id = service.next_session_id.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let session_slot = Arc::new(Mutex::new(None));
            let cumulative_usage = Arc::new(Mutex::new(CopilotUsage::default()));
            let turn_lock = Arc::new(Mutex::new(()));

            let previous = {
                let mut guard = service.active.lock().await;
                guard.replace(ActiveDesignEntry {
                    session_id,
                    cancel_tx: cancel_tx.clone(),
                    session: Arc::clone(&session_slot),
                    cumulative_usage: Arc::clone(&cumulative_usage),
                    turn_lock: Arc::clone(&turn_lock),
                })
            };
            if let Some(prev) = previous {
                let _ = prev.cancel_tx.send(true);
                if let Some(s) = prev.session.lock().await.take() {
                    let _ = s.disconnect().await;
                }
            }

            if !req.project_dir.is_dir() {
                service.remove_if_matching(session_id).await;
                let _ = tx.send(DesignStreamEvent::Error(format!(
                    "Project directory does not exist: {}",
                    req.project_dir.display()
                )));
                return;
            }

            let effective_model =
                resolve_model_override(&req.model_override, &req.settings.copilot_model);

            let _ = tx.send(DesignStreamEvent::Status(
                "Creating Copilot solution design session...".to_string(),
            ));

            let session = match service
                .copilot
                .create_session(SessionOptions {
                    token: req.settings.copilot_token.clone(),
                    model: Some(effective_model),
                    working_directory: Some(req.project_dir),
                    tools: vec![create_report_intent_tool()],
                    available_tools: None,
                    streaming: true,
                    label: "Solution Designer".to_string(),
                })
                .await
            {
                Ok(s) => Arc::new(s),
                Err(err) => {
                    service.remove_if_matching(session_id).await;
                    let _ = tx.send(DesignStreamEvent::Error(err.to_string()));
                    return;
                }
            };

            if *cancel_rx.borrow() {
                service.remove_if_matching(session_id).await;
                let _ = session.disconnect().await;
                let _ = tx.send(DesignStreamEvent::Cancelled);
                return;
            }

            *session_slot.lock().await = Some(Arc::clone(&session));

            let custom_general = &req.settings.prompts.solution_designer.general;
            let prompt = match &req.kind {
                DesignSessionKind::GroundRules {
                    project_name,
                    initial_context,
                } => build_ground_rules_prompt(project_name, initial_context, custom_general),
                DesignSessionKind::AreaDesign {
                    project,
                    area,
                    extra_context,
                    is_redesign,
                } => build_area_design_prompt(
                    project,
                    area,
                    extra_context,
                    custom_general,
                    *is_redesign,
                ),
                DesignSessionKind::NewArea {
                    project,
                    title,
                    suggested_file,
                    extra_context,
                } => build_new_area_prompt(
                    project,
                    title,
                    suggested_file,
                    extra_context,
                    custom_general,
                ),
                DesignSessionKind::StandaloneRefine {
                    project,
                    target_title,
                    target_file,
                    is_readme,
                    instructions,
                } => build_standalone_refinement_prompt(
                    project,
                    target_title,
                    target_file,
                    *is_readme,
                    instructions,
                    custom_general,
                ),
            };

            let _turn_guard = turn_lock.lock().await;
            let terminal = run_turn(&session, &prompt, cancel_rx, &cumulative_usage, &tx).await;
            let _ = tx.send(terminal);
        });

        rx
    }

    pub fn send_answer(
        self: &Arc<Self>,
        answer: String,
    ) -> mpsc::UnboundedReceiver<DesignStreamEvent> {
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
                let _ = tx.send(DesignStreamEvent::Error(
                    "No active Solution Designer session found. Please start a new session."
                        .to_string(),
                ));
                return;
            };
            let session = { session_slot.lock().await.clone() };
            let Some(session) = session else {
                let _ = tx.send(DesignStreamEvent::Error(
                    "Solution Designer session is not ready yet.".to_string(),
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

async fn run_turn(
    session: &Session,
    prompt: &str,
    cancel_rx: watch::Receiver<bool>,
    cumulative_usage: &Arc<Mutex<CopilotUsage>>,
    tx: &mpsc::UnboundedSender<DesignStreamEvent>,
) -> DesignStreamEvent {
    if *cancel_rx.borrow() {
        return DesignStreamEvent::Cancelled;
    }

    let mut received_question = false;
    let mut received_complete = false;

    let stream_res = CopilotService::send_and_collect_stream(session, prompt, cancel_rx, |ev| {
        match ev {
            StreamEvent::Status(status) => {
                let _ = tx.send(DesignStreamEvent::Status(status));
            }
            StreamEvent::JsonLine(line) => {
                if let Some(msg) = parse_design_jsonl_line(&line) {
                    match msg {
                        DesignMessage::Status(s) => {
                            let _ = tx.send(DesignStreamEvent::Status(s));
                        }
                        DesignMessage::Question {
                            text,
                            suggested_answers,
                        } => {
                            received_question = true;
                            let _ = tx.send(DesignStreamEvent::Question {
                                text,
                                suggested_answers,
                            });
                        }
                        DesignMessage::Complete {
                            file,
                            summary,
                            areas,
                        } => {
                            received_complete = true;
                            let _ = tx.send(DesignStreamEvent::Complete {
                                file,
                                summary,
                                areas,
                            });
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
            DesignStreamEvent::TurnCompleted {
                received_question,
                received_complete,
                cumulative_usage: updated,
            }
        }
        Err(StreamError::Timeout(dur)) => DesignStreamEvent::TimedOut {
            timeout_secs: dur.as_secs().max(1),
        },
        Err(StreamError::Cancelled) => DesignStreamEvent::Cancelled,
        Err(StreamError::Other(err)) => DesignStreamEvent::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question_complete_with_areas_complete_with_file_and_noise() {
        let q = r#"{"type": "question", "text": " Use Kafka or SQS? ", "suggestedAnswers": ["(Recommended) SQS", " ", "Kafka"]}"#;
        assert_eq!(
            parse_design_jsonl_line(q),
            Some(DesignMessage::Question {
                text: "Use Kafka or SQS?".to_string(),
                suggested_answers: vec!["(Recommended) SQS".to_string(), "Kafka".to_string()],
            })
        );

        let gr = r#"{"type": "complete", "summary": "Scaffolded 2 areas", "areas": [{"id": "api", "title": "API", "file": "01-api.md", "summary": "Public API"}]}"#;
        assert_eq!(
            parse_design_jsonl_line(gr),
            Some(DesignMessage::Complete {
                file: None,
                summary: Some("Scaffolded 2 areas".to_string()),
                areas: vec![ReportedArea {
                    id: Some("api".to_string()),
                    title: "API".to_string(),
                    file: "01-api.md".to_string(),
                    summary: "Public API".to_string(),
                }],
            })
        );

        let area_done = r#"{"type": "done", "file": "01-api.md", "summary": "Defined REST + gRPC contracts"}"#;
        assert_eq!(
            parse_design_jsonl_line(area_done),
            Some(DesignMessage::Complete {
                file: Some("01-api.md".to_string()),
                summary: Some("Defined REST + gRPC contracts".to_string()),
                areas: Vec::new(),
            })
        );

        assert_eq!(
            parse_design_jsonl_line(r#"{"type": "status", "text": "Reading README.md"}"#),
            Some(DesignMessage::Status("Reading README.md".to_string()))
        );

        assert!(parse_design_jsonl_line("```json").is_none());
        assert!(parse_design_jsonl_line("not json").is_none());
        assert!(parse_design_jsonl_line(r#"{"type": "question", "text": "  "}"#).is_none());
        assert!(parse_design_jsonl_line(r#"{"type": "other", "text": "x"}"#).is_none());
    }
}
