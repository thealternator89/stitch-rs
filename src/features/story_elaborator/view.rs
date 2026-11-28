use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
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
use crate::features::settings::SettingsTab;
use crate::features::story_elaborator::service::{
    ElaborationStartRequest, ElaborationStreamEvent, StoryElaboratorService,
};
use crate::infrastructure::copilot::CopilotService;
use crate::infrastructure::git::GitService;
use crate::infrastructure::notifications::NotificationService;
use crate::infrastructure::providers::build_issue_tracker_provider;
use crate::infrastructure::providers::confluence::strip_html_for_preview;
use crate::infrastructure::runtime::{runtime, spawn_tokio};
use crate::types::{AppSettings, CopilotUsage, TicketData};
use crate::ui::components::{
    AutocompleteEvent, AutocompleteInput, AutocompleteOption, DirectIdMode, MarkdownView,
    ModelDropdown, ModelDropdownEvent, ModelDropdownMode, ResetState, display_repo_name,
    normalize_default_model, open_reset_confirm_modal, open_timeout_modal,
    render_active_error_banner, render_context_pill, render_tool_reset_button,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElaboratorStage {
    Idle,
    Elaborating,
    PlanCompleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatSender {
    Copilot,
    User,
}

#[derive(Debug, Clone)]
enum FeedItem {
    Status {
        text: String,
    },
    Chat {
        uid: usize,
        sender: ChatSender,
        text: String,
    },
    RefinedPlan {
        uid: usize,
        markdown: String,
        expanded: bool,
    },
}

#[derive(Debug, Clone)]
enum RetryableTurn {
    Start,
    Answer(String),
}

pub struct StoryElaboratorView {
    app_state: Entity<AppState>,
    service: Arc<StoryElaboratorService>,
    ticket_search: Entity<AutocompleteInput>,
    ticket_search_cache: HashMap<String, TicketData>,
    selected_ticket_id: Option<String>,
    active_ticket_id: Option<String>,
    fetched_ticket: Option<TicketData>,
    fetching_ticket: bool,
    ticket_fetch_generation: u64,
    repo_path_input: Entity<InputState>,
    branch_input: Entity<InputState>,
    additional_context_input: Entity<TextareaState>,
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,
    stage: ElaboratorStage,
    context_expanded: bool,
    is_generating: bool,
    is_waiting_for_user: bool,
    session_run_id: u64,
    feed: Vec<FeedItem>,
    next_feed_uid: usize,
    feed_scroll_handle: ScrollHandle,
    current_suggestions: Vec<String>,
    reply_input: Entity<InputState>,
    plan_markdown: String,
    plan_file_path: Option<String>,
    plan_version: usize,
    is_refining: bool,
    refinement_input: Entity<TextareaState>,
    cumulative_usage: Option<CopilotUsage>,
    active_db_session_id: Option<i64>,
    is_posting: bool,
    publish_feedback: Option<Result<String, String>>,
    publish_gen: u64,
    copied: bool,
    copy_generation: u64,
    validation_error: Option<String>,
    status_notice: Option<String>,
    last_retry_turn: Option<RetryableTurn>,
    _subscriptions: Vec<Subscription>,
}

impl StoryElaboratorView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        copilot_service: Arc<CopilotService>,
        git_service: Arc<GitService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = Arc::new(StoryElaboratorService::new(copilot_service, git_service));
        let initial_model = normalize_default_model(&initial_settings.copilot_model);

        let initial_mode = ticket_id_mode_for(initial_settings);
        let ticket_search = cx.new(|cx| {
            AutocompleteInput::new(
                "elab-ticket-search",
                "Search tickets by title, text or ID...",
                initial_mode,
                "No tickets found.",
                "Searching work items...",
                window,
                cx,
            )
        });

        let repo_path_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("/path/to/local/repo (optional)")
        });
        let branch_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("develop")
                .default_value("develop")
        });
        let additional_context_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "e.g., steer Copilot to focus on frontend architecture, database setup, or edge cases...",
            )
        });
        let reply_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Type your answer and press Enter...")
        });
        let refinement_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Describe what changes you want to make (e.g., 'Split into two PRs' or 'Add migration rollback steps')...",
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
            let (default_model, next_mode) = {
                let settings = &app_state.read(cx).settings;
                (
                    normalize_default_model(&settings.copilot_model),
                    ticket_id_mode_for(settings),
                )
            };
            if !this.user_overrode_model && default_model != this.selected_model {
                this.selected_model = default_model.clone();
                this.model_dropdown.update(cx, |dd, cx| {
                    dd.set_selected_model(default_model, window, cx);
                });
            }
            this.ticket_search.update(cx, |input, cx| {
                input.set_direct_id_mode(next_mode, cx);
            });
            cx.notify();
        });

        let model_sub = cx.subscribe(&model_dropdown, |this, _, event, cx| {
            let ModelDropdownEvent::Changed(model_id) = event;
            this.selected_model = model_id.clone();
            this.user_overrode_model = true;
            cx.notify();
        });

        let ticket_sub =
            cx.subscribe_in(&ticket_search, window, |this, _, event, window, cx| {
                this.handle_ticket_autocomplete_event(event.clone(), window, cx);
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
        let context_sub = cx.subscribe(&additional_context_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        let reply_sub = cx.subscribe_in(&reply_input, window, |this, _, event, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => {
                    this.submit_reply_input(window, cx);
                }
                InputEvent::Change => {
                    cx.notify();
                }
                _ => {}
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
            ticket_search,
            ticket_search_cache: HashMap::new(),
            selected_ticket_id: None,
            active_ticket_id: None,
            fetched_ticket: None,
            fetching_ticket: false,
            ticket_fetch_generation: 0,
            repo_path_input,
            branch_input,
            additional_context_input,
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            stage: ElaboratorStage::Idle,
            context_expanded: false,
            is_generating: false,
            is_waiting_for_user: false,
            session_run_id: 0,
            feed: Vec::new(),
            next_feed_uid: 1,
            feed_scroll_handle: ScrollHandle::new(),
            current_suggestions: Vec::new(),
            reply_input,
            plan_markdown: String::new(),
            plan_file_path: None,
            plan_version: 0,
            is_refining: false,
            refinement_input,
            cumulative_usage: None,
            active_db_session_id: None,
            is_posting: false,
            publish_feedback: None,
            publish_gen: 0,
            copied: false,
            copy_generation: 0,
            validation_error: None,
            status_notice: None,
            last_retry_turn: None,
            _subscriptions: vec![
                state_sub,
                model_sub,
                ticket_sub,
                repo_sub,
                branch_sub,
                context_sub,
                reply_sub,
                refine_sub,
            ],
        }
    }

    fn browse_repo_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != ElaboratorStage::Idle {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Local Repository Directory".into()),
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

    fn handle_ticket_autocomplete_event(
        &mut self,
        event: AutocompleteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            AutocompleteEvent::SearchRequested { query, request_id } => {
                let settings = self.app_state.read(cx).settings.clone();
                let view = cx.entity();
                window
                    .spawn(cx, async move |cx| {
                        let res = spawn_tokio(async move {
                            let provider = build_issue_tracker_provider(&settings)?;
                            provider.search_tickets(&query, None).await
                        })
                        .await;

                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                let mapped = match res {
                                    Ok(Ok(tickets)) => {
                                        let mut opts = Vec::with_capacity(tickets.len());
                                        for ticket in tickets {
                                            if let Some(id) = ticket.id.clone() {
                                                this.ticket_search_cache
                                                    .insert(id.clone(), ticket.clone());
                                                opts.push(AutocompleteOption {
                                                    id,
                                                    title: ticket.title,
                                                    subtitle: None,
                                                });
                                            }
                                        }
                                        Ok(opts)
                                    }
                                    Ok(Err(err)) => Err(err.to_string()),
                                    Err(err) => Err(err.to_string()),
                                };
                                this.ticket_search.update(cx, |input, cx| {
                                    input.set_results(request_id, mapped, cx);
                                });
                            });
                        });
                    })
                    .detach();
            }
            AutocompleteEvent::Selected(opt) => {
                self.selected_ticket_id = Some(opt.id.clone());
                self.validation_error = None;
                self.status_notice = None;
                if let Some(cached) = self.ticket_search_cache.get(&opt.id).cloned() {
                    self.fetched_ticket = Some(cached);
                    cx.notify();
                } else {
                    self.fetch_ticket_by_id(&opt.id, window, cx);
                }
            }
            AutocompleteEvent::DirectValueChanged(maybe_id) => {
                self.selected_ticket_id = maybe_id;
                if self.selected_ticket_id.is_none() {
                    self.ticket_fetch_generation = self.ticket_fetch_generation.wrapping_add(1);
                    self.fetched_ticket = None;
                    self.fetching_ticket = false;
                }
                cx.notify();
            }
            AutocompleteEvent::Cleared => {
                self.ticket_fetch_generation = self.ticket_fetch_generation.wrapping_add(1);
                self.selected_ticket_id = None;
                self.fetched_ticket = None;
                self.fetching_ticket = false;
                self.ticket_search_cache.clear();
                cx.notify();
            }
        }
    }

    fn fetch_ticket_by_id(
        &mut self,
        ticket_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.app_state.read(cx).settings.clone();
        let ticket_id_owned = ticket_id.trim().to_string();
        if ticket_id_owned.is_empty() {
            return;
        }

        self.ticket_fetch_generation = self.ticket_fetch_generation.wrapping_add(1);
        let fetch_gen = self.ticket_fetch_generation;
        self.fetching_ticket = true;
        self.validation_error = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_issue_tracker_provider(&settings)?;
                    provider.fetch_ticket(&ticket_id_owned).await
                })
                .await;

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.ticket_fetch_generation != fetch_gen {
                            return;
                        }
                        this.fetching_ticket = false;
                        match res {
                            Ok(Ok(ticket)) => {
                                if let Some(id) = ticket.id.clone() {
                                    this.ticket_search_cache.insert(id, ticket.clone());
                                }
                                this.fetched_ticket = Some(ticket);
                            }
                            Ok(Err(err)) => {
                                this.validation_error = Some(err.to_string());
                            }
                            Err(err) => {
                                this.validation_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn handle_start_elaboration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != ElaboratorStage::Idle {
            return;
        }

        let Some(ticket_id) = self
            .selected_ticket_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            self.validation_error =
                Some("Please search for a ticket or enter a Ticket ID.".to_string());
            cx.notify();
            return;
        };

        let repo_raw = self.repo_path_input.read(cx).value().trim().to_string();
        let repo_path = if repo_raw.is_empty() {
            None
        } else {
            let pb = PathBuf::from(&repo_raw);
            if !pb.is_dir() {
                self.validation_error = Some(format!(
                    "Repository path does not exist or is not a directory: {}",
                    pb.display()
                ));
                cx.notify();
                return;
            }
            Some(pb)
        };

        let branch = {
            let raw = self.branch_input.read(cx).value().trim().to_string();
            if raw.is_empty() {
                "develop".to_string()
            } else {
                raw
            }
        };

        let additional_context = self.additional_context_input.read(cx).value().to_string();
        let model_override = self.selected_model.clone();
        let settings = self.app_state.read(cx).settings.clone();

        let previous_ticket = self.active_ticket_id.take();
        self.active_ticket_id = Some(ticket_id.clone());
        self.validation_error = None;
        self.status_notice = None;
        self.publish_feedback = None;
        self.feed.clear();
        self.current_suggestions.clear();
        self.plan_markdown.clear();
        self.plan_file_path = None;
        self.is_refining = false;
        self.cumulative_usage = None;
        self.copied = false;
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        self.stage = ElaboratorStage::Elaborating;
        self.context_expanded = false;
        self.is_generating = true;
        self.is_waiting_for_user = false;
        self.fetching_ticket = true;
        self.last_retry_turn = Some(RetryableTurn::Start);
        self.session_run_id = self.session_run_id.wrapping_add(1);
        self.ticket_fetch_generation = self.ticket_fetch_generation.wrapping_add(1);
        let run_id = self.session_run_id;
        let fetch_gen = self.ticket_fetch_generation;

        self.ticket_search
            .update(cx, |input, cx| input.set_disabled(true, cx));

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::StoryElaborator,
                Some("Fetching ticket details...".to_string()),
            );
            cx.notify();
        });
        cx.notify();

        let view = cx.entity();
        let service = Arc::clone(&self.service);
        let app_state = self.app_state.clone();

        window
            .spawn(cx, async move |cx| {
                let settings_for_fetch = settings.clone();
                let ticket_id_for_fetch = ticket_id.clone();
                let res = spawn_tokio(async move {
                    if let Some(prev) = previous_ticket {
                        service.stop_elaboration(&prev).await;
                    }
                    let provider = build_issue_tracker_provider(&settings_for_fetch)?;
                    let mut ticket = provider.fetch_ticket(&ticket_id_for_fetch).await?;
                    if ticket
                        .id
                        .as_deref()
                        .map(str::trim)
                        .is_none_or(|s| s.is_empty())
                    {
                        ticket.id = Some(ticket_id_for_fetch);
                    }
                    Ok::<_, anyhow::Error>(ticket)
                })
                .await;

                let fetched_ticket = match res {
                    Ok(Ok(ticket)) => ticket,
                    Ok(Err(err)) => {
                        let msg = err.to_string();
                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                if this.session_run_id != run_id {
                                    return;
                                }
                                this.reset_to_idle_on_error(msg, fetch_gen, cx);
                            });
                        });
                        return;
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                if this.session_run_id != run_id {
                                    return;
                                }
                                this.reset_to_idle_on_error(msg, fetch_gen, cx);
                            });
                        });
                        return;
                    }
                };

                let mut rx_opt = None;
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.session_run_id != run_id {
                            return;
                        }
                        if this.ticket_fetch_generation == fetch_gen {
                            this.fetching_ticket = false;
                        }
                        let effective_id = fetched_ticket
                            .id
                            .clone()
                            .unwrap_or_else(|| ticket_id.clone());
                        this.ticket_search_cache
                            .insert(effective_id.clone(), fetched_ticket.clone());
                        this.active_ticket_id = Some(effective_id);
                        this.fetched_ticket = Some(fetched_ticket.clone());

                        app_state.update(cx, |state, cx| {
                            state.set_task_status(
                                ActiveView::StoryElaborator,
                                Some("Starting elaboration session...".to_string()),
                            );
                            cx.notify();
                        });

                        let rx = this.service.start_elaboration(ElaborationStartRequest {
                            ticket_data: fetched_ticket,
                            repo_path,
                            branch,
                            additional_context,
                            model_override,
                            settings,
                        });
                        rx_opt = Some(rx);
                        cx.notify();
                    });
                });

                if let Some(rx) = rx_opt {
                    Self::drive_turn_stream(view, app_state, run_id, rx, cx).await;
                }
            })
            .detach();
    }

    fn reset_to_idle_on_error(
        &mut self,
        msg: String,
        fetch_gen: u64,
        cx: &mut Context<Self>,
    ) {
        self.stage = ElaboratorStage::Idle;
        self.context_expanded = false;
        self.is_generating = false;
        self.is_waiting_for_user = false;
        self.active_ticket_id = None;
        if self.ticket_fetch_generation == fetch_gen {
            self.fetching_ticket = false;
        }
        self.validation_error = Some(msg);
        self.ticket_search
            .update(cx, |input, cx| input.set_disabled(false, cx));
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::StoryElaborator, None);
            cx.notify();
        });
        cx.notify();
    }

    fn submit_reply_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != ElaboratorStage::Elaborating
            || !self.is_waiting_for_user
            || self.is_generating
        {
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
        let Some(ticket_id) = self.active_ticket_id.clone() else {
            return;
        };

        self.validation_error = None;
        self.current_suggestions.clear();
        self.push_chat_item(ChatSender::User, trimmed.clone(), cx);
        self.dispatch_answer_turn(ticket_id, trimmed, window, cx);
    }

    fn submit_refinement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != ElaboratorStage::PlanCompleted || self.is_generating || self.is_posting {
            return;
        }
        let instructions = self.refinement_input.read(cx).value().trim().to_string();
        if instructions.is_empty() {
            return;
        }
        let Some(ticket_id) = self.active_ticket_id.clone() else {
            return;
        };

        let previous_plan = self.plan_markdown.clone();
        self.push_refined_plan(previous_plan, cx);
        self.push_chat_item(ChatSender::User, instructions.clone(), cx);

        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.is_refining = false;
        self.publish_feedback = None;
        self.validation_error = None;
        self.stage = ElaboratorStage::Elaborating;
        self.dispatch_answer_turn(ticket_id, instructions, window, cx);
    }

    fn dispatch_answer_turn(
        &mut self,
        ticket_id: String,
        prompt_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_generating = true;
        self.is_waiting_for_user = false;
        self.last_retry_turn = Some(RetryableTurn::Answer(prompt_text.clone()));
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::StoryElaborator,
                Some("Waiting for Copilot response...".to_string()),
            );
            cx.notify();
        });
        cx.notify();

        let rx = self.service.send_answer(ticket_id, prompt_text);
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
        mut rx: mpsc::UnboundedReceiver<ElaborationStreamEvent>,
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
                        ElaborationStreamEvent::Status(text) => {
                            this.push_status_item(text.clone(), cx);
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, Some(text));
                                cx.notify();
                            });
                        }
                        ElaborationStreamEvent::Question {
                            text,
                            suggested_answers,
                        } => {
                            this.is_generating = false;
                            this.is_waiting_for_user = true;
                            this.current_suggestions = suggested_answers;
                            this.push_chat_item(ChatSender::Copilot, text, cx);
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, None);
                                cx.notify();
                            });
                            NotificationService::show_if_unfocused(
                                window,
                                "Story Elaborator Question",
                                "Copilot has a clarifying question for your ticket.",
                            );
                            cx.notify();
                        }
                        ElaborationStreamEvent::Plan { text, file_path } => {
                            this.plan_markdown = text;
                            this.plan_file_path = file_path;
                            this.plan_version = this.plan_version.wrapping_add(1);
                            this.stage = ElaboratorStage::PlanCompleted;
                            this.is_generating = false;
                            this.is_waiting_for_user = false;
                            this.is_refining = false;
                            this.current_suggestions.clear();
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, None);
                                cx.notify();
                            });
                            NotificationService::show_if_unfocused(
                                window,
                                "Story Elaborator Complete",
                                "Implementation plan is ready for review.",
                            );
                            cx.notify();
                        }
                        ElaborationStreamEvent::TurnCompleted {
                            received_question,
                            received_plan,
                            cumulative_usage,
                        } => {
                            let ctx_ref = this
                                .active_ticket_id
                                .as_deref()
                                .or(this.selected_ticket_id.as_deref())
                                .map(|id| format!("Ticket - {id}"));
                            let ai_output = if this.stage == ElaboratorStage::PlanCompleted || received_plan {
                                "Implementation Plan"
                            } else {
                                "Elaboration Session"
                            };
                            let model_id = this.selected_model.clone();
                            let existing_sid = this.active_db_session_id;
                            let updated_sid = app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, None);
                                let sid = state.upsert_tool_session_usage(
                                    existing_sid,
                                    "Story Elaborator",
                                    ctx_ref.as_deref(),
                                    Some(ai_output),
                                    "Story Elaboration Session",
                                    &model_id,
                                    &cumulative_usage,
                                    received_plan,
                                    cx,
                                );
                                cx.notify();
                                sid
                            });
                            this.active_db_session_id = updated_sid;
                            this.cumulative_usage = Some(cumulative_usage);
                            this.is_generating = false;
                            if !received_question
                                && !received_plan
                                && this.stage == ElaboratorStage::Elaborating
                            {
                                this.is_waiting_for_user = true;
                                this.validation_error = Some(
                                    "Copilot finished the turn without sending a structured question or final plan. Send a follow-up prompt below or click Reset to start over."
                                        .to_string(),
                                );
                            }
                            cx.notify();
                        }
                        ElaborationStreamEvent::TimedOut { timeout_secs } => {
                            this.is_generating = false;
                            if this.stage == ElaboratorStage::Elaborating {
                                this.is_waiting_for_user = true;
                            }
                            this.validation_error = Some(format!(
                                "Story elaboration timed out after {timeout_secs} seconds without Copilot output."
                            ));
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, None);
                                cx.notify();
                            });
                            open_timeout = Some((timeout_secs, this.last_retry_turn.clone()));
                            cx.notify();
                        }
                        ElaborationStreamEvent::Cancelled => {
                            this.is_generating = false;
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, None);
                                cx.notify();
                            });
                            cx.notify();
                        }
                        ElaborationStreamEvent::Error(msg) => {
                            this.is_generating = false;
                            if this.stage == ElaboratorStage::Elaborating {
                                let has_conversation = this.feed.iter().any(|item| {
                                    matches!(
                                        item,
                                        FeedItem::Chat { .. } | FeedItem::RefinedPlan { .. }
                                    )
                                });
                                if !has_conversation {
                                    this.stop_active_session_only(window, cx);
                                } else {
                                    this.is_waiting_for_user = true;
                                }
                            }
                            this.validation_error = Some(msg);
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::StoryElaborator, None);
                                cx.notify();
                            });
                            cx.notify();
                        }
                    }
                });

                if let Some((secs, retry_turn)) = open_timeout {
                    let view_for_retry = view.clone();
                    open_timeout_modal(
                        "Story Elaborator",
                        secs,
                        move |window, cx| {
                            view_for_retry.update(cx, |this, cx| match retry_turn.clone() {
                                Some(RetryableTurn::Start) => {
                                    let _ = this.stop_active_session_only(window, cx);
                                    this.handle_start_elaboration(window, cx);
                                }
                                Some(RetryableTurn::Answer(prompt_text)) => {
                                    if let Some(tid) = this.active_ticket_id.clone() {
                                        this.validation_error = None;
                                        this.dispatch_answer_turn(tid, prompt_text, window, cx);
                                    }
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

    fn stop_active_session_only(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.session_run_id = self.session_run_id.wrapping_add(1);
        self.ticket_fetch_generation = self.ticket_fetch_generation.wrapping_add(1);
        if let Some(tid) = self.active_ticket_id.take() {
            let service = Arc::clone(&self.service);
            runtime().spawn(async move {
                service.stop_elaboration(&tid).await;
            });
        }
        self.stage = ElaboratorStage::Idle;
        self.context_expanded = false;
        self.is_generating = false;
        self.is_waiting_for_user = false;
        self.fetching_ticket = false;
        self.ticket_search
            .update(cx, |input, cx| input.set_disabled(false, cx));
    }

    fn stop_and_reset(
        &mut self,
        cancelled_by_user: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_active_session_only(window, cx);
        self.feed.clear();
        self.current_suggestions.clear();
        self.plan_markdown.clear();
        self.plan_file_path = None;
        self.is_refining = false;
        self.is_posting = false;
        self.publish_feedback = None;
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.copied = false;
        self.validation_error = None;
        self.last_retry_turn = None;
        self.status_notice = if cancelled_by_user {
            Some("Elaboration session cancelled.".to_string())
        } else {
            None
        };
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::StoryElaborator, None);
            cx.notify();
        });
        cx.notify();
    }

    fn reset_state(&self, cx: &App) -> ResetState {
        if self.is_generating || self.is_posting {
            return ResetState::NeedsConfirmation { is_running: true };
        }
        if self.stage != ElaboratorStage::Idle
            || !self.plan_markdown.is_empty()
            || !self.feed.is_empty()
        {
            return ResetState::NeedsConfirmation { is_running: false };
        }
        let has_dirty_inputs = self.ticket_search.read(cx).has_value(cx)
            || self.selected_ticket_id.is_some()
            || self.fetched_ticket.is_some()
            || self.fetching_ticket
            || !self.repo_path_input.read(cx).value().trim().is_empty()
            || self.branch_input.read(cx).value().trim() != "develop"
            || !self
                .additional_context_input
                .read(cx)
                .value()
                .trim()
                .is_empty()
            || self.user_overrode_model
            || self.validation_error.is_some()
            || self.status_notice.is_some()
            || self.publish_feedback.is_some();
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
                    "This will cancel the active elaboration session, clean up any temporary worktree, and clear all Q&A history, plans, and form inputs."
                } else {
                    "This will end the current elaboration session and clear the Q&A history, implementation plan, and all form inputs."
                };
                open_reset_confirm_modal(
                    "Story Elaborator",
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
        self.publish_gen = self.publish_gen.wrapping_add(1);
        self.copy_generation = self.copy_generation.wrapping_add(1);
        self.stop_and_reset(false, window, cx);

        self.ticket_search
            .update(cx, |input, cx| input.clear(window, cx));
        self.ticket_search_cache.clear();
        self.selected_ticket_id = None;
        self.fetched_ticket = None;
        self.fetching_ticket = false;
        self.repo_path_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.branch_input
            .update(cx, |input, cx| input.set_value("develop", window, cx));
        self.additional_context_input
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

    fn push_status_item(&mut self, text: String, cx: &mut Context<Self>) {
        self.feed.push(FeedItem::Status { text });
        self.feed_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn push_chat_item(&mut self, sender: ChatSender, text: String, cx: &mut Context<Self>) {
        let uid = self.next_feed_uid;
        self.next_feed_uid += 1;
        self.feed.push(FeedItem::Chat { uid, sender, text });
        self.feed_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn push_refined_plan(&mut self, markdown: String, cx: &mut Context<Self>) {
        let uid = self.next_feed_uid;
        self.next_feed_uid += 1;
        self.feed.push(FeedItem::RefinedPlan {
            uid,
            markdown,
            expanded: false,
        });
        self.feed_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn toggle_feed_plan(&mut self, target_uid: usize, cx: &mut Context<Self>) {
        if let Some(FeedItem::RefinedPlan { expanded, .. }) = self
            .feed
            .iter_mut()
            .find(|item| matches!(item, FeedItem::RefinedPlan { uid, .. } if *uid == target_uid))
        {
            *expanded = !*expanded;
            cx.notify();
        }
    }

    fn copy_plan(&mut self, cx: &mut Context<Self>) {
        if self.plan_markdown.trim().is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.plan_markdown.clone()));
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

    fn post_plan_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_posting || self.is_refining || self.plan_markdown.trim().is_empty() {
            return;
        }
        let ticket_id = self
            .fetched_ticket
            .as_ref()
            .and_then(|t| t.id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.active_ticket_id.clone())
            .or_else(|| {
                self.selected_ticket_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });

        let Some(ticket_id) = ticket_id else {
            self.publish_feedback = Some(Err("No ticket ID available for commenting.".to_string()));
            cx.notify();
            return;
        };

        let settings = self.app_state.read(cx).settings.clone();
        let comment_text = self.plan_markdown.clone();

        self.is_posting = true;
        self.publish_feedback = None;
        self.publish_gen = self.publish_gen.wrapping_add(1);
        let pub_gen = self.publish_gen;
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::StoryElaborator,
                Some("Posting plan comment to ticket...".to_string()),
            );
            cx.notify();
        });
        cx.notify();

        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_issue_tracker_provider(&settings)?;
                    provider.add_comment(&ticket_id, &comment_text, false).await?;
                    Ok::<_, anyhow::Error>(format!(
                        "Implementation plan added as comment on ticket #{ticket_id}."
                    ))
                })
                .await;

                let outcome = match res {
                    Ok(Ok(msg)) => Ok(msg),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                };

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.publish_gen != pub_gen {
                            return;
                        }
                        this.is_posting = false;
                        let succeeded = outcome.is_ok();
                        let sid_opt = this.active_db_session_id;
                        this.publish_feedback = Some(outcome);
                        app_state.update(cx, |state, cx| {
                            state.set_task_status(ActiveView::StoryElaborator, None);
                            if succeeded {
                                if let Some(sid) = sid_opt {
                                    state.record_push_for_session(sid, "1 comment", cx);
                                }
                            }
                            cx.notify();
                        });
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn render_fetched_ticket_card(&self, ticket: TicketData, theme: &Theme) -> impl IntoElement {
        let id_label = ticket.id.unwrap_or_default();
        let preview = strip_html_for_preview(&ticket.description);
        let ac_preview = ticket
            .acceptance_criteria
            .as_deref()
            .map(strip_html_for_preview)
            .filter(|s| !s.is_empty());

        v_flex()
            .p_3p5()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(LucideIcon::Ticket).xsmall())
                            .child("FETCHED TICKET"),
                    )
                    .when(!id_label.is_empty(), |row| {
                        row.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .child(format!("#{id_label}")),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(ticket.title),
            )
            .child(
                div()
                    .max_h(px(140.))
                    .overflow_y_scrollbar()
                    .p_2p5()
                    .rounded(theme.radius)
                    .bg(theme.sidebar)
                    .border_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if preview.is_empty() {
                        "(Ticket description is empty)".to_string()
                    } else {
                        preview
                    }),
            )
            .when_some(ac_preview, |card, ac| {
                card.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child("Acceptance Criteria"),
                        )
                        .child(
                            div()
                                .max_h(px(90.))
                                .overflow_y_scrollbar()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.sidebar)
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(ac),
                        ),
                )
            })
    }

    fn render_left_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let settings = &self.app_state.read(cx).settings;
        let issues_configured = match settings.sources.issues.as_str() {
            "azureDevOps" => settings.connectors.is_azure_configured(),
            "github" => settings.connectors.is_github_configured(),
            _ => false,
        };
        let issue_source_label = match settings.sources.issues.as_str() {
            "azureDevOps" => "Azure DevOps",
            "github" => "GitHub",
            _ => "Unconfigured",
        };
        let worktree_enabled = settings.git_worktree_enabled;

        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);

        let app_state_for_warn = self.app_state.clone();
        let direct_ticket_id = self.selected_ticket_id.clone();

        v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .bg(theme.sidebar)
            .p_5()
            .gap_4()
            .when(!issues_configured, |col| {
                col.child(
                    v_flex()
                        .p_3()
                        .gap_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(amber.opacity(0.45))
                        .bg(amber.opacity(0.08))
                        .child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(amber)
                                .child(Icon::new(LucideIcon::TriangleAlert).small())
                                .child("Connector Setup Required"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.foreground)
                                .child("Configure Azure DevOps or GitHub and select Issues Tracker Source to search tickets and post plans."),
                        )
                        .child(
                            Button::new("elab-open-connectors")
                                .small()
                                .icon(Icon::new(LucideIcon::Plug).small())
                                .label("Open Connectors Settings")
                                .on_click(move |_, _, cx| {
                                    app_state_for_warn.update(cx, |state, cx| {
                                        state.navigate_to_settings_tab(SettingsTab::Connectors, cx);
                                        cx.notify();
                                    });
                                }),
                        ),
                )
            })
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(format!("Ticket Search ({issue_source_label})")),
                            )
                            .when_some(direct_ticket_id, |row, ticket_id| {
                                row.child(
                                    Button::new("elab-fetch-ticket-preview")
                                        .ghost()
                                        .xsmall()
                                        .disabled(self.fetching_ticket)
                                        .loading(self.fetching_ticket)
                                        .label("Fetch")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.fetch_ticket_by_id(&ticket_id, window, cx);
                                        })),
                                )
                            }),
                    )
                    .child(self.ticket_search.clone()),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Local Repository Path (Optional)"),
                    )
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
                                Button::new("elab-browse-repo")
                                    .small()
                                    .icon(Icon::new(LucideIcon::FolderOpen).small())
                                    .label("Browse")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.browse_repo_dir(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("If provided, Copilot can inspect codebase files (in an isolated Git worktree if enabled) to plan changes."),
                    ),
            )
            .when(worktree_enabled, |col| {
                col.child(
                    v_flex()
                        .gap_1p5()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child("Git Branch"),
                        )
                        .child(Input::new(&self.branch_input))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("The branch to checkout in the isolated worktree. Defaults to develop."),
                        ),
                )
            })
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Additional Context (Optional)"),
                    )
                    .child(Textarea::new(&self.additional_context_input).h(px(96.))),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Copilot Model"),
                    )
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
                    Button::new("elab-start")
                        .primary()
                        .w_full()
                        .disabled(self.selected_ticket_id.is_none())
                        .icon(Icon::new(LucideIcon::Rocket).small())
                        .label("Start Elaboration")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_start_elaboration(window, cx);
                        })),
                ),
            )
            .when_some(self.fetched_ticket.clone(), |col, ticket| {
                col.child(self.render_fetched_ticket_card(ticket, &theme))
            })
    }

    fn render_context_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let worktree_enabled = self.app_state.read(cx).settings.git_worktree_enabled;
        let ticket_id = self
            .fetched_ticket
            .as_ref()
            .and_then(|t| t.id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.active_ticket_id.clone())
            .or_else(|| self.selected_ticket_id.clone());
        let title_line = self
            .fetched_ticket
            .as_ref()
            .map(|t| t.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| match ticket_id.as_deref() {
                Some(id) if self.fetching_ticket => format!("Fetching ticket #{id}..."),
                Some(id) => format!("Ticket #{id}"),
                None => "Story Elaboration".to_string(),
            });
        let repo_raw = self.repo_path_input.read(cx).value().trim().to_string();
        let repo_name = display_repo_name(&repo_raw);
        let has_repo = !repo_raw.is_empty();
        let branch_display = {
            let raw = self.branch_input.read(cx).value().trim().to_string();
            if raw.is_empty() {
                "develop".to_string()
            } else {
                raw
            }
        };
        let additional_context = self
            .additional_context_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let is_running = self.stage == ElaboratorStage::Elaborating && self.is_generating;
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
                                    .when_some(ticket_id.clone(), |row, id| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(theme.primary)
                                                .child(format!("#{id}")),
                                        )
                                    })
                                    .child(if has_repo {
                                        render_context_pill(
                                            LucideIcon::Folder,
                                            repo_name,
                                            false,
                                            cx,
                                        )
                                        .into_any_element()
                                    } else {
                                        render_context_pill(
                                            LucideIcon::Ticket,
                                            "Ticket only",
                                            false,
                                            cx,
                                        )
                                        .into_any_element()
                                    })
                                    .when(has_repo && worktree_enabled, |row| {
                                        row.child(render_context_pill(
                                            LucideIcon::GitMerge,
                                            branch_display.clone(),
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
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(title_line),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(
                                Button::new("elab-toggle-context")
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
                                    Button::new("elab-cancel")
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
                        .when(has_repo, |col| {
                            col.child(
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
                                                    .child("Repository:"),
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
                                                        .child(branch_display),
                                                ),
                                        )
                                    }),
                            )
                        })
                        .when(!additional_context.is_empty(), |col| {
                            col.child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.muted_foreground)
                                            .child("Additional Context"),
                                    )
                                    .child(
                                        div()
                                            .max_h(px(100.))
                                            .overflow_y_scrollbar()
                                            .p_2p5()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.sidebar)
                                            .text_xs()
                                            .text_color(theme.foreground)
                                            .child(additional_context),
                                    ),
                            )
                        })
                        .child(if let Some(ticket) = self.fetched_ticket.clone() {
                            self.render_fetched_ticket_card(ticket, &theme)
                                .into_any_element()
                        } else {
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Fetching ticket details...")
                                .into_any_element()
                        }),
                )
            })
    }

    fn render_right_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);

        let stage_pill: Option<(&'static str, gpui_kit::Hsla)> = match self.stage {
            ElaboratorStage::Idle => None,
            ElaboratorStage::Elaborating if self.is_generating => {
                Some(("Thinking...", theme.primary))
            }
            ElaboratorStage::Elaborating if self.is_waiting_for_user => {
                Some(("Waiting for reply", indigo))
            }
            ElaboratorStage::Elaborating => Some(("Active session", theme.muted_foreground)),
            ElaboratorStage::PlanCompleted => Some(("Plan ready", green)),
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
            ElaboratorStage::Idle => v_flex()
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
                        .child(Icon::new(LucideIcon::Brain).large()),
                )
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Start a New Elaboration"),
                )
                .child(
                    div()
                        .max_w(px(460.))
                        .text_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Select a ticket on the left and launch the session. Copilot will ask clarifying questions one by one and draft a concise implementation plan."),
                )
                .into_any_element(),
            ElaboratorStage::Elaborating => self.render_elaborating_stage(&theme, cx),
            ElaboratorStage::PlanCompleted => self.render_plan_completed_stage(&theme, cx),
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
                            .child(Icon::new(LucideIcon::MessageSquareText).small())
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Elaboration Session"),
                            )
                            .when(
                                self.stage == ElaboratorStage::Elaborating && self.is_generating,
                                |row| row.child(Spinner::new().small().color(theme.primary)),
                            )
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
                            .when(self.stage == ElaboratorStage::PlanCompleted, |row| {
                                row.child(
                                    Button::new("elab-copy-plan")
                                        .small()
                                        .icon(
                                            Icon::new(if self.copied {
                                                LucideIcon::Check
                                            } else {
                                                LucideIcon::Copy
                                            })
                                            .small(),
                                        )
                                        .label(if self.copied { "Copied!" } else { "Copy Plan" })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.copy_plan(cx);
                                        })),
                                )
                            }),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().overflow_hidden().child(body))
    }

    fn render_elaborating_stage(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mono_font = theme.mono_font_family.clone();
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);
        let reply_has_text = !self.reply_input.read(cx).value().trim().is_empty();
        let can_reply = self.is_waiting_for_user && !self.is_generating;

        let feed_elements: Vec<AnyElement> = self
            .feed
            .iter()
            .map(|item| match item {
                FeedItem::Status { text } => h_flex()
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

                FeedItem::RefinedPlan {
                    uid,
                    markdown,
                    expanded,
                } => {
                    let uid_val = *uid;
                    let is_expanded = *expanded;
                    let md_text = markdown.clone();
                    v_flex()
                        .w_full()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.sidebar)
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .px_4()
                                .py_2p5()
                                .bg(theme.secondary.opacity(0.6))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.foreground)
                                        .child(Icon::new(LucideIcon::ClipboardList).small())
                                        .child("Generated Plan (Refined)"),
                                )
                                .child(
                                    Button::new(("elab-toggle-plan", uid_val))
                                        .ghost()
                                        .xsmall()
                                        .icon(
                                            Icon::new(if is_expanded {
                                                LucideIcon::ChevronUp
                                            } else {
                                                LucideIcon::ChevronDown
                                            })
                                            .xsmall(),
                                        )
                                        .label(if is_expanded { "Collapse" } else { "Expand" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_feed_plan(uid_val, cx);
                                        })),
                                ),
                        )
                        .when(is_expanded, |card| {
                            card.child(
                                div()
                                    .max_h(px(280.))
                                    .overflow_y_scrollbar()
                                    .p_4()
                                    .border_t_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .child(
                                        MarkdownView::new(("elab-prev-plan", uid_val), md_text)
                                            .selectable(true),
                                    ),
                            )
                        })
                        .into_any_element()
                }

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
                                    MarkdownView::new(("elab-msg", *uid), text.clone())
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
                            .id("elab-feed-scroll")
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
                                                .child(if self.fetching_ticket {
                                                    "Fetching ticket details..."
                                                } else {
                                                    "Initializing Copilot elaboration session..."
                                                }),
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
                                let label = sug.clone();
                                let value = sug.clone();
                                Button::new(("elab-suggestion", idx))
                                    .small()
                                    .disabled(self.is_generating)
                                    .label(label)
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
                                Button::new("elab-send-answer")
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

    fn render_plan_completed_stage(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
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
                        v_flex()
                            .gap_4()
                            .when_some(self.plan_file_path.clone(), |col, path| {
                                col.child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .p_3()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(green.opacity(0.4))
                                        .bg(green.opacity(0.08))
                                        .text_xs()
                                        .text_color(green)
                                        .child(Icon::new(LucideIcon::FileCheck).small())
                                        .child(format!("Plan saved to: {path}")),
                                )
                            })
                            .child(
                                div()
                                    .p_5()
                                    .rounded(theme.radius_lg)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.sidebar)
                                    .child(
                                        MarkdownView::new(
                                            ("elab-final-plan", self.plan_version),
                                            self.plan_markdown.clone(),
                                        )
                                        .selectable(true),
                                    ),
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
                                .child("What changes would you like to make to the plan?"),
                        )
                        .child(
                            Textarea::new(&self.refinement_input)
                                .h(px(88.))
                                .disabled(self.is_posting),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("elab-cancel-refine")
                                        .ghost()
                                        .small()
                                        .disabled(self.is_posting)
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
                                    Button::new("elab-submit-refine")
                                        .primary()
                                        .small()
                                        .disabled(!refinement_non_empty || self.is_posting)
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
                            .when_some(self.publish_feedback.clone(), |row, fb| {
                                let (color, icon, msg) = match fb {
                                    Ok(m) => (green, LucideIcon::Check, m),
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
                                    Button::new("elab-refine")
                                        .small()
                                        .disabled(self.is_posting)
                                        .icon(Icon::new(LucideIcon::Pencil).small())
                                        .label("Refine Plan")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.is_refining = true;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("elab-post-comment")
                                    .primary()
                                    .small()
                                    .disabled(self.is_posting || self.is_refining)
                                    .loading(self.is_posting)
                                    .icon(Icon::new(LucideIcon::MessageSquare).small())
                                    .label(if self.is_posting {
                                        "Posting..."
                                    } else {
                                        "Add as Comment on Ticket"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.post_plan_comment(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for StoryElaboratorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, border, radius, secondary, accent_fg, muted_fg, connectors_ready) = {
            let theme = cx.theme();
            let settings = &self.app_state.read(cx).settings;
            let ready = match settings.sources.issues.as_str() {
                "azureDevOps" => settings.connectors.is_azure_configured(),
                "github" => settings.connectors.is_github_configured(),
                _ => false,
            };
            (
                theme.background,
                theme.border,
                theme.radius_lg,
                theme.secondary,
                theme.accent_foreground,
                theme.muted_foreground,
                ready,
            )
        };
        let app_state_for_header = self.app_state.clone();
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
                                        Icon::new(ActiveView::StoryElaborator.icon())
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
                                            .child(ActiveView::StoryElaborator.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::StoryElaborator.subtitle()),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .when(!connectors_ready, |header| {
                                header.child(
                                    Button::new("elab-header-connectors")
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(LucideIcon::Plug).small())
                                        .label("Connectors")
                                        .on_click(move |_, _, cx| {
                                            app_state_for_header.update(cx, |state, cx| {
                                                state.navigate_to_settings_tab(
                                                    SettingsTab::Connectors,
                                                    cx,
                                                );
                                                cx.notify();
                                            });
                                        }),
                                )
                            })
                            .child(render_tool_reset_button(
                                "elab-header-reset",
                                reset_state,
                                move |window, cx| {
                                    view_for_reset.update(cx, |this, cx| {
                                        this.request_reset(window, cx);
                                    });
                                },
                            )),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(
                if self.stage == ElaboratorStage::Idle {
                    h_resizable("story-elaborator-split")
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
                                "elab-dismiss-active-error",
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

fn ticket_id_mode_for(settings: &AppSettings) -> DirectIdMode {
    if settings.sources.issues.trim() == "github" {
        DirectIdMode::NumericOrRepoIssue
    } else {
        DirectIdMode::NumericOnly
    }
}
