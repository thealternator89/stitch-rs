pub mod tools;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use github_copilot_sdk::session::Session;
use github_copilot_sdk::session_events::{
    AssistantMessageData, AssistantMessageDeltaData, AssistantUsageData, SessionErrorData,
    ToolExecutionCompleteData, ToolExecutionStartData,
};
use github_copilot_sdk::subscription::RecvErrorKind;
use github_copilot_sdk::types::{SessionConfig, Tool, WorkingDirectoryTrust};
use github_copilot_sdk::{Client, ClientInfo, ClientOptions};
use tokio::sync::{Mutex, watch};

use crate::types::{CopilotAuthInfo, CopilotModelInfo, CopilotUsage};

pub use tools::{
    create_report_intent_tool, format_tool_complete, format_tool_start, parse_resilient_jsonl,
};

const DEFAULT_TIMEOUT_MS: u64 = 180_000;

struct ManagedClient {
    token: Option<String>,
    client: Client,
}

/// Configuration for creating a Copilot agent session.
pub struct SessionOptions {
    pub token: Option<String>,
    pub model: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub tools: Vec<Tool>,
    pub available_tools: Option<Vec<String>>,
    pub streaming: bool,
    #[allow(dead_code)]
    pub label: String,
}

/// Live stream item produced while collecting a Copilot turn.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Status(String),
    JsonLine(String),
}

/// Failure reason returned from `CopilotService::send_and_collect_stream`.
#[derive(Debug)]
pub enum StreamError {
    Timeout(Duration),
    Cancelled,
    Other(anyhow::Error),
}

/// Manages the Copilot SDK client lifecycle, auth checks, cached model discovery, and streaming sessions.
pub struct CopilotService {
    client_slot: Mutex<Option<ManagedClient>>,
    cached_models: Mutex<Option<(Option<String>, Vec<CopilotModelInfo>)>>,
}

impl CopilotService {
    pub fn new() -> Self {
        Self {
            client_slot: Mutex::new(None),
            cached_models: Mutex::new(None),
        }
    }

    fn normalize_token(token: Option<String>) -> Option<String> {
        token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    fn build_client_options(token: Option<&str>) -> ClientOptions {
        let mut options = ClientOptions::new().with_client_info(
            ClientInfo::new()
                .with_application_name("Stitch")
                .with_application_version(env!("CARGO_PKG_VERSION")),
        );

        if let Some(tok) = token {
            options = options.with_github_token(tok).with_use_logged_in_user(false);
        }

        options
    }

    async fn ensure_client(&self, token: Option<String>) -> anyhow::Result<Client> {
        let old_client = {
            let mut guard = self.client_slot.lock().await;
            let token_changed = guard
                .as_ref()
                .is_some_and(|existing| existing.token != token);
            if token_changed {
                guard.take()
            } else {
                None
            }
        };

        if let Some(old) = old_client {
            let _ = old.client.stop().await;
        }

        let mut guard = self.client_slot.lock().await;
        if let Some(existing) = guard.as_ref() {
            if existing.token == token {
                return Ok(existing.client.clone());
            }
        }

        let options = Self::build_client_options(token.as_deref());
        let client = Client::start(options).await.map_err(|err| {
            anyhow::anyhow!(
                "Failed to start GitHub Copilot runtime: {err}. Ensure the Copilot CLI is available or configured."
            )
        })?;

        *guard = Some(ManagedClient {
            token,
            client: client.clone(),
        });

        Ok(client)
    }

    async fn reset_client(&self) {
        let old = {
            let mut guard = self.client_slot.lock().await;
            guard.take()
        };
        if let Some(stale) = old {
            let _ = stale.client.stop().await;
        }
    }

    /// Verifies authentication state and CLI runtime metadata via `get_auth_status()` and `get_status()`.
    pub async fn check_auth_status(&self, token: Option<String>) -> anyhow::Result<CopilotAuthInfo> {
        let normalized_token = Self::normalize_token(token);
        let client = self.ensure_client(normalized_token).await?;

        let auth = match client.get_auth_status().await {
            Ok(auth) => auth,
            Err(err) => {
                self.reset_client().await;
                return Err(anyhow::anyhow!("Copilot auth check failed: {err}"));
            }
        };

        let status = client.get_status().await.ok();

        Ok(CopilotAuthInfo {
            is_authenticated: auth.is_authenticated,
            login: auth.login,
            auth_type: auth.auth_type,
            host: auth.host,
            status_message: auth.status_message,
            cli_version: status.as_ref().map(|s| s.version.clone()),
            protocol_version: status.as_ref().map(|s| s.protocol_version.to_string()),
        })
    }

    /// Lists available Copilot models, caching results per token unless `force_refresh` is true.
    pub async fn list_models(
        &self,
        token: Option<String>,
        force_refresh: bool,
    ) -> anyhow::Result<Vec<CopilotModelInfo>> {
        let normalized_token = Self::normalize_token(token);

        if !force_refresh {
            let cache = self.cached_models.lock().await;
            if let Some((cached_token, models)) = cache.as_ref() {
                if cached_token == &normalized_token {
                    return Ok(models.clone());
                }
            }
        }

        let client = self.ensure_client(normalized_token.clone()).await?;

        let raw_models = if force_refresh {
            client.rpc().models().list().await.map(|res| res.models)
        } else {
            client.list_models().await
        };

        let raw_models = match raw_models {
            Ok(models) => models,
            Err(err) => {
                self.reset_client().await;
                return Err(anyhow::anyhow!("Failed to list Copilot models: {err}"));
            }
        };

        let models: Vec<CopilotModelInfo> = raw_models
            .into_iter()
            .map(|m| CopilotModelInfo {
                id: m.id,
                name: m.name,
                billing_multiplier: m.billing.and_then(|b| b.multiplier),
            })
            .collect();

        let mut cache = self.cached_models.lock().await;
        *cache = Some((normalized_token, models.clone()));

        Ok(models)
    }

    /// Creates a configured Copilot session with streaming enabled.
    pub async fn create_session(&self, opts: SessionOptions) -> anyhow::Result<Session> {
        let normalized_token = Self::normalize_token(opts.token);
        let client = self.ensure_client(normalized_token).await?;

        let mut config = SessionConfig::default()
            .with_streaming(opts.streaming)
            .with_model(resolve_session_model(opts.model.as_deref()))
            .approve_all_permissions();

        if let Some(dir) = opts.working_directory {
            config = config
                .with_working_directory(dir)
                .with_trust_working_directory(WorkingDirectoryTrust::Session);
        }

        if !opts.tools.is_empty() {
            config = config.with_tools(opts.tools);
        }

        if let Some(allowed) = opts.available_tools {
            config = config.with_available_tools(allowed);
        }

        match client.create_session(config).await {
            Ok(session) => Ok(session),
            Err(err) => {
                self.reset_client().await;
                Err(anyhow::anyhow!("Failed to create Copilot session: {err}"))
            }
        }
    }

    /// Sends a prompt to an active Copilot session and streams status updates and JSONL lines
    /// until `session.idle`, error, cancellation, or inactivity timeout (paused while tools execute).
    pub async fn send_and_collect_stream<F>(
        session: &Session,
        prompt: &str,
        cancel_rx: watch::Receiver<bool>,
        mut on_event: F,
    ) -> Result<CopilotUsage, StreamError>
    where
        F: FnMut(StreamEvent) + Send,
    {
        let mut buffer = String::new();
        let mut received_deltas = false;

        let (last_assistant_message, usage) =
            collect_session_turn(session, prompt, cancel_rx, |chunk| match chunk {
                TurnChunk::Delta(delta) => {
                    received_deltas = true;
                    buffer.push_str(&delta);
                    buffer = parse_resilient_jsonl(&buffer, |line| {
                        on_event(StreamEvent::JsonLine(line.to_string()));
                    });
                }
                TurnChunk::Status(status) => {
                    on_event(StreamEvent::Status(status));
                }
            })
            .await?;

        if !buffer.trim().is_empty() {
            let _ = parse_resilient_jsonl(&buffer, |line| {
                on_event(StreamEvent::JsonLine(line.to_string()));
            });
        } else if !received_deltas {
            if let Some(msg) = &last_assistant_message {
                let _ = parse_resilient_jsonl(msg, |line| {
                    on_event(StreamEvent::JsonLine(line.to_string()));
                });
            }
        }

        Ok(usage)
    }

    /// Sends a prompt to an active Copilot session and collects the plain-text response
    /// (streaming deltas if enabled, or the final `assistant.message` when `streaming == false`).
    pub async fn send_and_collect_text(
        session: &Session,
        prompt: &str,
        cancel_rx: watch::Receiver<bool>,
    ) -> Result<(String, CopilotUsage), StreamError> {
        let mut accumulated = String::new();

        let (last_assistant_message, usage) =
            collect_session_turn(session, prompt, cancel_rx, |chunk| {
                if let TurnChunk::Delta(delta) = chunk {
                    accumulated.push_str(&delta);
                }
            })
            .await?;

        let text = if !accumulated.is_empty() {
            accumulated
        } else {
            last_assistant_message.unwrap_or_default()
        };

        Ok((text, usage))
    }

    /// Clears cached model listings and stops any active client so future calls reload fresh state.
    #[allow(dead_code)]
    pub async fn invalidate_cache(&self) {
        {
            let mut cache = self.cached_models.lock().await;
            *cache = None;
        }
        self.reset_client().await;
    }

    /// Gracefully stops the underlying Copilot SDK client if running.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        self.invalidate_cache().await;
    }
}

enum TurnChunk {
    Delta(String),
    Status(String),
}

async fn collect_session_turn<F>(
    session: &Session,
    prompt: &str,
    mut cancel_rx: watch::Receiver<bool>,
    mut on_chunk: F,
) -> Result<(Option<String>, CopilotUsage), StreamError>
where
    F: FnMut(TurnChunk) + Send,
{
    if *cancel_rx.borrow() {
        return Err(StreamError::Cancelled);
    }

    log_prompt_if_configured(prompt);
    let timeout_dur = resolve_timeout_duration();

    // Subscribe before sending so no early deltas or tool events are lost.
    let mut sub = session.subscribe();

    if let Err(err) = session.send(prompt).await {
        return Err(StreamError::Other(anyhow::anyhow!(
            "Failed to send prompt to Copilot: {err}"
        )));
    }

    let mut last_assistant_message: Option<String> = None;
    let mut active_tool_calls: HashMap<String, String> = HashMap::new();
    let mut usage = CopilotUsage::default();

    let watchdog = tokio::time::sleep(timeout_dur);
    tokio::pin!(watchdog);

    loop {
        tokio::select! {
            res = cancel_rx.changed() => {
                if res.is_err() || *cancel_rx.borrow() {
                    let _ = session.abort().await;
                    return Err(StreamError::Cancelled);
                }
            }
            () = &mut watchdog, if active_tool_calls.is_empty() => {
                let _ = session.abort().await;
                return Err(StreamError::Timeout(timeout_dur));
            }
            ev_res = sub.recv() => {
                match ev_res {
                    Ok(event) => {
                        match event.event_type.as_str() {
                            "assistant.message_delta" => {
                                if let Ok(data) = serde_json::from_value::<AssistantMessageDeltaData>(event.data) {
                                    if !data.delta_content.is_empty() {
                                        on_chunk(TurnChunk::Delta(data.delta_content));
                                    }
                                }
                                watchdog.as_mut().reset(tokio::time::Instant::now() + timeout_dur);
                            }
                            "assistant.message" => {
                                if let Ok(data) = serde_json::from_value::<AssistantMessageData>(event.data) {
                                    last_assistant_message = Some(data.content);
                                }
                            }
                            "assistant.usage" => {
                                if let Ok(data) = serde_json::from_value::<AssistantUsageData>(event.data) {
                                    accumulate_usage(&mut usage, data);
                                }
                            }
                            "tool.execution_start" => {
                                if let Ok(data) = serde_json::from_value::<ToolExecutionStartData>(event.data) {
                                    let status = format_tool_start(&data.tool_name, data.arguments.as_ref());
                                    active_tool_calls.insert(data.tool_call_id, data.tool_name);
                                    on_chunk(TurnChunk::Status(status));
                                }
                            }
                            "tool.execution_complete" => {
                                if let Ok(data) = serde_json::from_value::<ToolExecutionCompleteData>(event.data) {
                                    let tool_name = active_tool_calls
                                        .remove(&data.tool_call_id)
                                        .unwrap_or_else(|| "tool".to_string());
                                    if let Some(err_status) = format_tool_complete(
                                        &tool_name,
                                        data.success,
                                        data.error.as_ref().map(|e| e.message.as_str()),
                                    ) {
                                        on_chunk(TurnChunk::Status(err_status));
                                    }
                                    if active_tool_calls.is_empty() {
                                        watchdog.as_mut().reset(tokio::time::Instant::now() + timeout_dur);
                                    }
                                }
                            }
                            "session.idle" => {
                                return Ok((last_assistant_message, usage));
                            }
                            "session.error" => {
                                if let Ok(data) = serde_json::from_value::<SessionErrorData>(event.data) {
                                    return Err(StreamError::Other(anyhow::anyhow!(
                                        "Copilot session error ({}): {}",
                                        data.error_type,
                                        data.message
                                    )));
                                }
                                return Err(StreamError::Other(anyhow::anyhow!(
                                    "Copilot session error"
                                )));
                            }
                            _ => {}
                        }
                    }
                    Err(err) => match err.kind() {
                        RecvErrorKind::Lagged(_) => continue,
                        RecvErrorKind::Closed => {
                            return Err(StreamError::Other(anyhow::anyhow!(
                                "Copilot session event stream closed unexpectedly"
                            )));
                        }
                        _ => {
                            return Err(StreamError::Other(anyhow::anyhow!(
                                "Copilot session stream error: {err}"
                            )));
                        }
                    },
                }
            }
        }
    }
}

fn accumulate_usage(usage: &mut CopilotUsage, data: AssistantUsageData) {
    usage.input_tokens = usage.input_tokens.saturating_add(
        data.input_tokens
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0),
    );
    usage.output_tokens = usage.output_tokens.saturating_add(
        data.output_tokens
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0),
    );
    usage.cache_read_tokens = usage.cache_read_tokens.saturating_add(
        data.cache_read_tokens
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0),
    );
    if let Some(cost) = data.cost {
        usage.cost += cost;
    }
    if !data.model.is_empty() {
        usage.model = Some(data.model);
    }
}

/// Resolves the effective Copilot model ID, preserving an explicit `"auto"` override
/// before falling back to `default_model` and finally `"auto"`.
pub fn resolve_model_override(override_model: &str, default_model: &str) -> String {
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

fn resolve_session_model(model: Option<&str>) -> &str {
    model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("auto")
}

fn resolve_timeout_duration() -> Duration {
    let ms = std::env::var("STITCH_COPILOT_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms)
}

fn log_prompt_if_configured(prompt: &str) {
    let Ok(log_path) = std::env::var("STITCH_PROMPT_LOG") else {
        return;
    };
    let trimmed = log_path.trim();
    if trimmed.is_empty() {
        return;
    }

    let timestamp = chrono::Utc::now().to_rfc3339();
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(trimmed)
    {
        Ok(mut file) => {
            if let Err(err) = writeln!(file, "\n=== [{timestamp}] ===\n{prompt}") {
                eprintln!("Warning: failed to write STITCH_PROMPT_LOG to {trimmed}: {err}");
            }
        }
        Err(err) => {
            eprintln!("Warning: failed to open STITCH_PROMPT_LOG at {trimmed}: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_session_model;

    #[test]
    fn resolve_session_model_passes_auto_and_trims_custom_models() {
        assert_eq!(resolve_session_model(Some("auto")), "auto");
        assert_eq!(resolve_session_model(None), "auto");
        assert_eq!(resolve_session_model(Some("  ")), "auto");
        assert_eq!(
            resolve_session_model(Some(" claude-sonnet-4.5 ")),
            "claude-sonnet-4.5"
        );
    }
}
