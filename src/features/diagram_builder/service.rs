use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use github_copilot_sdk::session::Session;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, watch};

use crate::features::diagram_builder::prompts::{
    build_change_request_prompt, build_initial_diagram_prompt, build_syntax_fix_prompt,
};
use crate::features::diagram_builder::renderer::{
    DiagramTheme, RenderedDiagram, normalize_mermaid_source, validate_and_render,
};
use crate::features::story_elaborator::service::merge_usage;
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, create_report_intent_tool, parse_resilient_jsonl,
    resolve_model_override,
};
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage};

pub const MAX_AUTO_FIX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone)]
pub struct DiagramStartRequest {
    pub description: String,
    pub project_dir: Option<PathBuf>,
    pub model_override: String,
    pub settings: AppSettings,
    pub diagram_theme: DiagramTheme,
}

#[derive(Debug, Clone)]
pub enum DiagramStreamEvent {
    Status(String),
    DiagramRendered(RenderedDiagram),
    ValidationExhausted {
        broken_mermaid: String,
        error: String,
        attempts: usize,
    },
    TurnCompleted {
        cumulative_usage: CopilotUsage,
        diagram_updated: bool,
    },
    TimedOut {
        timeout_secs: u64,
    },
    Cancelled,
    Error(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDiagramMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    mermaid: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

const MERMAID_KEYWORDS: &[&str] = &[
    "flowchart",
    "graph",
    "sequencediagram",
    "classdiagram",
    "statediagram",
    "erdiagram",
    "gantt",
    "pie",
    "mindmap",
    "gitgraph",
    "journey",
    "quadrantchart",
    "requirementdiagram",
    "c4context",
    "c4container",
    "c4component",
    "c4dynamic",
    "c4deployment",
    "sankey-beta",
    "xychart-beta",
    "block-beta",
    "timeline",
    "architecture-beta",
    "packet-beta",
    "kanban",
    "radar-beta",
    "treemap-beta",
    "%%",
];

fn starts_with_mermaid_keyword(text: &str) -> bool {
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();
    MERMAID_KEYWORDS
        .iter()
        .any(|kw| first_line.starts_with(kw))
}

fn extract_last_fenced_block(raw: &str) -> Option<String> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut blocks = Vec::new();
    let mut in_fence = false;
    let mut is_mermaid_fence = false;
    let mut current = Vec::new();

    for line in normalized.lines() {
        let trimmed = line.trim();
        if !in_fence {
            if let Some(rest) = trimmed.strip_prefix("```") {
                in_fence = true;
                let lang = rest.trim().to_ascii_lowercase();
                is_mermaid_fence = lang.is_empty() || lang == "mermaid";
                current.clear();
            }
        } else if trimmed.starts_with("```") {
            if is_mermaid_fence {
                let body = current.join("\n").trim().to_string();
                if !body.is_empty() {
                    blocks.push(body);
                }
            }
            in_fence = false;
            is_mermaid_fence = false;
            current.clear();
        } else {
            current.push(line);
        }
    }

    blocks.pop()
}

/// Extracts Mermaid diagram source from a Copilot assistant response.
///
/// Prefers JSONL `{"type": "diagram", "mermaid": "..."}` objects, then falls back to
/// fenced ```` ```mermaid ```` blocks, and finally to raw Mermaid text starting with a known header.
pub fn extract_mermaid_from_response(raw: &str) -> Option<String> {
    let mut last_json_diagram: Option<String> = None;
    let _ = parse_resilient_jsonl(raw, |line| {
        if let Ok(msg) = serde_json::from_str::<RawDiagramMessage>(line.trim()) {
            if msg.kind.trim().eq_ignore_ascii_case("diagram") {
                let candidate = msg
                    .mermaid
                    .or(msg.text)
                    .or(msg.code)
                    .map(|s| normalize_mermaid_source(&s))
                    .filter(|s| !s.is_empty());
                if let Some(c) = candidate {
                    last_json_diagram = Some(c);
                }
            }
        }
    });
    if last_json_diagram.is_some() {
        return last_json_diagram;
    }

    if let Some(fenced) = extract_last_fenced_block(raw) {
        let normalized = normalize_mermaid_source(&fenced);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }

    let normalized = normalize_mermaid_source(raw);
    if !normalized.is_empty() && starts_with_mermaid_keyword(&normalized) {
        return Some(normalized);
    }

    None
}

struct ActiveDiagramEntry {
    session_id: u64,
    cancel_tx: watch::Sender<bool>,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    cumulative_usage: Arc<Mutex<CopilotUsage>>,
    turn_lock: Arc<Mutex<()>>,
}

/// Manages the active Copilot session and validation/auto-fix loop for Diagram Builder.
pub struct DiagramBuilderService {
    copilot: Arc<CopilotService>,
    next_session_id: AtomicU64,
    active: Mutex<Option<ActiveDiagramEntry>>,
}

impl DiagramBuilderService {
    pub fn new(copilot: Arc<CopilotService>) -> Self {
        Self {
            copilot,
            next_session_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    pub fn start(
        self: &Arc<Self>,
        req: DiagramStartRequest,
    ) -> mpsc::UnboundedReceiver<DiagramStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            if let Some(ref dir) = req.project_dir {
                if !dir.is_dir() {
                    let _ = tx.send(DiagramStreamEvent::Error(format!(
                        "Project directory does not exist: {}",
                        dir.display()
                    )));
                    return;
                }
            }

            let session_id = service.next_session_id.fetch_add(1, Ordering::Relaxed);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let session_slot = Arc::new(Mutex::new(None));
            let cumulative_usage = Arc::new(Mutex::new(CopilotUsage::default()));
            let turn_lock = Arc::new(Mutex::new(()));

            let previous = {
                let mut guard = service.active.lock().await;
                guard.replace(ActiveDiagramEntry {
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

            let effective_model =
                resolve_model_override(&req.model_override, &req.settings.copilot_model);

            let _ = tx.send(DiagramStreamEvent::Status(
                "Creating Copilot diagram session...".to_string(),
            ));

            let session = match service
                .copilot
                .create_session(SessionOptions {
                    token: req.settings.copilot_token.clone(),
                    model: Some(effective_model),
                    working_directory: req.project_dir.clone(),
                    tools: vec![create_report_intent_tool()],
                    available_tools: None,
                    streaming: true,
                    label: "Diagram Builder".to_string(),
                })
                .await
            {
                Ok(s) => Arc::new(s),
                Err(err) => {
                    service.remove_if_matching(session_id).await;
                    let _ = tx.send(DiagramStreamEvent::Error(err.to_string()));
                    return;
                }
            };

            if *cancel_rx.borrow() {
                service.remove_if_matching(session_id).await;
                let _ = session.disconnect().await;
                let _ = tx.send(DiagramStreamEvent::Cancelled);
                return;
            }

            *session_slot.lock().await = Some(Arc::clone(&session));

            let prompt =
                build_initial_diagram_prompt(&req.description, req.project_dir.is_some());

            let _turn_guard = turn_lock.lock().await;
            run_turn_with_validation(
                &session,
                &prompt,
                req.diagram_theme,
                cancel_rx,
                &cumulative_usage,
                &tx,
                false,
                None,
            )
            .await;
        });

        rx
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_change(
        self: &Arc<Self>,
        change_request: String,
        current_mermaid: Option<String>,
        diagram_theme: DiagramTheme,
        project_dir: Option<PathBuf>,
        model_override: String,
        settings: AppSettings,
    ) -> mpsc::UnboundedReceiver<DiagramStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let (session, cumulative_usage, turn_lock, cancel_rx) = match service
                .ensure_active_session(project_dir, model_override, settings, &tx)
                .await
            {
                Ok(tuple) => tuple,
                Err(err_ev) => {
                    let _ = tx.send(err_ev);
                    return;
                }
            };

            let prompt =
                build_change_request_prompt(&change_request, current_mermaid.as_deref());

            let _turn_guard = turn_lock.lock().await;
            run_turn_with_validation(
                &session,
                &prompt,
                diagram_theme,
                cancel_rx,
                &cumulative_usage,
                &tx,
                false,
                None,
            )
            .await;
        });

        rx
    }

    #[allow(clippy::too_many_arguments)]
    pub fn retry_fix(
        self: &Arc<Self>,
        broken_mermaid: String,
        error: String,
        diagram_theme: DiagramTheme,
        project_dir: Option<PathBuf>,
        model_override: String,
        settings: AppSettings,
    ) -> mpsc::UnboundedReceiver<DiagramStreamEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);

        runtime().spawn(async move {
            let (session, cumulative_usage, turn_lock, cancel_rx) = match service
                .ensure_active_session(project_dir, model_override, settings, &tx)
                .await
            {
                Ok(tuple) => tuple,
                Err(err_ev) => {
                    let _ = tx.send(err_ev);
                    return;
                }
            };

            let prompt =
                build_syntax_fix_prompt(&broken_mermaid, &error, 1, MAX_AUTO_FIX_ATTEMPTS);

            let _turn_guard = turn_lock.lock().await;
            run_turn_with_validation(
                &session,
                &prompt,
                diagram_theme,
                cancel_rx,
                &cumulative_usage,
                &tx,
                true,
                Some((broken_mermaid, error)),
            )
            .await;
        });

        rx
    }

    async fn ensure_active_session(
        &self,
        project_dir: Option<PathBuf>,
        model_override: String,
        settings: AppSettings,
        tx: &mpsc::UnboundedSender<DiagramStreamEvent>,
    ) -> Result<
        (
            Arc<Session>,
            Arc<Mutex<CopilotUsage>>,
            Arc<Mutex<()>>,
            watch::Receiver<bool>,
        ),
        DiagramStreamEvent,
    > {
        let lookup = {
            let guard = self.active.lock().await;
            guard.as_ref().map(|e| {
                (
                    Arc::clone(&e.session),
                    Arc::clone(&e.cumulative_usage),
                    Arc::clone(&e.turn_lock),
                    e.cancel_tx.subscribe(),
                )
            })
        };

        if let Some((session_slot, cumulative_usage, turn_lock, cancel_rx)) = lookup {
            if let Some(session) = session_slot.lock().await.clone() {
                if !*cancel_rx.borrow() {
                    return Ok((session, cumulative_usage, turn_lock, cancel_rx));
                }
            }
        }

        // No active session (e.g. user cancelled a previous turn) — recreate seamlessly.
        if let Some(ref dir) = project_dir {
            if !dir.is_dir() {
                return Err(DiagramStreamEvent::Error(format!(
                    "Project directory does not exist: {}",
                    dir.display()
                )));
            }
        }

        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let session_slot = Arc::new(Mutex::new(None));
        let cumulative_usage = Arc::new(Mutex::new(CopilotUsage::default()));
        let turn_lock = Arc::new(Mutex::new(()));

        {
            let mut guard = self.active.lock().await;
            *guard = Some(ActiveDiagramEntry {
                session_id,
                cancel_tx,
                session: Arc::clone(&session_slot),
                cumulative_usage: Arc::clone(&cumulative_usage),
                turn_lock: Arc::clone(&turn_lock),
            });
        }

        let effective_model = resolve_model_override(&model_override, &settings.copilot_model);
        let _ = tx.send(DiagramStreamEvent::Status(
            "Reconnecting Copilot diagram session...".to_string(),
        ));

        let session = match self
            .copilot
            .create_session(SessionOptions {
                token: settings.copilot_token,
                model: Some(effective_model),
                working_directory: project_dir,
                tools: vec![create_report_intent_tool()],
                available_tools: None,
                streaming: true,
                label: "Diagram Builder".to_string(),
            })
            .await
        {
            Ok(s) => Arc::new(s),
            Err(err) => {
                self.remove_if_matching(session_id).await;
                return Err(DiagramStreamEvent::Error(err.to_string()));
            }
        };

        if *cancel_rx.borrow() {
            self.remove_if_matching(session_id).await;
            let _ = session.disconnect().await;
            return Err(DiagramStreamEvent::Cancelled);
        }

        *session_slot.lock().await = Some(Arc::clone(&session));
        Ok((session, cumulative_usage, turn_lock, cancel_rx))
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

async fn validate_candidate_async(
    candidate: String,
    theme: DiagramTheme,
) -> Result<RenderedDiagram, String> {
    tokio::task::spawn_blocking(move || validate_and_render(&candidate, theme))
        .await
        .map_err(|e| format!("Validation task failed: {e}"))?
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_with_validation(
    session: &Session,
    initial_prompt: &str,
    theme: DiagramTheme,
    cancel_rx: watch::Receiver<bool>,
    cumulative_usage: &Arc<Mutex<CopilotUsage>>,
    tx: &mpsc::UnboundedSender<DiagramStreamEvent>,
    initial_is_fix_attempt: bool,
    initial_broken: Option<(String, String)>,
) {
    if *cancel_rx.borrow() {
        let _ = tx.send(DiagramStreamEvent::Cancelled);
        return;
    }

    let mut current_prompt = initial_prompt.to_string();
    let mut last_broken = initial_broken
        .as_ref()
        .map(|(src, _)| src.clone())
        .unwrap_or_default();
    let mut last_error = initial_broken
        .as_ref()
        .map(|(_, err)| err.clone())
        .unwrap_or_default();

    let total_turns = if initial_is_fix_attempt {
        MAX_AUTO_FIX_ATTEMPTS
    } else {
        1 + MAX_AUTO_FIX_ATTEMPTS
    };

    for turn_idx in 0..total_turns {
        if *cancel_rx.borrow() {
            let _ = tx.send(DiagramStreamEvent::Cancelled);
            return;
        }

        if initial_is_fix_attempt {
            let attempt = turn_idx + 1;
            let _ = tx.send(DiagramStreamEvent::Status(format!(
                "Retrying Mermaid syntax fix (attempt {attempt}/{MAX_AUTO_FIX_ATTEMPTS})..."
            )));
        } else if turn_idx > 0 {
            let attempt = turn_idx;
            let _ = tx.send(DiagramStreamEvent::Status(format!(
                "Mermaid syntax error — auto-fixing (attempt {attempt}/{MAX_AUTO_FIX_ATTEMPTS})..."
            )));
        }

        let tx_status = tx.clone();
        let turn_res = CopilotService::send_and_collect_text_with_status(
            session,
            &current_prompt,
            cancel_rx.clone(),
            move |status| {
                let _ = tx_status.send(DiagramStreamEvent::Status(status));
            },
        )
        .await;

        let raw_response = match turn_res {
            Ok((text, turn_usage)) => {
                let mut total = cumulative_usage.lock().await;
                merge_usage(&mut total, &turn_usage);
                text
            }
            Err(StreamError::Timeout(dur)) => {
                let _ = tx.send(DiagramStreamEvent::TimedOut {
                    timeout_secs: dur.as_secs().max(1),
                });
                return;
            }
            Err(StreamError::Cancelled) => {
                let _ = tx.send(DiagramStreamEvent::Cancelled);
                return;
            }
            Err(StreamError::Other(err)) => {
                let _ = tx.send(DiagramStreamEvent::Error(err.to_string()));
                return;
            }
        };

        let Some(candidate) = extract_mermaid_from_response(&raw_response) else {
            last_error = "Copilot did not return a Mermaid diagram payload.".to_string();
            if turn_idx + 1 < total_turns {
                let next_attempt = if initial_is_fix_attempt {
                    turn_idx + 2
                } else {
                    turn_idx + 1
                };
                current_prompt = build_syntax_fix_prompt(
                    &last_broken,
                    &last_error,
                    next_attempt,
                    MAX_AUTO_FIX_ATTEMPTS,
                );
                continue;
            }
            break;
        };

        last_broken = candidate.clone();
        let _ = tx.send(DiagramStreamEvent::Status(
            "Validating and rendering Mermaid diagram...".to_string(),
        ));

        match validate_candidate_async(candidate, theme).await {
            Ok(rendered) => {
                let usage_snapshot = cumulative_usage.lock().await.clone();
                let _ = tx.send(DiagramStreamEvent::DiagramRendered(rendered));
                let _ = tx.send(DiagramStreamEvent::TurnCompleted {
                    cumulative_usage: usage_snapshot,
                    diagram_updated: true,
                });
                return;
            }
            Err(err) => {
                last_error = err;
                if turn_idx + 1 < total_turns {
                    let next_attempt = if initial_is_fix_attempt {
                        turn_idx + 2
                    } else {
                        turn_idx + 1
                    };
                    current_prompt = build_syntax_fix_prompt(
                        &last_broken,
                        &last_error,
                        next_attempt,
                        MAX_AUTO_FIX_ATTEMPTS,
                    );
                }
            }
        }
    }

    let _ = tx.send(DiagramStreamEvent::Status(format!(
        "Auto-fix failed after {MAX_AUTO_FIX_ATTEMPTS} attempts."
    )));
    let _ = tx.send(DiagramStreamEvent::ValidationExhausted {
        broken_mermaid: last_broken,
        error: last_error,
        attempts: MAX_AUTO_FIX_ATTEMPTS,
    });
    let usage_snapshot = cumulative_usage.lock().await.clone();
    let _ = tx.send(DiagramStreamEvent::TurnCompleted {
        cumulative_usage: usage_snapshot,
        diagram_updated: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mermaid_from_jsonl_response() {
        let raw = r#"{"type": "diagram", "mermaid": "flowchart TD\n    A[\"Start\"] --> B[\"End\"]"}"#;
        let extracted = extract_mermaid_from_response(raw).expect("jsonl diagram");
        assert_eq!(extracted, "flowchart TD\n    A[\"Start\"] --> B[\"End\"]");
    }

    #[test]
    fn extracts_mermaid_from_fenced_fallback() {
        let raw = "Here is the diagram:\n```mermaid\nsequenceDiagram\n    Alice->>Bob: Hi\n```\n";
        let extracted = extract_mermaid_from_response(raw).expect("fenced diagram");
        assert_eq!(extracted, "sequenceDiagram\n    Alice->>Bob: Hi");
    }

    #[test]
    fn extracts_mermaid_from_raw_header_fallback() {
        let raw = "flowchart LR\n    A --> B";
        let extracted = extract_mermaid_from_response(raw).expect("raw diagram");
        assert_eq!(extracted, "flowchart LR\n    A --> B");
    }

    #[test]
    fn returns_none_for_non_diagram_prose() {
        assert_eq!(
            extract_mermaid_from_response("I cannot generate a diagram right now."),
            None
        );
    }
}
