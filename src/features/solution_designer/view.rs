use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
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
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, hsla,
    prelude::FluentBuilder as _, px,
};
use tokio::sync::mpsc;

use crate::app_state::{ActiveView, AppState};
use crate::features::settings::SettingsTab;
use crate::features::solution_designer::service::{
    DesignSessionKind, DesignStartRequest, DesignStreamEvent, SolutionDesignerService,
};
use crate::features::solution_designer::storage::{
    AreaStatus, DesignArea, LoadedProject, README_FILE_NAME, create_project, list_projects,
    load_project, next_area_filename, open_directory_in_os, read_project_file,
    reconcile_ground_rules_areas, record_area_completed, sanitize_relative_md_file,
    set_area_status,
};
use crate::infrastructure::copilot::CopilotService;
use crate::infrastructure::notifications::NotificationService;
use crate::infrastructure::runtime::spawn_tokio;
use crate::types::{AppSettings, CopilotUsage};
use crate::ui::components::{
    MarkdownView, ModelDropdown, ModelDropdownEvent, ModelDropdownMode, ResetState,
    normalize_default_model, open_reset_confirm_modal, open_timeout_modal,
    render_active_error_banner, render_tool_reset_button,
};

const TOOL_NAME: &str = "Solution Designer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewScreen {
    Landing,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectedTarget {
    Readme,
    Area(String),
    NewAreaForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceStage {
    Viewing,
    Interviewing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveSessionContext {
    GroundRules,
    ReadmeRefine,
    AreaDesign(String),
    NewArea {
        title: String,
        suggested_file: String,
    },
    AreaRefine(String),
}

impl ActiveSessionContext {
    fn label(&self) -> String {
        match self {
            Self::GroundRules => "Ground Rules Session".to_string(),
            Self::ReadmeRefine => "Refining Overview & Ground Rules".to_string(),
            Self::AreaDesign(_) => "Area Design Session".to_string(),
            Self::NewArea { title, .. } => format!("New Area: {title}"),
            Self::AreaRefine(_) => "Refining Design Area".to_string(),
        }
    }
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
    Start(DesignStartRequest),
    Answer(String),
}

pub struct SolutionDesignerView {
    app_state: Entity<AppState>,
    service: Arc<SolutionDesignerService>,
    screen: ViewScreen,

    // Landing screen state
    project_name_input: Entity<InputState>,
    initial_context_input: Entity<TextareaState>,
    projects: Vec<LoadedProject>,
    projects_loading: bool,
    projects_error: Option<String>,
    last_synced_base_dir: Option<PathBuf>,

    // Model selection
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,

    // Workspace state
    active_project: Option<LoadedProject>,
    selected_target: SelectedTarget,
    stage: WorkspaceStage,
    active_session: Option<ActiveSessionContext>,
    just_completed_ground_rules: bool,

    // New area & per-area context inputs
    new_area_title_input: Entity<InputState>,
    new_area_context_input: Entity<TextareaState>,
    area_extra_context_input: Entity<TextareaState>,

    // Document viewing & refinement
    doc_markdown: String,
    doc_version: usize,
    doc_load_error: Option<String>,
    is_refining: bool,
    refinement_input: Entity<TextareaState>,
    copied: bool,
    copy_generation: u64,

    // Interview / Q&A stream state
    is_generating: bool,
    is_waiting_for_user: bool,
    session_run_id: u64,
    feed: Vec<FeedItem>,
    next_feed_uid: usize,
    feed_scroll_handle: ScrollHandle,
    current_suggestions: Vec<String>,
    reply_input: Entity<InputState>,
    cumulative_usage: Option<CopilotUsage>,
    active_db_session_id: Option<i64>,
    last_retry_turn: Option<RetryableTurn>,

    validation_error: Option<String>,
    status_notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SolutionDesignerView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        copilot_service: Arc<CopilotService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = Arc::new(SolutionDesignerService::new(copilot_service));
        let initial_model = normalize_default_model(&initial_settings.copilot_model);
        let initial_base_dir = initial_settings.solution_design_base_dir.clone();

        let project_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g., Payments Ledger v2")
        });
        let initial_context_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Describe the problem, goals, constraints, target scale, and key stakeholders...",
            )
        });
        let new_area_title_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g., Notification Delivery Pipeline")
        });
        let new_area_context_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Optional: specific focus, constraints, or questions to explore for this area...",
            )
        });
        let area_extra_context_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Optional: additional context or constraints to guide this area's design session...",
            )
        });
        let reply_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type your answer (or 'proceed') and press Enter...")
        });
        let refinement_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Describe what to revise or expand (e.g., 'Add a failover diagram' or 'Clarify tenant isolation')...",
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
            let (default_model, base_dir) = {
                let settings = &app_state.read(cx).settings;
                (
                    normalize_default_model(&settings.copilot_model),
                    settings.solution_design_base_dir.clone(),
                )
            };
            if !this.user_overrode_model && default_model != this.selected_model {
                this.selected_model = default_model.clone();
                this.model_dropdown.update(cx, |dd, cx| {
                    dd.set_selected_model(default_model, window, cx);
                });
            }
            if base_dir != this.last_synced_base_dir {
                this.last_synced_base_dir = base_dir.clone();
                this.refresh_projects_for_dir(base_dir, window, cx);
            }
            cx.notify();
        });

        let model_sub = cx.subscribe(&model_dropdown, |this, _, event, cx| {
            let ModelDropdownEvent::Changed(model_id) = event;
            this.selected_model = model_id.clone();
            this.user_overrode_model = true;
            cx.notify();
        });

        let project_name_sub = cx.subscribe(&project_name_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let initial_ctx_sub = cx.subscribe(&initial_context_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let new_area_title_sub = cx.subscribe(&new_area_title_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let new_area_ctx_sub = cx.subscribe(&new_area_context_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let area_extra_sub = cx.subscribe(&area_extra_context_input, |_, _, event, cx| {
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
        let refine_sub = cx.subscribe(&refinement_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        let mut view = Self {
            app_state,
            service,
            screen: ViewScreen::Landing,
            project_name_input,
            initial_context_input,
            projects: Vec::new(),
            projects_loading: false,
            projects_error: None,
            last_synced_base_dir: initial_base_dir.clone(),
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            active_project: None,
            selected_target: SelectedTarget::Readme,
            stage: WorkspaceStage::Viewing,
            active_session: None,
            just_completed_ground_rules: false,
            new_area_title_input,
            new_area_context_input,
            area_extra_context_input,
            doc_markdown: String::new(),
            doc_version: 0,
            doc_load_error: None,
            is_refining: false,
            refinement_input,
            copied: false,
            copy_generation: 0,
            is_generating: false,
            is_waiting_for_user: false,
            session_run_id: 0,
            feed: Vec::new(),
            next_feed_uid: 1,
            feed_scroll_handle: ScrollHandle::new(),
            current_suggestions: Vec::new(),
            reply_input,
            cumulative_usage: None,
            active_db_session_id: None,
            last_retry_turn: None,
            validation_error: None,
            status_notice: None,
            _subscriptions: vec![
                state_sub,
                model_sub,
                project_name_sub,
                initial_ctx_sub,
                new_area_title_sub,
                new_area_ctx_sub,
                area_extra_sub,
                reply_sub,
                refine_sub,
            ],
        };

        view.refresh_projects_for_dir(initial_base_dir, window, cx);
        view
    }

    fn set_task(&self, status: Option<String>, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::SolutionDesigner, status);
            cx.notify();
        });
    }

    fn browse_base_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Solution Design Directory".into()),
        });
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                if let Ok(Ok(Some(paths))) = receiver.await {
                    if let Some(selected) = paths.into_iter().next() {
                        let _ = cx.update(|_, cx| {
                            app_state.update(cx, |state, cx| {
                                state.settings.solution_design_base_dir = Some(selected);
                                if let Err(err) = state.save_settings() {
                                    eprintln!(
                                        "Warning: failed to save solution_design_base_dir: {err}"
                                    );
                                }
                                cx.notify();
                            });
                        });
                    }
                }
            })
            .detach();
    }

    fn refresh_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let base_dir = self
            .app_state
            .read(cx)
            .settings
            .solution_design_base_dir
            .clone();
        self.refresh_projects_for_dir(base_dir, window, cx);
    }

    fn refresh_projects_for_dir(
        &mut self,
        base_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(base_dir) = base_dir else {
            self.projects.clear();
            self.projects_loading = false;
            self.projects_error = None;
            cx.notify();
            return;
        };

        self.projects_loading = true;
        self.projects_error = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move { list_projects(&base_dir) }).await;
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.projects_loading = false;
                        match res {
                            Ok(Ok(list)) => {
                                this.projects = list;
                                this.projects_error = None;
                            }
                            Ok(Err(err)) => {
                                this.projects_error = Some(err.to_string());
                            }
                            Err(err) => {
                                this.projects_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn handle_create_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base_dir) = self
            .app_state
            .read(cx)
            .settings
            .solution_design_base_dir
            .clone()
        else {
            self.validation_error =
                Some("Please configure a Solution Design Directory first.".to_string());
            cx.notify();
            return;
        };

        let name = self.project_name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.validation_error = Some("Please enter a project name.".to_string());
            cx.notify();
            return;
        }
        let initial_context = self.initial_context_input.read(cx).value().to_string();

        match create_project(&base_dir, &name, &initial_context) {
            Ok(project) => {
                self.validation_error = None;
                self.status_notice = None;
                self.project_name_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.initial_context_input
                    .update(cx, |input, cx| input.set_value("", window, cx));

                self.active_project = Some(project.clone());
                self.screen = ViewScreen::Workspace;
                self.selected_target = SelectedTarget::Readme;
                self.just_completed_ground_rules = false;
                self.doc_markdown.clear();
                self.doc_load_error = None;
                self.refresh_projects(window, cx);
                self.start_ground_rules_session(&project, window, cx);
            }
            Err(err) => {
                self.validation_error = Some(err.to_string());
                cx.notify();
            }
        }
    }

    fn open_project(&mut self, project: LoadedProject, window: &mut Window, cx: &mut Context<Self>) {
        let project_dir = project.dir.clone();
        let reloaded = load_project(&project_dir).unwrap_or(project);
        self.active_project = Some(reloaded);
        self.screen = ViewScreen::Workspace;
        self.selected_target = SelectedTarget::Readme;
        self.stage = WorkspaceStage::Viewing;
        self.active_session = None;
        self.just_completed_ground_rules = false;
        self.is_refining = false;
        self.validation_error = None;
        self.status_notice = None;
        self.feed.clear();
        self.current_suggestions.clear();
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.area_extra_context_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.load_selected_document(cx);
        cx.notify();
    }

    fn reload_active_project_from_disk(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.active_project.as_ref() else {
            return;
        };
        match load_project(&project.dir) {
            Ok(reloaded) => {
                self.active_project = Some(reloaded);
                self.validation_error = None;
                self.load_selected_document(cx);
            }
            Err(err) => {
                self.validation_error = Some(format!("Failed to reload project: {err}"));
            }
        }
        cx.notify();
    }

    fn load_selected_document(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.active_project.as_ref() else {
            self.doc_markdown.clear();
            self.doc_load_error = None;
            return;
        };

        let rel_file = match &self.selected_target {
            SelectedTarget::Readme => Some(README_FILE_NAME.to_string()),
            SelectedTarget::Area(area_id) => project
                .metadata
                .areas
                .iter()
                .find(|a| a.id == *area_id)
                .map(|a| a.file.clone()),
            SelectedTarget::NewAreaForm => None,
        };

        let Some(file) = rel_file else {
            self.doc_markdown.clear();
            self.doc_load_error = None;
            self.doc_version = self.doc_version.wrapping_add(1);
            cx.notify();
            return;
        };

        match read_project_file(&project.dir, &file) {
            Ok(content) => {
                self.doc_markdown = content;
                self.doc_load_error = None;
            }
            Err(err) => {
                self.doc_markdown.clear();
                self.doc_load_error = Some(err.to_string());
            }
        }
        self.doc_version = self.doc_version.wrapping_add(1);
        cx.notify();
    }

    fn guard_if_interviewing(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.stage != WorkspaceStage::Interviewing {
            action(self, window, cx);
            return;
        }
        let view = cx.entity();
        let is_running = self.is_generating;
        let detail = if is_running {
            "A Copilot design turn is currently running. Switching away will cancel it and discard the current in-memory Q&A transcript (saved Markdown files on disk are kept)."
        } else {
            "You have an in-progress Q&A interview. Leaving now will stop the session and discard the current in-memory Q&A transcript (saved Markdown files on disk are kept)."
        };
        open_reset_confirm_modal(
            TOOL_NAME,
            is_running,
            detail,
            move |window, cx| {
                view.update(cx, |this, cx| {
                    this.stop_active_session_only(cx);
                    action(this, window, cx);
                });
            },
            window,
            cx,
        );
    }

    fn return_to_landing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.guard_if_interviewing(window, cx, |this, window, cx| {
            this.stop_active_session_only(cx);
            this.screen = ViewScreen::Landing;
            this.active_project = None;
            this.doc_markdown.clear();
            this.doc_load_error = None;
            this.is_refining = false;
            this.validation_error = None;
            this.status_notice = None;
            this.refresh_projects(window, cx);
            cx.notify();
        });
    }

    fn select_target(
        &mut self,
        target: SelectedTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_target == target && self.stage == WorkspaceStage::Viewing {
            return;
        }
        self.guard_if_interviewing(window, cx, move |this, window, cx| {
            if this.active_session.is_some() {
                this.stop_active_session_only(cx);
            }
            this.selected_target = target.clone();
            this.stage = WorkspaceStage::Viewing;
            this.is_refining = false;
            this.validation_error = None;
            this.status_notice = None;
            this.copied = false;
            this.area_extra_context_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            this.refinement_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            if matches!(target, SelectedTarget::NewAreaForm) {
                this.new_area_title_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                this.new_area_context_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            this.load_selected_document(cx);
        });
    }

    fn start_ground_rules_session(
        &mut self,
        project: &LoadedProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.app_state.read(cx).settings.clone();
        let req = DesignStartRequest {
            project_dir: project.dir.clone(),
            kind: DesignSessionKind::GroundRules {
                project_name: project.metadata.name.clone(),
                initial_context: project.metadata.initial_context.clone(),
            },
            model_override: self.selected_model.clone(),
            settings,
        };
        self.selected_target = SelectedTarget::Readme;
        self.just_completed_ground_rules = false;
        self.begin_session_stream(ActiveSessionContext::GroundRules, req, window, cx);
    }

    fn start_area_design_session(
        &mut self,
        area: DesignArea,
        is_redesign: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let extra_context = self.area_extra_context_input.read(cx).value().to_string();
        let settings = self.app_state.read(cx).settings.clone();
        let area_id = area.id.clone();
        let req = DesignStartRequest {
            project_dir: project.dir.clone(),
            kind: DesignSessionKind::AreaDesign {
                project,
                area,
                extra_context,
                is_redesign,
            },
            model_override: self.selected_model.clone(),
            settings,
        };
        self.selected_target = SelectedTarget::Area(area_id.clone());
        self.begin_session_stream(ActiveSessionContext::AreaDesign(area_id), req, window, cx);
    }

    fn start_new_area_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let title = self
            .new_area_title_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if title.is_empty() {
            self.validation_error = Some("Please enter a title for the new design area.".to_string());
            cx.notify();
            return;
        }
        let extra_context = self.new_area_context_input.read(cx).value().to_string();
        let suggested_file = next_area_filename(&project.metadata.areas, &title);
        let settings = self.app_state.read(cx).settings.clone();

        let req = DesignStartRequest {
            project_dir: project.dir.clone(),
            kind: DesignSessionKind::NewArea {
                project,
                title: title.clone(),
                suggested_file: suggested_file.clone(),
                extra_context,
            },
            model_override: self.selected_model.clone(),
            settings,
        };
        self.begin_session_stream(
            ActiveSessionContext::NewArea {
                title,
                suggested_file,
            },
            req,
            window,
            cx,
        );
    }

    fn begin_session_stream(
        &mut self,
        ctx: ActiveSessionContext,
        req: DesignStartRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.validation_error = None;
        self.status_notice = None;
        self.is_refining = false;
        self.feed.clear();
        self.current_suggestions.clear();
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.copied = false;
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        self.active_session = Some(ctx);
        self.stage = WorkspaceStage::Interviewing;
        self.is_generating = true;
        self.is_waiting_for_user = false;
        self.last_retry_turn = Some(RetryableTurn::Start(req.clone()));
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;

        self.set_task(Some("Starting solution design session...".to_string()), cx);
        cx.notify();

        let rx = self.service.start(req);
        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                Self::drive_turn_stream(view, app_state, run_id, rx, cx).await;
            })
            .detach();
    }

    fn submit_reply_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != WorkspaceStage::Interviewing
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
        self.validation_error = None;
        self.current_suggestions.clear();
        self.push_chat_item(ChatSender::User, trimmed.clone(), cx);
        self.dispatch_answer_turn(trimmed, window, cx);
    }

    fn submit_refinement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != WorkspaceStage::Viewing || self.is_generating {
            return;
        }
        let instructions = self.refinement_input.read(cx).value().trim().to_string();
        if instructions.is_empty() {
            return;
        }
        let Some(project) = self.active_project.clone() else {
            return;
        };

        let can_reuse_live = match (&self.selected_target, &self.active_session) {
            (SelectedTarget::Readme, Some(ActiveSessionContext::GroundRules | ActiveSessionContext::ReadmeRefine)) => true,
            (SelectedTarget::Area(id), Some(ActiveSessionContext::AreaDesign(sid) | ActiveSessionContext::AreaRefine(sid))) if id == sid => true,
            _ => false,
        };

        self.refinement_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.is_refining = false;
        self.validation_error = None;

        if can_reuse_live {
            self.push_chat_item(ChatSender::User, instructions.clone(), cx);
            self.stage = WorkspaceStage::Interviewing;
            let target_file = match &self.selected_target {
                SelectedTarget::Readme => README_FILE_NAME.to_string(),
                SelectedTarget::Area(id) => project
                    .metadata
                    .areas
                    .iter()
                    .find(|a| a.id == *id)
                    .map(|a| a.file.clone())
                    .unwrap_or_else(|| README_FILE_NAME.to_string()),
                SelectedTarget::NewAreaForm => README_FILE_NAME.to_string(),
            };
            let prompt = format!(
                "Refine `{target_file}` according to these instructions (ask clarifying questions one at a time only if genuinely ambiguous, otherwise edit `{target_file}` on disk and emit a \"complete\" JSONL message):\n{instructions}"
            );
            self.dispatch_answer_turn(prompt, window, cx);
        } else {
            let (target_title, target_file, is_readme, ctx) = match &self.selected_target {
                SelectedTarget::Readme => (
                    "Overview & Ground Rules".to_string(),
                    README_FILE_NAME.to_string(),
                    true,
                    ActiveSessionContext::ReadmeRefine,
                ),
                SelectedTarget::Area(area_id) => {
                    let Some(area) = project
                        .metadata
                        .areas
                        .iter()
                        .find(|a| a.id == *area_id)
                        .cloned()
                    else {
                        return;
                    };
                    (
                        area.title,
                        area.file,
                        false,
                        ActiveSessionContext::AreaRefine(area_id.clone()),
                    )
                }
                SelectedTarget::NewAreaForm => return,
            };

            let settings = self.app_state.read(cx).settings.clone();
            let req = DesignStartRequest {
                project_dir: project.dir.clone(),
                kind: DesignSessionKind::StandaloneRefine {
                    project,
                    target_title,
                    target_file,
                    is_readme,
                    instructions,
                },
                model_override: self.selected_model.clone(),
                settings,
            };
            self.begin_session_stream(ctx, req, window, cx);
        }
    }

    fn dispatch_answer_turn(
        &mut self,
        prompt_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        mut rx: mpsc::UnboundedReceiver<DesignStreamEvent>,
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
                        DesignStreamEvent::Status(text) => {
                            this.feed.push(FeedItem::Status(text.clone()));
                            this.feed_scroll_handle.scroll_to_bottom();
                            this.set_task(Some(text), cx);
                            cx.notify();
                        }
                        DesignStreamEvent::Question {
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
                                "Solution Designer Question",
                                "Copilot has a design question for you.",
                            );
                            cx.notify();
                        }
                        DesignStreamEvent::Complete {
                            file,
                            summary,
                            areas,
                        } => {
                            this.handle_session_complete(file, summary, areas, window, cx);
                        }
                        DesignStreamEvent::TurnCompleted {
                            received_question,
                            received_complete,
                            cumulative_usage,
                        } => {
                            let ctx_ref = this
                                .active_project
                                .as_ref()
                                .map(|p| {
                                    let session_lbl = this
                                        .active_session
                                        .as_ref()
                                        .map(ActiveSessionContext::label)
                                        .unwrap_or_else(|| "Design".to_string());
                                    format!("{} - {}", p.metadata.name, session_lbl)
                                })
                                .unwrap_or_else(|| "Solution Design".to_string());

                            let ai_output = if received_complete {
                                "Design Doc"
                            } else {
                                "Design Session"
                            };
                            let model_id = this.selected_model.clone();
                            let existing_sid = this.active_db_session_id;
                            let updated_sid = app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::SolutionDesigner, None);
                                let sid = state.upsert_tool_session_usage(
                                    existing_sid,
                                    TOOL_NAME,
                                    Some(&ctx_ref),
                                    Some(ai_output),
                                    "Solution Designer Session",
                                    &model_id,
                                    &cumulative_usage,
                                    received_complete,
                                    cx,
                                );
                                cx.notify();
                                sid
                            });
                            this.active_db_session_id = updated_sid;
                            this.cumulative_usage = Some(cumulative_usage);
                            this.is_generating = false;
                            if !received_question
                                && !received_complete
                                && this.stage == WorkspaceStage::Interviewing
                            {
                                this.is_waiting_for_user = true;
                                this.validation_error = Some(
                                    "Copilot finished the turn without sending a structured question or completion signal. Send a follow-up prompt below or click Cancel."
                                        .to_string(),
                                );
                            }
                            cx.notify();
                        }
                        DesignStreamEvent::TimedOut { timeout_secs } => {
                            this.is_generating = false;
                            if this.stage == WorkspaceStage::Interviewing {
                                this.is_waiting_for_user = true;
                            }
                            this.validation_error = Some(format!(
                                "Solution design session timed out after {timeout_secs} seconds without Copilot output."
                            ));
                            this.set_task(None, cx);
                            open_timeout = Some((timeout_secs, this.last_retry_turn.clone()));
                            cx.notify();
                        }
                        DesignStreamEvent::Cancelled => {
                            this.is_generating = false;
                            this.set_task(None, cx);
                            cx.notify();
                        }
                        DesignStreamEvent::Error(msg) => {
                            this.is_generating = false;
                            if this.stage == WorkspaceStage::Interviewing {
                                this.is_waiting_for_user = true;
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
                                Some(RetryableTurn::Start(req)) => {
                                    let ctx_kind = this
                                        .active_session
                                        .clone()
                                        .unwrap_or(ActiveSessionContext::GroundRules);
                                    this.stop_active_session_only(cx);
                                    this.begin_session_stream(ctx_kind, req, window, cx);
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

    fn handle_session_complete(
        &mut self,
        file: Option<String>,
        summary: Option<String>,
        areas: Vec<crate::features::solution_designer::storage::ReportedArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut project) = self.active_project.take() else {
            return;
        };

        let ctx = self.active_session.clone();
        match ctx {
            Some(ActiveSessionContext::GroundRules) => {
                if let Err(err) = reconcile_ground_rules_areas(&mut project, &areas) {
                    self.validation_error = Some(format!("Failed to update .stitch.json: {err}"));
                }
                self.selected_target = SelectedTarget::Readme;
                self.just_completed_ground_rules = true;
            }
            Some(ActiveSessionContext::ReadmeRefine) => {
                if !areas.is_empty() || !project.metadata.ground_rules_completed {
                    if let Err(err) = reconcile_ground_rules_areas(&mut project, &areas) {
                        self.validation_error =
                            Some(format!("Failed to update .stitch.json: {err}"));
                    }
                }
                self.selected_target = SelectedTarget::Readme;
            }
            Some(ActiveSessionContext::AreaDesign(area_id)) => {
                let fallback_title = project
                    .metadata
                    .areas
                    .iter()
                    .find(|a| a.id == area_id)
                    .map(|a| a.title.clone())
                    .unwrap_or_else(|| "Design Area".to_string());
                match record_area_completed(
                    &mut project,
                    Some(&area_id),
                    &fallback_title,
                    file.as_deref(),
                    summary.as_deref(),
                    false,
                ) {
                    Ok(updated) => {
                        self.selected_target = SelectedTarget::Area(updated.id);
                    }
                    Err(err) => {
                        self.validation_error =
                            Some(format!("Failed to update .stitch.json: {err}"));
                    }
                }
            }
            Some(ActiveSessionContext::NewArea {
                title,
                suggested_file,
            }) => {
                let effective_file = file
                    .as_deref()
                    .map(|f| sanitize_relative_md_file(f, &title))
                    .unwrap_or(suggested_file);
                match record_area_completed(
                    &mut project,
                    None,
                    &title,
                    Some(&effective_file),
                    summary.as_deref(),
                    false,
                ) {
                    Ok(created) => {
                        self.selected_target = SelectedTarget::Area(created.id);
                    }
                    Err(err) => {
                        self.validation_error =
                            Some(format!("Failed to update .stitch.json: {err}"));
                    }
                }
            }
            Some(ActiveSessionContext::AreaRefine(area_id)) => {
                let fallback_title = project
                    .metadata
                    .areas
                    .iter()
                    .find(|a| a.id == area_id)
                    .map(|a| a.title.clone())
                    .unwrap_or_else(|| "Design Area".to_string());
                match record_area_completed(
                    &mut project,
                    Some(&area_id),
                    &fallback_title,
                    file.as_deref(),
                    summary.as_deref(),
                    true,
                ) {
                    Ok(updated) => {
                        self.selected_target = SelectedTarget::Area(updated.id);
                    }
                    Err(err) => {
                        self.validation_error =
                            Some(format!("Failed to update .stitch.json: {err}"));
                    }
                }
            }
            None => {}
        }

        self.active_project = Some(project);
        self.stage = WorkspaceStage::Viewing;
        self.is_generating = false;
        self.is_waiting_for_user = false;
        self.is_refining = false;
        self.current_suggestions.clear();
        self.set_task(None, cx);
        self.load_selected_document(cx);
        self.refresh_projects(window, cx);

        NotificationService::show_if_unfocused(
            window,
            "Solution Design Complete",
            summary
                .as_deref()
                .unwrap_or("Markdown documentation has been written to disk."),
        );
        cx.notify();
    }

    fn toggle_area_finalized(&mut self, area_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.active_project.as_mut() else {
            return;
        };
        let Some(current) = project
            .metadata
            .areas
            .iter()
            .find(|a| a.id == area_id)
            .map(|a| a.status)
        else {
            return;
        };
        let next = match current {
            AreaStatus::Finalized => AreaStatus::Drafted,
            AreaStatus::Planned | AreaStatus::Drafted => AreaStatus::Finalized,
        };
        if let Err(err) = set_area_status(project, area_id, next) {
            self.validation_error = Some(err.to_string());
        } else {
            self.validation_error = None;
        }
        cx.notify();
    }

    fn stop_active_session_only(&mut self, cx: &mut Context<Self>) {
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let service = Arc::clone(&self.service);
        crate::infrastructure::runtime::runtime().spawn(async move {
            service.stop().await;
        });
        self.active_session = None;
        self.stage = WorkspaceStage::Viewing;
        self.is_generating = false;
        self.is_waiting_for_user = false;
        self.set_task(None, cx);
        cx.notify();
    }

    fn cancel_interview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_active_session_only(cx);
        self.feed.clear();
        self.current_suggestions.clear();
        self.status_notice = Some("Design session cancelled.".to_string());
        self.reply_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.load_selected_document(cx);
        cx.notify();
    }

    fn reset_state(&self) -> ResetState {
        if self.stage != WorkspaceStage::Interviewing {
            return ResetState::Clean;
        }
        ResetState::NeedsConfirmation {
            is_running: self.is_generating,
        }
    }

    fn request_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let ResetState::NeedsConfirmation { is_running } = self.reset_state() {
            let view = cx.entity();
            let detail = if is_running {
                "This will cancel the active Copilot design turn and stop the current interview."
            } else {
                "This will stop the current interview and discard the in-memory Q&A transcript."
            };
            open_reset_confirm_modal(
                TOOL_NAME,
                is_running,
                detail,
                move |window, cx| {
                    view.update(cx, |this, cx| {
                        this.cancel_interview(window, cx);
                    });
                },
                window,
                cx,
            );
        }
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

    fn render_unconfigured_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let app_state = self.app_state.clone();

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                v_flex()
                    .max_w(px(520.))
                    .p_6()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(52.))
                            .rounded(theme.radius_lg)
                            .bg(theme.secondary)
                            .text_color(theme.primary)
                            .child(Icon::new(LucideIcon::Layers).large()),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Set a Solution Design Directory"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child("Choose a base folder on disk where your solution design projects, Markdown documents, and .stitch.json metadata will live."),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .pt_1()
                            .child(
                                Button::new("sd-setup-browse")
                                    .primary()
                                    .icon(Icon::new(LucideIcon::FolderOpen).small())
                                    .label("Choose Directory...")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.browse_base_dir(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("sd-setup-settings")
                                    .ghost()
                                    .icon(Icon::new(LucideIcon::Settings).small())
                                    .label("Open Settings")
                                    .on_click(move |_, _, cx| {
                                        app_state.update(cx, |state, cx| {
                                            state.navigate_to_settings_tab(SettingsTab::General, cx);
                                            cx.notify();
                                        });
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_landing_screen(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(base_dir) = self
            .app_state
            .read(cx)
            .settings
            .solution_design_base_dir
            .clone()
        else {
            return self.render_unconfigured_banner(cx);
        };

        let theme = cx.theme().clone();
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let can_start = !self.project_name_input.read(cx).value().trim().is_empty();

        let left_pane = v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .bg(theme.sidebar)
            .p_5()
            .gap_4()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Start a New Project"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Create a project folder and run a Ground Rules interview to define scope, constraints, and design areas."),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Project Name"),
                    )
                    .child(Input::new(&self.project_name_input)),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Initial Working Context"),
                    )
                    .child(Textarea::new(&self.initial_context_input).h(px(180.)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Share what you're building, key constraints, non-goals, or rough architectural thoughts."),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
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
            .child(
                div().pt_1().child(
                    Button::new("sd-start-ground-rules")
                        .primary()
                        .w_full()
                        .disabled(!can_start)
                        .icon(Icon::new(LucideIcon::Rocket).small())
                        .label("Start Ground Rules")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_create_project(window, cx);
                        })),
                ),
            );

        let base_dir_str = base_dir.to_string_lossy().to_string();
        let right_pane = v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_6()
                    .py_3p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Continue Existing Project"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(base_dir_str),
                            ),
                    )
                    .child(
                        Button::new("sd-refresh-projects")
                            .ghost()
                            .small()
                            .disabled(self.projects_loading)
                            .loading(self.projects_loading)
                            .icon(Icon::new(LucideIcon::RefreshCw).small())
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh_projects(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .p_6()
                    .child(self.render_projects_list(&theme, green, red, cx)),
            );

        h_resizable("solution-designer-landing-split")
            .child(
                resizable_panel()
                    .size(px(390.))
                    .size_range(px(320.)..px(540.))
                    .child(left_pane),
            )
            .child(resizable_panel().child(right_pane))
            .into_any_element()
    }

    fn render_projects_list(
        &self,
        theme: &Theme,
        green: gpui_kit::Hsla,
        red: gpui_kit::Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.projects_loading {
            return v_flex()
                .py_12()
                .items_center()
                .justify_center()
                .gap_3()
                .child(Spinner::new().large().color(theme.primary))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Scanning solution design projects..."),
                )
                .into_any_element();
        }

        if let Some(err) = &self.projects_error {
            return h_flex()
                .p_4()
                .gap_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(red.opacity(0.4))
                .bg(red.opacity(0.08))
                .text_xs()
                .text_color(red)
                .child(Icon::new(LucideIcon::CircleAlert).small())
                .child(div().flex_1().child(err.clone()))
                .into_any_element();
        }

        if self.projects.is_empty() {
            return v_flex()
                .py_12()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(LucideIcon::Folder).large().text_color(theme.muted_foreground))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("No Solution Design Projects Yet"),
                )
                .child(
                    div()
                        .max_w(px(380.))
                        .text_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Create your first project on the left to kick off a Ground Rules interview."),
                )
                .into_any_element();
        }

        v_flex()
            .gap_3()
            .children(self.projects.iter().enumerate().map(|(idx, project)| {
                let total = project.metadata.areas.len();
                let drafted = project
                    .metadata
                    .areas
                    .iter()
                    .filter(|a| matches!(a.status, AreaStatus::Drafted | AreaStatus::Finalized))
                    .count();
                let finalized = project
                    .metadata
                    .areas
                    .iter()
                    .filter(|a| a.status == AreaStatus::Finalized)
                    .count();

                let progress_label = if !project.metadata.ground_rules_completed {
                    "Ground Rules Pending".to_string()
                } else if total == 0 {
                    "Ground Rules Complete".to_string()
                } else if finalized > 0 {
                    format!("{drafted}/{total} drafted • {finalized} finalized")
                } else {
                    format!("{drafted}/{total} areas drafted")
                };

                let badge_color = if !project.metadata.ground_rules_completed {
                    theme.muted_foreground
                } else if total > 0 && finalized == total {
                    green
                } else {
                    theme.primary
                };

                let updated_label = format_rfc3339_short(&project.metadata.updated_at);
                let proj_clone = project.clone();

                h_flex()
                    .id(("sd-project-card", idx))
                    .w_full()
                    .p_4()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.secondary_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_project(proj_clone.clone(), window, cx);
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Icon::new(LucideIcon::Folder).small().text_color(theme.primary))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(project.metadata.name.clone()),
                                    ),
                            )
                            .when(!project.metadata.initial_context.trim().is_empty(), |col| {
                                col.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(project.metadata.initial_context.clone()),
                                )
                            }),
                    )
                    .child(
                        v_flex()
                            .items_end()
                            .gap_1()
                            .flex_shrink_0()
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(badge_color.opacity(0.4))
                                    .bg(badge_color.opacity(0.1))
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(badge_color)
                                    .child(progress_label),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(LucideIcon::Clock).xsmall())
                                    .child(updated_label),
                            ),
                    )
            }))
            .into_any_element()
    }

    fn render_workspace_screen(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(project) = self.active_project.as_ref() else {
            return self.render_landing_screen(cx);
        };

        let theme = cx.theme().clone();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);

        let left_sidebar = v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                v_flex()
                    .p_3()
                    .gap_1p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("sd-back-to-projects")
                            .ghost()
                            .small()
                            .icon(Icon::new(LucideIcon::Undo2).small())
                            .label("All Projects")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.return_to_landing(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .px_2()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(project.metadata.name.clone()),
                    )
                    .child(
                        h_flex()
                            .px_2()
                            .items_center()
                            .gap_1p5()
                            .child(
                                Button::new("sd-open-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::FolderOpen).xsmall())
                                    .label("Open Folder")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(proj) = this.active_project.as_ref() {
                                            if let Err(err) = open_directory_in_os(&proj.dir) {
                                                this.validation_error = Some(err.to_string());
                                                cx.notify();
                                            }
                                        }
                                    })),
                            )
                            .child(
                                Button::new("sd-reload-project")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::RefreshCw).xsmall())
                                    .label("Reload")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_active_project_from_disk(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p_3()
                    .gap_3()
                    .child({
                        let is_selected = self.selected_target == SelectedTarget::Readme;
                        let (badge_lbl, badge_col) = if project.metadata.ground_rules_completed {
                            ("Ready", green)
                        } else {
                            ("Pending", theme.muted_foreground)
                        };
                        h_flex()
                            .id("sd-nav-readme")
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .px_2p5()
                            .py_2()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .bg(if is_selected {
                                theme.secondary
                            } else {
                                theme.transparent
                            })
                            .hover(|s| s.bg(theme.secondary_hover))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_target(SelectedTarget::Readme, window, cx);
                            }))
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap_2()
                                    .child(Icon::new(LucideIcon::BookOpen).small().text_color(theme.primary))
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child("Overview & Ground Rules"),
                                    ),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(4.))
                                    .bg(badge_col.opacity(0.12))
                                    .text_xs()
                                    .text_color(badge_col)
                                    .child(badge_lbl),
                            )
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .px_2()
                            .pt_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("DESIGN AREAS"),
                            )
                            .child(
                                Button::new("sd-new-area-btn")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::Plus).xsmall())
                                    .label("New Area")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.select_target(SelectedTarget::NewAreaForm, window, cx);
                                    })),
                            ),
                    )
                    .when(project.metadata.areas.is_empty(), |col| {
                        col.child(
                            div()
                                .px_2p5()
                                .py_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("No areas yet. Complete Ground Rules or click + New Area."),
                        )
                    })
                    .children(project.metadata.areas.iter().enumerate().map(|(idx, area)| {
                        let is_selected =
                            self.selected_target == SelectedTarget::Area(area.id.clone());
                        let area_id = area.id.clone();
                        let (status_color, icon) = match area.status {
                            AreaStatus::Planned => (theme.muted_foreground, LucideIcon::FileText),
                            AreaStatus::Drafted => (indigo, LucideIcon::FileCheck),
                            AreaStatus::Finalized => (green, LucideIcon::CircleCheck),
                        };
                        h_flex()
                            .id(("sd-area-nav", idx))
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .px_2p5()
                            .py_2()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .bg(if is_selected {
                                theme.secondary
                            } else {
                                theme.transparent
                            })
                            .hover(|s| s.bg(theme.secondary_hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_target(
                                    SelectedTarget::Area(area_id.clone()),
                                    window,
                                    cx,
                                );
                            }))
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_center()
                                    .gap_2()
                                    .child(Icon::new(icon).small().text_color(status_color))
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .truncate()
                                                    .child(area.title.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .text_color(theme.muted_foreground)
                                                    .truncate()
                                                    .child(area.file.clone()),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(4.))
                                    .bg(status_color.opacity(0.12))
                                    .text_xs()
                                    .text_color(status_color)
                                    .child(area.status.label()),
                            )
                    })),
            )
            .child(
                v_flex()
                    .p_3()
                    .gap_1p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Copilot Model"),
                    )
                    .child(self.model_dropdown.clone()),
            );

        let right_content: AnyElement = match self.stage {
            WorkspaceStage::Interviewing => self.render_interview_pane(&theme, cx),
            WorkspaceStage::Viewing => self.render_viewing_pane(project, &theme, cx),
        };

        h_resizable("solution-designer-workspace-split")
            .child(
                resizable_panel()
                    .size(px(300.))
                    .size_range(px(250.)..px(440.))
                    .child(left_sidebar),
            )
            .child(resizable_panel().child(right_content))
            .into_any_element()
    }

    fn render_interview_pane(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mono_font = theme.mono_font_family.clone();
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);
        let reply_has_text = !self.reply_input.read(cx).value().trim().is_empty();
        let can_reply = self.is_waiting_for_user && !self.is_generating;

        let title = self
            .active_session
            .as_ref()
            .map(ActiveSessionContext::label)
            .unwrap_or_else(|| "Design Session".to_string());

        let (pill_label, pill_color) = if self.is_generating {
            ("Working...", theme.primary)
        } else if self.is_waiting_for_user {
            ("Waiting for reply", indigo)
        } else {
            ("Active", theme.muted_foreground)
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
                                    MarkdownView::new(("sd-msg", *uid), text.clone())
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

        let view_for_dismiss = cx.entity();

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
                                    .child(title),
                            )
                            .when(self.is_generating, |row| {
                                row.child(Spinner::new().small().color(theme.primary))
                            })
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(pill_color.opacity(0.4))
                                    .bg(pill_color.opacity(0.1))
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(pill_color)
                                    .child(pill_label),
                            ),
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
                            .child(
                                Button::new("sd-cancel-interview")
                                    .danger()
                                    .small()
                                    .icon(Icon::new(LucideIcon::Square).small())
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_interview(window, cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.validation_error.clone(), |col, err| {
                col.child(render_active_error_banner(
                    "sd-dismiss-interview-error",
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
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .id("sd-feed-scroll")
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
                                                .child("Starting Solution Designer session..."),
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
                                Button::new(("sd-suggestion", idx))
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
                                Button::new("sd-proceed")
                                    .small()
                                    .disabled(!can_reply)
                                    .label("Proceed")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_answer(
                                            "Proceed with your recommended options, write the markdown file(s) to disk, and emit the complete signal."
                                                .to_string(),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("sd-send-answer")
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

    fn render_viewing_pane(
        &self,
        project: &LoadedProject,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &self.selected_target {
            SelectedTarget::NewAreaForm => self.render_new_area_form(theme, cx),
            SelectedTarget::Readme => self.render_readme_view(project, theme, cx),
            SelectedTarget::Area(area_id) => {
                if let Some(area) = project.metadata.areas.iter().find(|a| a.id == *area_id) {
                    self.render_area_view(area, theme, cx)
                } else {
                    self.render_readme_view(project, theme, cx)
                }
            }
        }
    }

    fn render_new_area_form(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let can_start = !self.new_area_title_input.read(cx).value().trim().is_empty();
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_6()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(LucideIcon::FilePlus).small().text_color(theme.primary))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("New Design Area"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p_6()
                    .child(
                        v_flex()
                            .max_w(px(640.))
                            .p_5()
                            .gap_4()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Area Title"),
                                    )
                                    .child(Input::new(&self.new_area_title_input)),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Working Context (Optional)"),
                                    )
                                    .child(Textarea::new(&self.new_area_context_input).h(px(140.)))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Copilot will read README.md and sibling docs, interview you with /grill-me questions, then write the new area document."),
                                    ),
                            )
                            .when_some(self.validation_error.clone(), |col, err| {
                                col.child(
                                    h_flex()
                                        .p_3()
                                        .gap_2()
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
                            .child(
                                h_flex().justify_end().child(
                                    Button::new("sd-start-new-area")
                                        .primary()
                                        .disabled(!can_start)
                                        .icon(Icon::new(LucideIcon::Rocket).small())
                                        .label("Start Area Design Session")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.start_new_area_session(window, cx);
                                        })),
                                ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_readme_view(
        &self,
        project: &LoadedProject,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let project_clone = project.clone();
        let first_planned_id = project
            .metadata
            .areas
            .iter()
            .find(|a| a.status == AreaStatus::Planned)
            .or_else(|| project.metadata.areas.first())
            .map(|a| a.id.clone());

        if !project.metadata.ground_rules_completed && self.doc_markdown.trim().is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .p_8()
                .gap_3()
                .child(Icon::new(LucideIcon::BookOpen).large().text_color(theme.primary))
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Ground Rules Not Started"),
                )
                .child(
                    div()
                        .max_w(px(440.))
                        .text_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Run the Ground Rules interview to establish shared constraints and scaffold your project's design areas."),
                )
                .child(
                    Button::new("sd-run-ground-rules")
                        .primary()
                        .icon(Icon::new(LucideIcon::Play).small())
                        .label("Start Ground Rules Interview")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_ground_rules_session(&project_clone, window, cx);
                        })),
                )
                .into_any_element();
        }

        let refine_non_empty = !self.refinement_input.read(cx).value().trim().is_empty();
        let has_doc = !self.doc_markdown.trim().is_empty();

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(self.render_doc_header("Overview & Ground Rules", README_FILE_NAME, has_doc, theme, cx))
            .child(self.render_doc_body(theme, cx))
            .when(self.is_refining, |pane| {
                pane.child(self.render_refinement_box(refine_non_empty, theme, cx))
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
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("sd-readme-reload")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label("Reload from Disk")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_active_project_from_disk(cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .when(!self.is_refining, |row| {
                                row.child(
                                    Button::new("sd-readme-refine")
                                        .small()
                                        .icon(Icon::new(LucideIcon::Pencil).small())
                                        .label("Refine")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.is_refining = true;
                                            cx.notify();
                                        })),
                                )
                            })
                            .when(self.just_completed_ground_rules, |row| {
                                let target_id = first_planned_id.clone();
                                row.child(
                                    Button::new("sd-continue-to-areas")
                                        .primary()
                                        .small()
                                        .icon(Icon::new(LucideIcon::ArrowRight).small())
                                        .label("Continue to Project Areas")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.just_completed_ground_rules = false;
                                            if let Some(id) = target_id.clone() {
                                                this.select_target(
                                                    SelectedTarget::Area(id),
                                                    window,
                                                    cx,
                                                );
                                            } else {
                                                this.select_target(
                                                    SelectedTarget::NewAreaForm,
                                                    window,
                                                    cx,
                                                );
                                            }
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_area_view(
        &self,
        area: &DesignArea,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let area_clone = area.clone();
        let area_id_for_toggle = area.id.clone();
        let has_doc = !self.doc_markdown.trim().is_empty();
        let refine_non_empty = !self.refinement_input.read(cx).value().trim().is_empty();

        if area.status == AreaStatus::Planned {
            let area_for_start = area.clone();
            return v_flex()
                .size_full()
                .bg(theme.background)
                .child(self.render_doc_header(&area.title, &area.file, has_doc, theme, cx))
                .child(self.render_doc_body(theme, cx))
                .child(
                    v_flex()
                        .gap_3()
                        .px_6()
                        .py_4()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.sidebar)
                        .child(
                            v_flex()
                                .gap_1p5()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Additional Context for This Area (Optional)"),
                                )
                                .child(Textarea::new(&self.area_extra_context_input).h(px(76.))),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    Button::new("sd-planned-reload")
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(LucideIcon::RefreshCw).small())
                                        .label("Reload from Disk")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.reload_active_project_from_disk(cx);
                                        })),
                                )
                                .child(
                                    Button::new("sd-start-area-design")
                                        .primary()
                                        .icon(Icon::new(LucideIcon::Rocket).small())
                                        .label("Start Design Session")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.start_area_design_session(
                                                area_for_start.clone(),
                                                false,
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                        ),
                )
                .into_any_element();
        }

        let is_finalized = area.status == AreaStatus::Finalized;
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(self.render_doc_header(&area.title, &area.file, has_doc, theme, cx))
            .child(self.render_doc_body(theme, cx))
            .when(self.is_refining, |pane| {
                pane.child(self.render_refinement_box(refine_non_empty, theme, cx))
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
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("sd-area-reload")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label("Reload from Disk")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_active_project_from_disk(cx);
                                    })),
                            )
                            .child(
                                Button::new("sd-area-toggle-finalized")
                                    .small()
                                    .icon(
                                        Icon::new(if is_finalized {
                                            LucideIcon::RotateCcw
                                        } else {
                                            LucideIcon::CircleCheck
                                        })
                                        .small(),
                                    )
                                    .label(if is_finalized {
                                        "Reopen as Draft"
                                    } else {
                                        "Mark Finalized"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_area_finalized(&area_id_for_toggle, cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("sd-area-redesign")
                                    .small()
                                    .disabled(self.is_refining)
                                    .icon(Icon::new(LucideIcon::MessagesSquare).small())
                                    .label("Re-design (Q&A)")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.start_area_design_session(
                                            area_clone.clone(),
                                            true,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .when(!self.is_refining, |row| {
                                row.child(
                                    Button::new("sd-area-refine")
                                        .primary()
                                        .small()
                                        .icon(Icon::new(LucideIcon::Pencil).small())
                                        .label("Refine")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.is_refining = true;
                                            cx.notify();
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_doc_header(
        &self,
        title: &str,
        file: &str,
        has_doc: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .items_center()
            .justify_between()
            .gap_3()
            .px_6()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(LucideIcon::FileText).small().text_color(theme.primary))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title.to_string()),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.))
                            .bg(theme.secondary)
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(file.to_string()),
                    ),
            )
            .when(has_doc, |row| {
                row.child(
                    Button::new("sd-copy-doc")
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
            })
    }

    fn render_doc_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let view_for_dismiss = cx.entity();

        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scrollbar()
            .p_6()
            .child(
                v_flex()
                    .gap_4()
                    .when_some(self.validation_error.clone(), |col, err| {
                        col.child(render_active_error_banner(
                            "sd-dismiss-view-error",
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
                    .child(if !self.doc_markdown.trim().is_empty() {
                        div()
                            .p_5()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                MarkdownView::new(
                                    ("sd-doc", self.doc_version),
                                    self.doc_markdown.clone(),
                                )
                                .selectable(true),
                            )
                            .into_any_element()
                    } else if let Some(err) = &self.doc_load_error {
                        h_flex()
                            .p_4()
                            .gap_2()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(red.opacity(0.35))
                            .bg(red.opacity(0.08))
                            .text_xs()
                            .text_color(red)
                            .child(Icon::new(LucideIcon::CircleAlert).small())
                            .child(div().flex_1().child(format!(
                                "Document not found on disk yet ({err}). Start the design session or click Reload from Disk."
                            )))
                            .into_any_element()
                    } else {
                        div()
                            .p_5()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("No content written for this document yet.")
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn render_refinement_box(
        &self,
        refine_non_empty: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                    .child("What should be revised or expanded?"),
            )
            .child(Textarea::new(&self.refinement_input).h(px(88.)))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("sd-cancel-refine")
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
                        Button::new("sd-submit-refine")
                            .primary()
                            .small()
                            .disabled(!refine_non_empty)
                            .label("Submit Refinement")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_refinement(window, cx);
                            })),
                    ),
            )
    }
}

impl Render for SolutionDesignerView {
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

        let reset_state = self.reset_state();
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
                                        Icon::new(ActiveView::SolutionDesigner.icon())
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
                                            .child(ActiveView::SolutionDesigner.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::SolutionDesigner.subtitle()),
                                    ),
                            ),
                    )
                    .when(!reset_state.is_clean(), |header| {
                        header.child(render_tool_reset_button(
                            "sd-header-reset",
                            reset_state,
                            move |window, cx| {
                                view_for_reset.update(cx, |this, cx| {
                                    this.request_reset(window, cx);
                                });
                            },
                        ))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(match self.screen {
                        ViewScreen::Landing => self.render_landing_screen(cx),
                        ViewScreen::Workspace => self.render_workspace_screen(cx),
                    }),
            )
    }
}

fn format_rfc3339_short(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Local).format("%b %d, %H:%M").to_string())
        .unwrap_or_else(|_| "-".to_string())
}
