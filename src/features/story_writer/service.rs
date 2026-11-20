use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::features::story_writer::prompts::build_story_prompt;
use crate::infrastructure::copilot::{
    CopilotService, SessionOptions, StreamError, StreamEvent, resolve_model_override,
};
use crate::infrastructure::runtime::runtime;
use crate::types::{AppSettings, CopilotUsage, DocPageData, StoryData};

#[derive(Debug, Clone)]
pub struct StoryGenerationRequest {
    pub page_data: DocPageData,
    pub additional_context: String,
    pub model_override: String,
    pub settings: AppSettings,
}

#[derive(Debug, Clone)]
pub enum StoryGenerationStreamEvent {
    Story(StoryData),
    Completed {
        story_count: usize,
        usage: CopilotUsage,
    },
    TimedOut {
        timeout_secs: u64,
    },
    Cancelled,
    Error(String),
}

pub struct ActiveStoryGenerationHandle {
    cancel_tx: watch::Sender<bool>,
}

impl ActiveStoryGenerationHandle {
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }
}

/// Parses a single JSONL line into a validated `StoryData` record.
pub fn parse_story_jsonl_line(line: &str) -> Option<StoryData> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }

    let mut story: StoryData = serde_json::from_str(trimmed).ok()?;
    story.title = story.title.trim().to_string();
    if story.title.is_empty() {
        return None;
    }
    story.notes = story
        .notes
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());

    Some(story)
}

/// Streams generated `StoryData` objects from Copilot onto a Tokio task and forwards them to the UI.
pub fn generate_stories(
    copilot: Arc<CopilotService>,
    req: StoryGenerationRequest,
) -> (
    ActiveStoryGenerationHandle,
    mpsc::UnboundedReceiver<StoryGenerationStreamEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);

    runtime().spawn(async move {
        let terminal = run_story_generation_task(copilot, req, cancel_rx, &tx).await;
        let _ = tx.send(terminal);
    });

    (ActiveStoryGenerationHandle { cancel_tx }, rx)
}

async fn run_story_generation_task(
    copilot: Arc<CopilotService>,
    req: StoryGenerationRequest,
    cancel_rx: watch::Receiver<bool>,
    tx: &mpsc::UnboundedSender<StoryGenerationStreamEvent>,
) -> StoryGenerationStreamEvent {
    if *cancel_rx.borrow() {
        return StoryGenerationStreamEvent::Cancelled;
    }

    let effective_model = resolve_model_override(&req.model_override, &req.settings.copilot_model);

    let session = match copilot
        .create_session(SessionOptions {
            token: req.settings.copilot_token.clone(),
            model: Some(effective_model),
            working_directory: None,
            tools: Vec::new(),
            available_tools: Some(Vec::new()),
            streaming: true,
            label: "Story Writer".to_string(),
        })
        .await
    {
        Ok(session) => session,
        Err(err) => return StoryGenerationStreamEvent::Error(err.to_string()),
    };

    let prompt = build_story_prompt(
        &req.page_data.title,
        &req.page_data.body,
        &req.additional_context,
        &req.settings.prompts.story_writer,
    );

    let mut story_count = 0usize;
    let stream_res = CopilotService::send_and_collect_stream(&session, &prompt, cancel_rx, |ev| {
        if let StreamEvent::JsonLine(line) = ev {
            if let Some(story) = parse_story_jsonl_line(&line) {
                story_count += 1;
                let _ = tx.send(StoryGenerationStreamEvent::Story(story));
            }
        }
    })
    .await;

    let _ = session.disconnect().await;

    match stream_res {
        Ok(usage) => StoryGenerationStreamEvent::Completed { story_count, usage },
        Err(StreamError::Timeout(dur)) => StoryGenerationStreamEvent::TimedOut {
            timeout_secs: dur.as_secs().max(1),
        },
        Err(StreamError::Cancelled) => StoryGenerationStreamEvent::Cancelled,
        Err(StreamError::Other(err)) => StoryGenerationStreamEvent::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_story_jsonl_line_parses_valid_stories_and_skips_noise() {
        let valid = r#"{"title": "  Add SSO  ", "description": "As a user...", "acceptanceCriteria": "* Login works", "notes": "   "}"#;
        let parsed = parse_story_jsonl_line(valid).expect("valid story");
        assert_eq!(parsed.title, "Add SSO");
        assert_eq!(parsed.description, "As a user...");
        assert_eq!(parsed.acceptance_criteria, "* Login works");
        assert_eq!(parsed.notes, None);

        let with_notes = r#"{"title": "Audit Logs", "description": "Desc", "acceptanceCriteria": "* AC", "notes": "  Keep 90 days "}"#;
        let parsed_notes = parse_story_jsonl_line(with_notes).expect("story with notes");
        assert_eq!(parsed_notes.notes.as_deref(), Some("Keep 90 days"));

        assert!(parse_story_jsonl_line("```json").is_none());
        assert!(parse_story_jsonl_line("   ").is_none());
        assert!(parse_story_jsonl_line(r#"{"title": "  ", "description": "x", "acceptanceCriteria": "y"}"#).is_none());
        assert!(parse_story_jsonl_line("not json").is_none());
    }

    #[test]
    fn resolve_model_override_preserves_explicit_auto_and_falls_back_cleanly() {
        assert_eq!(resolve_model_override("auto", "gpt-5.4"), "auto");
        assert_eq!(resolve_model_override("", "gpt-5.4"), "gpt-5.4");
        assert_eq!(resolve_model_override("", ""), "auto");
    }
}
