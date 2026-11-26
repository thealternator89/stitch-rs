use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, Theme, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, hsla,
    prelude::FluentBuilder as _, px,
};
use tokio::sync::mpsc;

use crate::app_state::{ActiveView, AppState};
use crate::features::code_explainer::service::{
    CodeExplainerService, ExplainStartRequest, ExplainStreamEvent,
};
use crate::infrastructure::copilot::CopilotService;
use crate::infrastructure::git::GitService;
use crate::infrastructure::notifications::NotificationService;
use crate::infrastructure::runtime::{runtime, spawn_tokio};
use crate::types::{AppSettings, CopilotUsage};
use crate::ui::components::{
    ModelDropdown, ModelDropdownEvent, ModelDropdownMode, ResetState, display_repo_name,
    normalize_default_model, open_reset_confirm_modal, open_timeout_modal,
    render_active_error_banner, render_context_pill, render_tool_reset_button,
    summarize_single_line,
};

const TOOL_NAME: &str = "Code Explainer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Investigating,
    DocReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatSender {
    Copilot,
    User,
}

#[derive(Debug, Clone)]
enum FeedItem {
    Status(String),
    Chat {
        uid: usize,
        sender: ChatSender,
        text: String,
    },
}

#[derive(Debug, Clone)]
enum RetryableTurn {
    Start,
    Answer(String),
}

pub struct CodeExplainerView {
    app_state: Entity<AppState>,
    service: Arc<CodeExplainerService>,
    repo_path_input: Entity<InputState>,
    branch_input: Entity<InputState>,
    request_input: Entity<TextareaState>,
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,
    stage: Stage,
    context_expanded: bool,
    is_generating: bool,
    is_waiting_for_user: bool,
    session_run_id: u64,
    feed: Vec<FeedItem>,
    next_feed_uid: usize,
    feed_scroll_handle: ScrollHandle,
    current_suggestions: Vec<String>,
    reply_input: Entity<InputState>,
    doc_markdown: String,
    doc_version: usize,
    is_refining: bool,
    refinement_input: Entity<TextareaState>,
    cumulative_usage: Option<CopilotUsage>,
    active_db_session_id: Option<i64>,
    saved_path: Option<Result<String, String>>,
    copied: bool,
    copy_generation: u64,
    validation_error: Option<String>,
    status_notice: Option<String>,
    last_retry_turn: Option<RetryableTurn>,
    _subscriptions: Vec<Subscription>,
}

impl CodeExplainerView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        copilot_service: Arc<CopilotService>,
        git_service: Arc<GitService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = Arc::new(CodeExplainerService::new(copilot_service, git_service));
        let initial_model = normalize_default_model(&initial_settings.copilot_model);

        let repo_path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/local/project"));
        let branch_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Optional: branch to explore (blank = working copy)")
        });
        let request_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "e.g., Explain how invoice totals and tax are calculated, from the API handler down to the database.",
            )
        });
        let reply_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type your answer (or 'proceed') and press Enter...")
        });
        let refinement_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Describe what to change or expand (e.g., 'Go deeper on the rounding rules' or 'Add a section on retries')...",
            )
        });
        let model_dropdown = cx.new(|cx| {
            ModelDropdown::new(
                app_state.clone(),
                initial_model.clone(),
                Vec::new(),
                ModelDropdownMode::ToolOverride,
                window,
                cx,
            )
        });

        let state_sub = cx.observe_in(&app_state, window, |this, app_state, window, cx| {
            let default_model =
                normalize_default_model(&app_state.read(cx).settings.copilot_model);
            if !this.user_overrode_model && default_model != this.selected_model {
                this.selected_model = default_model.clone();
                this.model_dropdown.update(cx, |dd, cx| {
                    dd.set_selected_model(default_model, window, cx);
                });
            }
            cx.notify();
        });
        let model_sub = cx.subscribe(&model_dropdown, |this, _, event, cx| {
            let ModelDropdownEvent::Changed(model_id) = event;
            this.selected_model = model_id.clone();
            this.user_overrode_model = true;
            cx.notify();
        });
        let repo_sub = cx.subscribe(&repo_path_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let branch_sub = cx.subscribe(&branch_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let reply_sub = cx.subscribe_in(&reply_input, window, |this, _, event, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.submit_reply_input(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });
        let request_sub = cx.subscribe(&request_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let refine_sub = cx.subscribe(&refinement_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        Self {
            app_state,
            service,
            repo_path_input,
            branch_input,
            request_input,
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            stage: Stage::Idle,
            context_expanded: false,
            is_generating: false,
            is_waiting_for_user: false,
            session_run_id: 0,
            feed: Vec::new(),
            next_feed_uid: 1,
            feed_scroll_handle: ScrollHandle::new(),
            current_suggestions: Vec::new(),
            reply_input,
            doc_markdown: String::new(),
            doc_version: 0,
            is_refining: false,
            refinement_input,
            cumulative_usage: None,
            active_db_session_id: None,
            saved_path: None,
            copied: false,
            copy_generation: 0,
            validation_error: None,
            status_notice: None,
            last_retry_turn: None,
            _subscriptions: vec![
                state_sub,
                model_sub,
                repo_sub,
                branch_sub,
                reply_sub,
                request_sub,
                refine_sub,
            ],
        }
    }

    fn browse_repo_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::Idle {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Project Directory".into()),
        });
        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                if let Ok(Ok(Some(paths))) = receiver.await {
                    if let Some(selected) = paths.into_iter().next() {
                        let _ = cx.update(|window, cx| {
                            view.update(cx, |this, cx| {
                                let path_str = selected.to_string_lossy().to_string();
                                this.repo_path_input.update(cx, |input, cx| {
                                    input.set_value(path_str, window, cx);
                                });
                                this.validation_error = None;
                                cx.notify();
                            });
                        });
                    }
                }
            })
            .detach();
    }

    fn set_task(&self, status: Option<String>, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::CodeExplainer, status);
            cx.notify();
        });
    }

    fn handle_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::Idle {
            return;
        }

        let request = self.request_input.read(cx).value().trim().to_string();
        if request.is_empty() {
            self.validation_error =
                Some("Please describe the part of the solution you want explored.".to_string());
            cx.notify();
            return;
        }
        let repo_raw = self.repo_path_input.read(cx).value().trim().to_string();
        if repo_raw.is_empty() {
            self.validation_error = Some("Please select a project directory.".to_string());
            cx.notify();
            return;
        }
        let repo_path = PathBuf::from(&repo_raw);
        if !repo_path.is_dir() {
            self.validation_error = Some(format!(
                "Project path does not exist or is not a directory: {}",
                repo_path.display()
            ));
            cx.notify();
            return;
        }

        let branch = self.branch_input.read(cx).value().trim().to_string();
        let model_override = self.selected_model.clone();
        let settings = self.app_state.read(cx).settings.clone();

        self.validation_error = None;
        self.status_notice = None;
        self.saved_path = None;
        self.feed.clear();
        self.current_suggestions.clear();
        self.doc_markdown.clear();
        self.is_refining = false;
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.copied = false;
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.stage = Stage::Investigating;
        self.context_expanded = false;
        self.is_generating = true;
        self.is_waiting_for_user = false;
        self.last_retry_turn = Some(RetryableTurn::Start);
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;

        self.set_task(Some("Starting explainer session...".to_string()), cx);
        cx.notify();

        let rx = self.service.start(ExplainStartRequest {
            repo_path,
            branch,
            request,
            model_override,
            settings,
        });
        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                Self::drive_turn_stream(view, app_state, run_id, rx, cx).await;
            })
            .detach();
    }

    fn submit_reply_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::Investigating || !self.is_waiting_for_user || self.is_generating {
            return;
        }
        let text = self.reply_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.submit_answer(text, window, cx);
    }

    fn submit_answer(&mut self, answer: String, window: &mut Window, cx: &mut Context<Self>) {
        let trimmed = answer.trim().to_string();
        if trimmed.is_empty() || self.is_generating {
            return;
        }
        self.validation_error = None;
        self.current_suggestions.clear();
        self.push_chat_item(ChatSender::User, trimmed.clone(), cx);
        self.dispatch_answer_turn(trimmed, window, cx);
    }

    fn submit_refinement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::DocReady || self.is_generating {
            return;
        }
        let instructions = self.refinement_input.read(cx).value().trim().to_string();
        if instructions.is_empty() {
            return;
        }
        self.push_chat_item(ChatSender::User, instructions.clone(), cx);
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.is_refining = false;
        self.saved_path = None;
        self.validation_error = None;
        self.stage = Stage::Investigating;
        self.dispatch_answer_turn(
            format!(
                "Revise the documentation as follows, investigating the code further if needed, then return the full updated documentation as a \"documentation\" message:\n{instructions}"
            ),
            window,
            cx,
        );
    }

    fn dispatch_answer_turn(&mut self, prompt_text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.is_generating = true;
        self.is_waiting_for_user = false;
        self.last_retry_turn = Some(RetryableTurn::Answer(prompt_text.clone()));
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;
        self.set_task(Some("Waiting for Copilot response...".to_string()), cx);
        cx.notify();

        let rx = self.service.send_answer(prompt_text);
        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                Self::drive_turn_stream(view, app_state, run_id, rx, cx).await;
            })
            .detach();
    }

    async fn drive_turn_stream(
        view: Entity<Self>,
        app_state: Entity<AppState>,
        run_id: u64,
        mut rx: mpsc::UnboundedReceiver<ExplainStreamEvent>,
        cx: &mut gpui_kit::AsyncWindowContext,
    ) {
        while let Some(event) = rx.recv().await {
            let _ = cx.update(|window, cx| {
                let mut open_timeout: Option<(u64, Option<RetryableTurn>)> = None;

                view.update(cx, |this, cx| {
                    if this.session_run_id != run_id {
                        return;
                    }
                    match event {
                        ExplainStreamEvent::Status(text) => {
                            this.feed.push(FeedItem::Status(text.clone()));
                            this.feed_scroll_handle.scroll_to_bottom();
                            this.set_task(Some(text), cx);
                            cx.notify();
                        }
                        ExplainStreamEvent::Question {
                            text,
                            suggested_answers,
                        } => {
                            this.is_generating = false;
                            this.is_waiting_for_user = true;
                            this.current_suggestions = suggested_answers;
                            this.push_chat_item(ChatSender::Copilot, text, cx);
                            this.set_task(None, cx);
                            NotificationService::show_if_unfocused(
                                window,
                                "Code Explainer Question",
                                "Copilot has a scoping question about your project.",
                            );
                            cx.notify();
                        }
                        ExplainStreamEvent::Documentation(text) => {
                            this.doc_markdown = text;
                            this.doc_version = this.doc_version.wrapping_add(1);
                            this.stage = Stage::DocReady;
                            this.is_generating = false;
                            this.is_waiting_for_user = false;
                            this.is_refining = false;
                            this.current_suggestions.clear();
                            this.set_task(None, cx);
                            NotificationService::show_if_unfocused(
                                window,
                                "Code Explainer Complete",
                                "Documentation is ready for review.",
                            );
                            cx.notify();
                        }
                        ExplainStreamEvent::TurnCompleted {
                            received_question,
                            received_documentation,
                            cumulative_usage,
                        } => {
                            let ctx_ref = {
                                let raw = this.request_input.read(cx).value().trim().to_string();
                                let mut short: String = raw.chars().take(80).collect();
                                if raw.chars().count() > 80 {
                                    short.push('…');
                                }
                                format!("Explain - {short}")
                            };
                            let ai_output = if this.stage == Stage::DocReady || received_documentation
                            {
                                "Documentation"
                            } else {
                                "Explainer Session"
                            };
                            let model_id = this.selected_model.clone();
                            let existing_sid = this.active_db_session_id;
                            let updated_sid = app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::CodeExplainer, None);
                                let sid = state.upsert_tool_session_usage(
                                    existing_sid,
                                    TOOL_NAME,
                                    Some(&ctx_ref),
                                    Some(ai_output),
                                    "Code Explainer Session",
                                    &model_id,
                                    &cumulative_usage,
                                    received_documentation,
                                    cx,
                                );
                                cx.notify();
                                sid
                            });
                            this.active_db_session_id = updated_sid;
                            this.cumulative_usage = Some(cumulative_usage);
                            this.is_generating = false;
                            if !received_question
                                && !received_documentation
                                && this.stage == Stage::Investigating
                            {
                                this.is_waiting_for_user = true;
                                this.validation_error = Some(
                                    "Copilot finished the turn without sending a structured question or the final documentation. Send a follow-up prompt below or click Reset to start over."
                                        .to_string(),
                                );
                            }
                            cx.notify();
                        }
                        ExplainStreamEvent::TimedOut { timeout_secs } => {
                            this.is_generating = false;
                            if this.stage == Stage::Investigating {
                                this.is_waiting_for_user = true;
                            }
                            this.validation_error = Some(format!(
                                "Code explanation timed out after {timeout_secs} seconds without Copilot output."
                            ));
                            this.set_task(None, cx);
                            open_timeout = Some((timeout_secs, this.last_retry_turn.clone()));
                            cx.notify();
                        }
                        ExplainStreamEvent::Cancelled => {
                            this.is_generating = false;
                            this.set_task(None, cx);
                            cx.notify();
                        }
                        ExplainStreamEvent::Error(msg) => {
                            this.is_generating = false;
                            if this.stage == Stage::Investigating {
                                let has_conversation = this
                                    .feed
                                    .iter()
                                    .any(|item| matches!(item, FeedItem::Chat { .. }));
                                if !has_conversation {
                                    this.stop_active_session_only(cx);
                                } else {
                                    this.is_waiting_for_user = true;
                                }
                            }
                            this.validation_error = Some(msg);
                            this.set_task(None, cx);
                            cx.notify();
                        }
                    }
                });

                if let Some((secs, retry_turn)) = open_timeout {
                    let view_for_retry = view.clone();
                    open_timeout_modal(
                        TOOL_NAME,
                        secs,
                        move |window, cx| {
                            view_for_retry.update(cx, |this, cx| match retry_turn.clone() {
                                Some(RetryableTurn::Start) => {
                                    this.stop_active_session_only(cx);
                                    this.handle_start(window, cx);
                                }
                                Some(RetryableTurn::Answer(prompt_text)) => {
                                    this.validation_error = None;
                                    this.dispatch_answer_turn(prompt_text, window, cx);
                                }
                                None => {}
                            });
                        },
                        window,
                        cx,
                    );
                }
            });
        }
    }

    fn stop_active_session_only(&mut self, cx: &mut Context<Self>) {
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let service = Arc::clone(&self.service);
        runtime().spawn(async move {
            service.stop().await;
        });
        self.stage = Stage::Idle;
        self.context_expanded = false;
        self.is_generating = false;
        self.is_waiting_for_user = false;
        cx.notify();
    }

    fn stop_and_reset(&mut self, cancelled_by_user: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_active_session_only(cx);
        self.feed.clear();
        self.current_suggestions.clear();
        self.doc_markdown.clear();
        self.is_refining = false;
        self.saved_path = None;
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.copied = false;
        self.validation_error = None;
        self.last_retry_turn = None;
        self.status_notice = cancelled_by_user.then(|| "Session cancelled.".to_string());
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.set_task(None, cx);
        cx.notify();
    }

    fn reset_state(&self, cx: &App) -> ResetState {
        if self.is_generating {
            return ResetState::NeedsConfirmation { is_running: true };
        }
        if self.stage != Stage::Idle || !self.doc_markdown.is_empty() || !self.feed.is_empty() {
            return ResetState::NeedsConfirmation { is_running: false };
        }
        let has_dirty_inputs = !self.repo_path_input.read(cx).value().trim().is_empty()
            || !self.branch_input.read(cx).value().trim().is_empty()
            || !self.request_input.read(cx).value().trim().is_empty()
            || self.user_overrode_model
            || self.validation_error.is_some()
            || self.status_notice.is_some()
            || self.saved_path.is_some();
        if has_dirty_inputs {
            ResetState::DirtyInputsOnly
        } else {
            ResetState::Clean
        }
    }

    fn request_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.reset_state(cx) {
            ResetState::Clean => {}
            ResetState::DirtyInputsOnly => self.reset(window, cx),
            ResetState::NeedsConfirmation { is_running } => {
                let view = cx.entity();
                let detail = if is_running {
                    "This will cancel the active investigation, stop the Copilot session, clean up any temporary worktree, and clear all inputs and conversation history."
                } else {
                    "This will end the current explainer session and clear the generated documentation, conversation history, and all form inputs."
                };
                open_reset_confirm_modal(
                    TOOL_NAME,
                    is_running,
                    detail,
                    move |window, cx| {
                        view.update(cx, |this, cx| {
                            this.reset(window, cx);
                        });
                    },
                    window,
                    cx,
                );
            }
        }
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_generation = self.copy_generation.wrapping_add(1);
        self.stop_and_reset(false, window, cx);
        self.repo_path_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.branch_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.request_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        let default_model =
            normalize_default_model(&self.app_state.read(cx).settings.copilot_model);
        self.user_overrode_model = false;
        self.selected_model = default_model.clone();
        self.model_dropdown.update(cx, |dd, cx| {
            dd.set_selected_model(default_model, window, cx);
        });

        self.status_notice = None;
        cx.notify();
    }

    fn push_chat_item(&mut self, sender: ChatSender, text: String, cx: &mut Context<Self>) {
        let uid = self.next_feed_uid;
        self.next_feed_uid += 1;
        self.feed.push(FeedItem::Chat { uid, sender, text });
        self.feed_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn copy_doc(&mut self, cx: &mut Context<Self>) {
        if self.doc_markdown.trim().is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.doc_markdown.clone()));
        self.copied = true;
        self.copy_generation = self.copy_generation.wrapping_add(1);
        let copy_gen = self.copy_generation;
        cx.notify();
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = view.update(cx, |this, cx| {
                if this.copy_generation == copy_gen {
                    this.copied = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn save_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc_markdown.trim().is_empty() {
            return;
        }
        let start_dir = {
            let raw = self.repo_path_input.read(cx).value().trim().to_string();
            let p = PathBuf::from(raw);
            if p.is_dir() {
                p
            } else {
                std::env::temp_dir()
            }
        };
        let suggested = suggested_file_name(&self.doc_markdown);
        let receiver = cx.prompt_for_new_path(&start_dir, Some(&suggested));
        let markdown = self.doc_markdown.clone();
        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                if let Ok(Ok(Some(path))) = receiver.await {
                    let write_path = path.clone();
                    let res = spawn_tokio(async move {
                        tokio::fs::write(&write_path, markdown.as_bytes()).await
                    })
                    .await;
                    let outcome = match res {
                        Ok(Ok(())) => Ok(path.to_string_lossy().to_string()),
                        Ok(Err(err)) => Err(err.to_string()),
                        Err(err) => Err(err.to_string()),
                    };
                    let _ = cx.update(|_, cx| {
                        view.update(cx, |this, cx| {
                            this.saved_path = Some(outcome);
                            cx.notify();
                        });
                    });
                }
            })
            .detach();
    }

    fn render_left_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let worktree_enabled = self.app_state.read(cx).settings.git_worktree_enabled;
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let can_start = !self.request_input.read(cx).value().trim().is_empty()
            && !self.repo_path_input.read(cx).value().trim().is_empty();

        let label = |text: &'static str| {
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(text)
        };
        let hint = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };

        v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .bg(theme.sidebar)
            .p_5()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(label("Project Directory"))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.repo_path_input)),
                            )
                            .child(
                                Button::new("explain-browse-repo")
                                    .small()
                                    .icon(Icon::new(LucideIcon::FolderOpen).small())
                                    .label("Browse")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.browse_repo_dir(window, cx);
                                    })),
                            ),
                    )
                    .child(hint(
                        "Copilot gets read-only access to this folder to investigate the code.",
                    )),
            )
            .when(worktree_enabled, |col| {
                col.child(
                    v_flex()
                        .gap_1p5()
                        .child(label("Git Branch (Optional)"))
                        .child(Input::new(&self.branch_input))
                        .child(hint(
                            "If set, the branch is checked out in an isolated worktree. Leave blank to read the working copy as-is.",
                        )),
                )
            })
            .child(
                v_flex()
                    .gap_1p5()
                    .child(label("What should be explained?"))
                    .child(Textarea::new(&self.request_input).h(px(160.)))
                    .child(hint(
                        "Describe the feature or area in your own words. Tip: mention entry points (files, functions, endpoints, UI screens), key terms, and what you care about (e.g. calculations, data flow).",
                    )),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(label("Copilot Model"))
                    .child(self.model_dropdown.clone()),
            )
            .when_some(self.validation_error.clone(), |col, err| {
                col.child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .items_start()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(red.opacity(0.4))
                        .bg(red.opacity(0.08))
                        .text_xs()
                        .text_color(red)
                        .child(Icon::new(LucideIcon::CircleAlert).small())
                        .child(div().flex_1().child(err)),
                )
            })
            .when_some(self.status_notice.clone(), |col, note| {
                col.child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .items_start()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(green.opacity(0.4))
                        .bg(green.opacity(0.08))
                        .text_xs()
                        .text_color(green)
                        .child(Icon::new(LucideIcon::Info).small())
                        .child(div().flex_1().child(note)),
                )
            })
            .child(
                div().pt_1().child(
                    Button::new("explain-start")
                        .primary()
                        .w_full()
                        .disabled(!can_start)
                        .icon(Icon::new(LucideIcon::Rocket).small())
                        .label("Start Investigation")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_start(window, cx);
                        })),
                ),
            )
    }

    fn render_context_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let worktree_enabled = self.app_state.read(cx).settings.git_worktree_enabled;
        let repo_raw = self.repo_path_input.read(cx).value().trim().to_string();
        let repo_name = display_repo_name(&repo_raw);
        let branch_raw = self.branch_input.read(cx).value().trim().to_string();
        let branch_label = if branch_raw.is_empty() {
            "Working copy".to_string()
        } else {
            branch_raw
        };
        let request_raw = self.request_input.read(cx).value().trim().to_string();
        let request_summary = summarize_single_line(&request_raw, 140);
        let is_running = self.stage == Stage::Investigating && self.is_generating;
        let chevron = if self.context_expanded {
            LucideIcon::ChevronUp
        } else {
            LucideIcon::ChevronDown
        };

        v_flex()
            .w_full()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_6()
                    .py_2p5()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .when(!repo_name.is_empty(), |row| {
                                        row.child(render_context_pill(
                                            LucideIcon::Folder,
                                            repo_name,
                                            false,
                                            cx,
                                        ))
                                    })
                                    .when(worktree_enabled, |row| {
                                        row.child(render_context_pill(
                                            LucideIcon::GitMerge,
                                            branch_label.clone(),
                                            true,
                                            cx,
                                        ))
                                    })
                                    .child(render_context_pill(
                                        LucideIcon::Cpu,
                                        self.selected_model.clone(),
                                        false,
                                        cx,
                                    )),
                            )
                            .when(!request_summary.is_empty(), |col| {
                                col.child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(request_summary),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(
                                Button::new("explain-toggle-context")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(chevron).small())
                                    .label(if self.context_expanded {
                                        "Hide Details"
                                    } else {
                                        "Details"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.context_expanded = !this.context_expanded;
                                        cx.notify();
                                    })),
                            )
                            .when(is_running, |row| {
                                row.child(
                                    Button::new("explain-cancel")
                                        .danger()
                                        .small()
                                        .icon(Icon::new(LucideIcon::Square).small())
                                        .label("Cancel")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.stop_and_reset(true, window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .when(self.context_expanded, |bar| {
                bar.child(
                    v_flex()
                        .gap_3()
                        .px_6()
                        .py_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(
                            h_flex()
                                .flex_wrap()
                                .items_center()
                                .gap_4()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_1p5()
                                        .text_xs()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(theme.muted_foreground)
                                                .child("Project Directory:"),
                                        )
                                        .child(
                                            div()
                                                .font_family(theme.mono_font_family.clone())
                                                .text_color(theme.foreground)
                                                .child(repo_raw),
                                        ),
                                )
                                .when(worktree_enabled, |row| {
                                    row.child(
                                        h_flex()
                                            .items_center()
                                            .gap_1p5()
                                            .text_xs()
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.muted_foreground)
                                                    .child("Branch:"),
                                            )
                                            .child(
                                                div()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .text_color(theme.foreground)
                                                    .child(branch_label),
                                            ),
                                    )
                                }),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.muted_foreground)
                                        .child("What should be explained?"),
                                )
                                .child(
                                    div()
                                        .max_h(px(160.))
                                        .overflow_y_scrollbar()
                                        .p_3()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.sidebar)
                                        .text_xs()
                                        .text_color(theme.foreground)
                                        .child(request_raw),
                                ),
                        ),
                )
            })
    }

    fn render_right_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);

        let stage_pill: Option<(&'static str, gpui_kit::Hsla)> = match self.stage {
            Stage::Idle => None,
            Stage::Investigating if self.is_generating => Some(("Investigating...", theme.primary)),
            Stage::Investigating if self.is_waiting_for_user => {
                Some(("Waiting for reply", indigo))
            }
            Stage::Investigating => Some(("Active session", theme.muted_foreground)),
            Stage::DocReady => Some(("Documentation ready", green)),
        };

        let usage_summary = self.cumulative_usage.as_ref().and_then(|u| {
            if u.input_tokens == 0 && u.output_tokens == 0 {
                return None;
            }
            let model_suffix = u
                .model
                .as_deref()
                .map(|m| format!(" • {m}"))
                .unwrap_or_default();
            Some(format!(
                "{} in / {} out{model_suffix}",
                u.input_tokens, u.output_tokens
            ))
        });

        let body: AnyElement = match self.stage {
            Stage::Idle => v_flex()
                .size_full()
                .p_8()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(56.))
                        .rounded(theme.radius_lg)
                        .bg(theme.secondary)
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(LucideIcon::BookOpen).large()),
                )
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Document Part of a Project"),
                )
                .child(
                    div()
                        .max_w(px(460.))
                        .text_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Pick a project and describe the area you want explained. Copilot will investigate the code, confirm scope with a few questions, then write comprehensive documentation."),
                )
                .into_any_element(),
            Stage::Investigating => self.render_investigating_stage(&theme, cx),
            Stage::DocReady => self.render_doc_stage(&theme, cx),
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(LucideIcon::BookOpen).small())
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Investigation"),
                            )
                            .when(self.stage == Stage::Investigating && self.is_generating, |row| {
                                row.child(Spinner::new().small().color(theme.primary))
                            })
                            .when_some(stage_pill, |row, (label, color)| {
                                row.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(color.opacity(0.4))
                                        .bg(color.opacity(0.1))
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(color)
                                        .child(label),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .when_some(usage_summary, |row, summary| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(summary),
                                )
                            })
                            .when(self.stage == Stage::DocReady, |row| {
                                row.child(
                                    Button::new("explain-copy-doc")
                                        .small()
                                        .icon(
                                            Icon::new(if self.copied {
                                                LucideIcon::Check
                                            } else {
                                                LucideIcon::Copy
                                            })
                                            .small(),
                                        )
                                        .label(if self.copied { "Copied!" } else { "Copy" })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.copy_doc(cx);
                                        })),
                                )
                            }),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().overflow_hidden().child(body))
    }

    fn render_investigating_stage(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mono_font = theme.mono_font_family.clone();
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);
        let reply_has_text = !self.reply_input.read(cx).value().trim().is_empty();
        let can_reply = self.is_waiting_for_user && !self.is_generating;

        let feed_elements: Vec<AnyElement> = self
            .feed
            .iter()
            .map(|item| match item {
                FeedItem::Status(text) => h_flex()
                    .w_full()
                    .justify_start()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .max_w(px(760.))
                            .px_3()
                            .py_1p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .font_family(mono_font.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(LucideIcon::Terminal).xsmall())
                            .child(div().flex_1().min_w_0().child(text.clone())),
                    )
                    .into_any_element(),
                FeedItem::Chat {
                    uid,
                    sender: ChatSender::Copilot,
                    text,
                } => h_flex()
                    .w_full()
                    .justify_start()
                    .child(
                        v_flex()
                            .max_w(px(760.))
                            .p_4()
                            .gap_1p5()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(LucideIcon::Bot).xsmall())
                                    .child("Copilot"),
                            )
                            .child(
                                div().text_sm().text_color(theme.foreground).child(
                                    TextView::markdown(("explain-msg", *uid), text.clone())
                                        .selectable(true),
                                ),
                            ),
                    )
                    .into_any_element(),
                FeedItem::Chat {
                    sender: ChatSender::User,
                    text,
                    ..
                } => h_flex()
                    .w_full()
                    .justify_end()
                    .child(
                        v_flex()
                            .max_w(px(560.))
                            .p_3p5()
                            .gap_1()
                            .rounded(theme.radius_lg)
                            .bg(indigo)
                            .text_color(gpui_kit::white())
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(gpui_kit::white().opacity(0.85))
                                    .child(Icon::new(LucideIcon::User).xsmall())
                                    .child("You"),
                            )
                            .child(div().text_sm().child(text.clone())),
                    )
                    .into_any_element(),
            })
            .collect();

        v_flex()
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .id("explain-feed-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.feed_scroll_handle)
                            .p_6()
                            .gap_3()
                            .when(feed_elements.is_empty(), |col| {
                                col.child(
                                    v_flex()
                                        .py_12()
                                        .items_center()
                                        .justify_center()
                                        .gap_3()
                                        .child(Spinner::new().large().color(theme.primary))
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme.muted_foreground)
                                                .child("Initializing Copilot explainer session..."),
                                        ),
                                )
                            })
                            .children(feed_elements)
                            .when(self.is_generating && !self.feed.is_empty(), |col| {
                                col.child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .pt_1()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(Spinner::new().small().color(theme.primary))
                                        .child("Copilot is working..."),
                                )
                            }),
                    )
                    .vertical_scrollbar(&self.feed_scroll_handle),
            )
            .child(
                v_flex()
                    .gap_3()
                    .px_6()
                    .py_4()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .when(
                        self.is_waiting_for_user && !self.current_suggestions.is_empty(),
                        |footer| {
                            let chips = self.current_suggestions.iter().enumerate().map(|(idx, sug)| {
                                let value = sug.clone();
                                Button::new(("explain-suggestion", idx))
                                    .small()
                                    .disabled(self.is_generating)
                                    .label(sug.clone())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.submit_answer(value.clone(), window, cx);
                                    }))
                            });
                            footer.child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.muted_foreground)
                                            .child("Suggested answers (click to reply):"),
                                    )
                                    .child(h_flex().flex_wrap().gap_2().children(chips)),
                            )
                        },
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.reply_input).disabled(!can_reply)),
                            )
                            .child(
                                Button::new("explain-proceed")
                                    .small()
                                    .disabled(!can_reply)
                                    .label("Proceed")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_answer(
                                            "Proceed with what you know now and write the documentation."
                                                .to_string(),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("explain-send-answer")
                                    .primary()
                                    .disabled(!can_reply || !reply_has_text)
                                    .loading(self.is_generating)
                                    .icon(Icon::new(LucideIcon::Send).small())
                                    .label("Send")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_reply_input(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_doc_stage(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let refinement_non_empty = !self.refinement_input.read(cx).value().trim().is_empty();

        v_flex()
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .p_6()
                    .child(
                        div()
                            .p_5()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                TextView::markdown(
                                    ("explain-final-doc", self.doc_version),
                                    self.doc_markdown.clone(),
                                )
                                .selectable(true),
                            ),
                    ),
            )
            .when(self.is_refining, |pane| {
                pane.child(
                    v_flex()
                        .gap_2p5()
                        .px_6()
                        .py_4()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.sidebar)
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child("What should be changed or expanded?"),
                        )
                        .child(Textarea::new(&self.refinement_input).h(px(88.)))
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("explain-cancel-refine")
                                        .ghost()
                                        .small()
                                        .label("Cancel")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.is_refining = false;
                                            this.refinement_input.update(cx, |input, cx| {
                                                input.set_value("", window, cx);
                                            });
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("explain-submit-refine")
                                        .primary()
                                        .small()
                                        .disabled(!refinement_non_empty)
                                        .label("Submit Refinement")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.submit_refinement(window, cx);
                                        })),
                                ),
                        ),
                )
            })
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_6()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_3()
                            .when_some(self.saved_path.clone(), |row, fb| {
                                let (color, icon, msg) = match fb {
                                    Ok(p) => (green, LucideIcon::FileCheck, format!("Saved to {p}")),
                                    Err(e) => (red, LucideIcon::CircleAlert, e),
                                };
                                row.child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .items_center()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(color)
                                        .child(Icon::new(icon).small())
                                        .child(div().flex_1().min_w_0().truncate().child(msg)),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .when(!self.is_refining, |row| {
                                row.child(
                                    Button::new("explain-refine")
                                        .small()
                                        .icon(Icon::new(LucideIcon::Pencil).small())
                                        .label("Refine")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.is_refining = true;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("explain-save-doc")
                                    .primary()
                                    .small()
                                    .disabled(self.is_refining)
                                    .icon(Icon::new(LucideIcon::Save).small())
                                    .label("Save as Markdown")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_doc(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for CodeExplainerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, border, radius, secondary, accent_fg, muted_fg) = {
            let theme = cx.theme();
            (
                theme.background,
                theme.border,
                theme.radius_lg,
                theme.secondary,
                theme.accent_foreground,
                theme.muted_foreground,
            )
        };

        let reset_state = self.reset_state(cx);
        let view_for_reset = cx.entity();

        v_flex()
            .size_full()
            .bg(bg)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(36.))
                                    .rounded(radius)
                                    .bg(secondary)
                                    .text_color(accent_fg)
                                    .child(
                                        Icon::new(ActiveView::CodeExplainer.icon())
                                            .with_size(Size::Medium),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(ActiveView::CodeExplainer.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::CodeExplainer.subtitle()),
                                    ),
                            ),
                    )
                    .child(render_tool_reset_button(
                        "explain-header-reset",
                        reset_state,
                        move |window, cx| {
                            view_for_reset.update(cx, |this, cx| {
                                this.request_reset(window, cx);
                            });
                        },
                    )),
            )
            .child(div().flex_1().min_h_0().w_full().child(
                if self.stage == Stage::Idle {
                    h_resizable("code-explainer-split")
                        .child(
                            resizable_panel()
                                .size(px(390.))
                                .size_range(px(320.)..px(540.))
                                .child(self.render_left_pane(cx)),
                        )
                        .child(resizable_panel().child(self.render_right_pane(cx)))
                        .into_any_element()
                } else {
                    let view_for_dismiss = cx.entity();
                    v_flex()
                        .size_full()
                        .child(self.render_context_bar(cx))
                        .when_some(self.validation_error.clone(), |col, err| {
                            col.child(render_active_error_banner(
                                "explain-dismiss-active-error",
                                err,
                                move |_, cx| {
                                    view_for_dismiss.update(cx, |this, cx| {
                                        this.validation_error = None;
                                        cx.notify();
                                    });
                                },
                                cx,
                            ))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .child(self.render_right_pane(cx)),
                        )
                        .into_any_element()
                },
            ))
    }
}

/// Derives a filename like `how-invoices-are-totalled.md` from the document's first heading.
fn suggested_file_name(markdown: &str) -> String {
    let title = markdown
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .unwrap_or_default();
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(60).collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "code-explanation.md".to_string()
    } else {
        format!("{slug}.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_file_name_uses_first_heading() {
        assert_eq!(
            suggested_file_name("\n# Invoice Totals & Tax!\n\ntext"),
            "invoice-totals-tax.md"
        );
        assert_eq!(suggested_file_name("no heading"), "code-explanation.md");
        assert_eq!(suggested_file_name("# ???"), "code-explanation.md");
    }
}
