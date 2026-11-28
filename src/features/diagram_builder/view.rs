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
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, Theme, ThemeMode as UiThemeMode,
    h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable as _,
    FontWeight, Image, ImageFormat, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement as _,
    PathPromptOptions, Render, ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Subscription, Window, div, hsla, img,
    prelude::FluentBuilder as _, px,
};
use tokio::sync::mpsc;

use crate::app_state::{ActiveView, AppState};
use crate::features::diagram_builder::history::DiagramHistory;
use crate::features::diagram_builder::renderer::{
    DiagramTheme, RenderedDiagram, render_png, render_svg, validate_and_render,
};
use crate::features::diagram_builder::service::{
    DiagramBuilderService, DiagramStartRequest, DiagramStreamEvent,
};
use crate::infrastructure::copilot::CopilotService;
use crate::infrastructure::notifications::NotificationService;
use crate::infrastructure::runtime::{runtime, spawn_tokio};
use crate::types::{AppSettings, CopilotUsage};
use crate::ui::components::{
    ModelDropdown, ModelDropdownEvent, ModelDropdownMode, ResetState, display_repo_name,
    normalize_default_model, open_reset_confirm_modal, open_timeout_modal, render_context_pill,
    render_tool_reset_button,
};

const TOOL_NAME: &str = "Diagram Builder";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewScreen {
    Start,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagramPanelMode {
    Diagram,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMenu {
    Copy,
    Save,
}

#[derive(Debug, Clone)]
enum FeedItem {
    UserPrompt(String),
    Status { text: String, is_error: bool },
}

#[derive(Debug, Clone)]
struct FailedValidation {
    broken_mermaid: String,
    error: String,
    attempts: usize,
}

#[derive(Debug, Clone)]
enum RetryableTurn {
    Start(DiagramStartRequest),
    Change {
        prompt: String,
        current_mermaid: Option<String>,
    },
    RetryFix {
        broken_mermaid: String,
        error: String,
    },
}

pub struct DiagramBuilderView {
    app_state: Entity<AppState>,
    service: Arc<DiagramBuilderService>,
    root_focus_handle: FocusHandle,

    // Start screen inputs
    description_input: Entity<TextareaState>,
    project_dir_input: Entity<InputState>,
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,

    // Workspace state
    screen: ViewScreen,
    panel_mode: DiagramPanelMode,
    diagram_theme: DiagramTheme,
    user_overrode_diagram_theme: bool,
    history: DiagramHistory,
    rendered_diagram: Option<RenderedDiagram>,
    rendered_image: Option<Arc<Image>>,
    render_generation: u64,

    // Viewport pan & zoom
    zoom: f32,
    fit_to_view: bool,
    pan_x: f32,
    pan_y: f32,
    is_panning: bool,
    last_pan_pos: Option<(f32, f32)>,

    // Chat & status feed (one-sided)
    feed: Vec<FeedItem>,
    feed_scroll_handle: ScrollHandle,
    change_input: Entity<InputState>,

    // Async turn tracking
    is_generating: bool,
    session_run_id: u64,
    active_project_dir: Option<PathBuf>,
    active_description: String,
    cumulative_usage: Option<CopilotUsage>,
    active_db_session_id: Option<i64>,
    last_retry_turn: Option<RetryableTurn>,

    // Validation failure banner
    failed_validation: Option<FailedValidation>,
    showing_broken_source: bool,

    // Toolbar menus & feedback
    open_menu: Option<OpenMenu>,
    feedback_notice: Option<Result<String, String>>,
    feedback_generation: u64,
    validation_error: Option<String>,
    status_notice: Option<String>,

    _subscriptions: Vec<Subscription>,
}

impl DiagramBuilderView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        copilot_service: Arc<CopilotService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = Arc::new(DiagramBuilderService::new(copilot_service));
        let initial_model = normalize_default_model(&initial_settings.copilot_model);
        let initial_diagram_theme =
            DiagramTheme::from_dark(matches!(cx.theme().mode, UiThemeMode::Dark));

        let description_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "Describe the architecture, sequence flow, state machine, or ER diagram you want to build...",
            )
        });
        let project_dir_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("/path/to/local/project (optional)")
        });
        let change_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Describe a change and press Enter...")
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

            let app_diagram_theme =
                DiagramTheme::from_dark(matches!(cx.theme().mode, UiThemeMode::Dark));
            if !this.user_overrode_diagram_theme && app_diagram_theme != this.diagram_theme {
                this.diagram_theme = app_diagram_theme;
                this.rerender_active_snapshot(window, cx);
            }
            cx.notify();
        });

        let model_sub = cx.subscribe(&model_dropdown, |this, _, event, cx| {
            let ModelDropdownEvent::Changed(model_id) = event;
            this.selected_model = model_id.clone();
            this.user_overrode_model = true;
            cx.notify();
        });

        let desc_sub = cx.subscribe(&description_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        let dir_sub = cx.subscribe(&project_dir_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        let change_sub = cx.subscribe_in(&change_input, window, |this, _, event, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.submit_change_input(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });

        Self {
            app_state,
            service,
            root_focus_handle: cx.focus_handle(),
            description_input,
            project_dir_input,
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            screen: ViewScreen::Start,
            panel_mode: DiagramPanelMode::Diagram,
            diagram_theme: initial_diagram_theme,
            user_overrode_diagram_theme: false,
            history: DiagramHistory::new(),
            rendered_diagram: None,
            rendered_image: None,
            render_generation: 0,
            zoom: 1.0,
            fit_to_view: true,
            pan_x: 0.0,
            pan_y: 0.0,
            is_panning: false,
            last_pan_pos: None,
            feed: Vec::new(),
            feed_scroll_handle: ScrollHandle::new(),
            change_input,
            is_generating: false,
            session_run_id: 0,
            active_project_dir: None,
            active_description: String::new(),
            cumulative_usage: None,
            active_db_session_id: None,
            last_retry_turn: None,
            failed_validation: None,
            showing_broken_source: false,
            open_menu: None,
            feedback_notice: None,
            feedback_generation: 0,
            validation_error: None,
            status_notice: None,
            _subscriptions: vec![state_sub, model_sub, desc_sub, dir_sub, change_sub],
        }
    }

    fn set_task(&self, status: Option<String>, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::DiagramBuilder, status);
            cx.notify();
        });
    }

    fn browse_project_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != ViewScreen::Start || self.is_generating {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Project Directory (Optional)".into()),
        });
        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                if let Ok(Ok(Some(paths))) = receiver.await {
                    if let Some(selected) = paths.into_iter().next() {
                        let _ = cx.update(|window, cx| {
                            view.update(cx, |this, cx| {
                                let path_str = selected.to_string_lossy().to_string();
                                this.project_dir_input.update(cx, |input, cx| {
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

    fn handle_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != ViewScreen::Start || self.is_generating {
            return;
        }

        let description = self.description_input.read(cx).value().trim().to_string();
        if description.is_empty() {
            self.validation_error =
                Some("Please describe the diagram you want to create.".to_string());
            cx.notify();
            return;
        }

        let dir_raw = self.project_dir_input.read(cx).value().trim().to_string();
        let project_dir = if dir_raw.is_empty() {
            None
        } else {
            let pb = PathBuf::from(&dir_raw);
            if !pb.is_dir() {
                self.validation_error = Some(format!(
                    "Project directory does not exist or is not a directory: {}",
                    pb.display()
                ));
                cx.notify();
                return;
            }
            Some(pb)
        };

        let settings = self.app_state.read(cx).settings.clone();
        let req = DiagramStartRequest {
            description: description.clone(),
            project_dir: project_dir.clone(),
            model_override: self.selected_model.clone(),
            settings,
            diagram_theme: self.diagram_theme,
        };

        self.validation_error = None;
        self.status_notice = None;
        self.feedback_notice = None;
        self.open_menu = None;
        self.failed_validation = None;
        self.showing_broken_source = false;
        self.history.clear();
        self.rendered_diagram = None;
        self.rendered_image = None;
        self.zoom = 1.0;
        self.fit_to_view = true;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.feed.clear();
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.active_project_dir = project_dir;
        self.active_description = description.clone();
        self.change_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        self.screen = ViewScreen::Workspace;
        self.panel_mode = DiagramPanelMode::Diagram;
        self.push_user_prompt(description, cx);
        self.is_generating = true;
        self.last_retry_turn = Some(RetryableTurn::Start(req.clone()));
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;

        self.set_task(Some("Generating Mermaid diagram...".to_string()), cx);
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

    fn submit_change_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != ViewScreen::Workspace || self.is_generating {
            return;
        }
        let text = self.change_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.change_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.dispatch_change(text, window, cx);
    }

    fn dispatch_change(&mut self, prompt: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_generating {
            return;
        }
        self.open_menu = None;
        self.failed_validation = None;
        self.showing_broken_source = false;
        self.validation_error = None;
        self.push_user_prompt(prompt.clone(), cx);

        let current_mermaid = self.history.current().map(str::to_string);
        self.dispatch_change_turn(prompt, current_mermaid, window, cx);
    }

    fn dispatch_change_turn(
        &mut self,
        prompt: String,
        current_mermaid: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_generating = true;
        self.last_retry_turn = Some(RetryableTurn::Change {
            prompt: prompt.clone(),
            current_mermaid: current_mermaid.clone(),
        });
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;

        self.set_task(Some("Updating Mermaid diagram...".to_string()), cx);
        cx.notify();

        let settings = self.app_state.read(cx).settings.clone();
        let rx = self.service.send_change(
            prompt,
            current_mermaid,
            self.diagram_theme,
            self.active_project_dir.clone(),
            self.selected_model.clone(),
            settings,
        );
        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                Self::drive_turn_stream(view, app_state, run_id, rx, cx).await;
            })
            .detach();
    }

    fn trigger_retry_fix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_generating {
            return;
        }
        let Some(failed) = self.failed_validation.clone() else {
            return;
        };
        self.open_menu = None;
        self.failed_validation = None;
        self.showing_broken_source = false;
        self.dispatch_retry_fix(failed.broken_mermaid, failed.error, window, cx);
    }

    fn dispatch_retry_fix(
        &mut self,
        broken_mermaid: String,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_generating = true;
        self.last_retry_turn = Some(RetryableTurn::RetryFix {
            broken_mermaid: broken_mermaid.clone(),
            error: error.clone(),
        });
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let run_id = self.session_run_id;

        self.set_task(Some("Retrying Mermaid syntax fix...".to_string()), cx);
        cx.notify();

        let settings = self.app_state.read(cx).settings.clone();
        let rx = self.service.retry_fix(
            broken_mermaid,
            error,
            self.diagram_theme,
            self.active_project_dir.clone(),
            self.selected_model.clone(),
            settings,
        );
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
        mut rx: mpsc::UnboundedReceiver<DiagramStreamEvent>,
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
                        DiagramStreamEvent::Status(text) => {
                            this.push_status(text.clone(), false, cx);
                            this.set_task(Some(text), cx);
                        }
                        DiagramStreamEvent::DiagramRendered(rendered) => {
                            this.failed_validation = None;
                            this.showing_broken_source = false;
                            this.history.push(rendered.source.clone());
                            this.apply_rendered_diagram(rendered, cx);
                            this.push_status("Diagram updated.".to_string(), false, cx);
                            NotificationService::show_if_unfocused(
                                window,
                                "Diagram Builder",
                                "Mermaid diagram rendered.",
                            );
                        }
                        DiagramStreamEvent::ValidationExhausted {
                            broken_mermaid,
                            error,
                            attempts,
                        } => {
                            this.push_status(
                                format!(
                                    "Could not fix Mermaid syntax after {attempts} attempts: {error}"
                                ),
                                true,
                                cx,
                            );
                            this.failed_validation = Some(FailedValidation {
                                broken_mermaid,
                                error,
                                attempts,
                            });
                            NotificationService::show_if_unfocused(
                                window,
                                "Diagram Builder",
                                "Mermaid syntax fix failed after 3 attempts.",
                            );
                            cx.notify();
                        }
                        DiagramStreamEvent::TurnCompleted {
                            cumulative_usage,
                            diagram_updated,
                        } => {
                            this.is_generating = false;
                            this.cumulative_usage = Some(cumulative_usage.clone());
                            let ctx_ref = {
                                let short: String =
                                    this.active_description.chars().take(80).collect();
                                if this.active_description.chars().count() > 80 {
                                    format!("Diagram - {short}…")
                                } else {
                                    format!("Diagram - {short}")
                                }
                            };
                            let model_id = this.selected_model.clone();
                            let existing_sid = this.active_db_session_id;
                            let updated_sid = app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::DiagramBuilder, None);
                                let sid = state.upsert_tool_session_usage(
                                    existing_sid,
                                    TOOL_NAME,
                                    Some(&ctx_ref),
                                    Some(if this.history.is_empty() {
                                        "Diagram Session"
                                    } else {
                                        "Mermaid Diagram"
                                    }),
                                    "Diagram Builder Session",
                                    &model_id,
                                    &cumulative_usage,
                                    diagram_updated,
                                    cx,
                                );
                                cx.notify();
                                sid
                            });
                            this.active_db_session_id = updated_sid;
                            cx.notify();
                        }
                        DiagramStreamEvent::TimedOut { timeout_secs } => {
                            this.is_generating = false;
                            this.push_status(
                                format!(
                                    "Request timed out after {timeout_secs} seconds without Copilot output."
                                ),
                                true,
                                cx,
                            );
                            this.set_task(None, cx);
                            open_timeout = Some((timeout_secs, this.last_retry_turn.clone()));
                            cx.notify();
                        }
                        DiagramStreamEvent::Cancelled => {
                            this.is_generating = false;
                            this.set_task(None, cx);
                            cx.notify();
                        }
                        DiagramStreamEvent::Error(msg) => {
                            this.is_generating = false;
                            this.push_status(msg.clone(), true, cx);
                            this.set_task(None, cx);
                            if this.history.is_empty() && this.feed.len() <= 2 {
                                this.validation_error = Some(msg);
                                this.screen = ViewScreen::Start;
                            }
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
                                    this.is_generating = true;
                                    this.session_run_id = this.session_run_id.wrapping_add(1);
                                    let run_id = this.session_run_id;
                                    this.set_task(
                                        Some("Generating Mermaid diagram...".to_string()),
                                        cx,
                                    );
                                    let rx = this.service.start(req);
                                    let view = cx.entity();
                                    let app_state = this.app_state.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            Self::drive_turn_stream(
                                                view, app_state, run_id, rx, cx,
                                            )
                                            .await;
                                        })
                                        .detach();
                                }
                                Some(RetryableTurn::Change {
                                    prompt,
                                    current_mermaid,
                                }) => {
                                    this.dispatch_change_turn(
                                        prompt,
                                        current_mermaid,
                                        window,
                                        cx,
                                    );
                                }
                                Some(RetryableTurn::RetryFix {
                                    broken_mermaid,
                                    error,
                                }) => {
                                    this.dispatch_retry_fix(broken_mermaid, error, window, cx);
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

    fn apply_rendered_diagram(&mut self, rendered: RenderedDiagram, cx: &mut Context<Self>) {
        self.render_generation = self.render_generation.wrapping_add(1);
        let image = Arc::new(Image::from_bytes(
            ImageFormat::Png,
            rendered.png_bytes.clone(),
        ));
        self.rendered_diagram = Some(rendered);
        self.rendered_image = Some(image);
        cx.notify();
    }

    fn rerender_active_snapshot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.history.current().map(str::to_string) else {
            return;
        };
        let theme = self.diagram_theme;
        self.render_generation = self.render_generation.wrapping_add(1);
        let gen_id = self.render_generation;
        let view = cx.entity();

        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move { validate_and_render(&source, theme) }).await;
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.render_generation != gen_id {
                            return;
                        }
                        if let Ok(Ok(rendered)) = res {
                            this.rendered_image = Some(Arc::new(Image::from_bytes(
                                ImageFormat::Png,
                                rendered.png_bytes.clone(),
                            )));
                            this.rendered_diagram = Some(rendered);
                            cx.notify();
                        }
                    });
                });
            })
            .detach();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_generating || !self.history.can_undo() {
            return;
        }
        if self.history.undo().is_some() {
            self.open_menu = None;
            self.rerender_active_snapshot(window, cx);
            cx.notify();
        }
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_generating || !self.history.can_redo() {
            return;
        }
        if self.history.redo().is_some() {
            self.open_menu = None;
            self.rerender_active_snapshot(window, cx);
            cx.notify();
        }
    }

    fn toggle_diagram_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diagram_theme = self.diagram_theme.toggle();
        self.user_overrode_diagram_theme = true;
        self.open_menu = None;
        self.rerender_active_snapshot(window, cx);
        cx.notify();
    }

    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.fit_to_view = false;
        self.zoom = (self.zoom * 1.2).clamp(0.2, 4.0);
        cx.notify();
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.fit_to_view = false;
        self.zoom = (self.zoom / 1.2).clamp(0.2, 4.0);
        cx.notify();
    }

    fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.fit_to_view = true;
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        cx.notify();
    }

    fn set_feedback(&mut self, outcome: Result<String, String>, cx: &mut Context<Self>) {
        self.feedback_notice = Some(outcome);
        self.feedback_generation = self.feedback_generation.wrapping_add(1);
        let gen_id = self.feedback_generation;
        cx.notify();
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let _ = view.update(cx, |this, cx| {
                if this.feedback_generation == gen_id {
                    this.feedback_notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn copy_mermaid_text(&mut self, cx: &mut Context<Self>) {
        let Some(src) = self.history.current().map(str::to_string) else {
            return;
        };
        self.open_menu = None;
        cx.write_to_clipboard(ClipboardItem::new_string(src));
        self.set_feedback(Ok("Copied Mermaid syntax".to_string()), cx);
    }

    fn copy_png_image(&mut self, cx: &mut Context<Self>) {
        let Some(ref rendered) = self.rendered_diagram else {
            return;
        };
        self.open_menu = None;
        let image = Image::from_bytes(ImageFormat::Png, rendered.png_bytes.clone());
        cx.write_to_clipboard(ClipboardItem::new_image(&image));
        self.set_feedback(Ok("Copied PNG image".to_string()), cx);
    }

    fn save_export(&mut self, ext: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.history.current().map(str::to_string) else {
            return;
        };
        self.open_menu = None;
        let theme = self.diagram_theme;
        let start_dir = self
            .active_project_dir
            .clone()
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let suggested = format!("diagram.{ext}");
        let receiver = cx.prompt_for_new_path(&start_dir, Some(&suggested));
        let view = cx.entity();

        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(path))) = receiver.await else {
                    return;
                };
                let write_path = path.clone();
                let res = spawn_tokio(async move {
                    let bytes: Vec<u8> = match ext {
                        "mmd" => source.into_bytes(),
                        "svg" => render_svg(&source, theme)
                            .map_err(anyhow::Error::msg)?
                            .into_bytes(),
                        "png" => render_png(&source, theme).map_err(anyhow::Error::msg)?,
                        _ => anyhow::bail!("Unsupported export format: {ext}"),
                    };
                    tokio::fs::write(&write_path, &bytes).await?;
                    Ok::<_, anyhow::Error>(write_path.to_string_lossy().to_string())
                })
                .await;

                let outcome = match res {
                    Ok(Ok(saved)) => Ok(format!("Saved {saved}")),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                };
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.set_feedback(outcome, cx);
                    });
                });
            })
            .detach();
    }

    fn cancel_active_turn(&mut self, cx: &mut Context<Self>) {
        self.session_run_id = self.session_run_id.wrapping_add(1);
        let service = Arc::clone(&self.service);
        runtime().spawn(async move {
            service.stop().await;
        });
        self.is_generating = false;
        self.set_task(None, cx);

        if self.history.is_empty() {
            self.screen = ViewScreen::Start;
            self.status_notice = Some("Diagram generation cancelled.".to_string());
        } else {
            self.push_status("Cancelled.".to_string(), false, cx);
        }
        cx.notify();
    }

    fn reset_state(&self, cx: &App) -> ResetState {
        if self.is_generating {
            return ResetState::NeedsConfirmation { is_running: true };
        }
        if self.screen == ViewScreen::Workspace || !self.history.is_empty() || !self.feed.is_empty()
        {
            return ResetState::NeedsConfirmation { is_running: false };
        }
        let has_dirty_inputs = !self.description_input.read(cx).value().trim().is_empty()
            || !self.project_dir_input.read(cx).value().trim().is_empty()
            || self.user_overrode_model
            || self.validation_error.is_some()
            || self.status_notice.is_some();
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
                    "This will cancel the active Copilot diagram session and clear all diagram history and inputs."
                } else {
                    "This will end the current diagram session and clear the diagram, undo history, and chat log."
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
        self.session_run_id = self.session_run_id.wrapping_add(1);
        self.render_generation = self.render_generation.wrapping_add(1);
        self.feedback_generation = self.feedback_generation.wrapping_add(1);
        let service = Arc::clone(&self.service);
        runtime().spawn(async move {
            service.stop().await;
        });

        self.screen = ViewScreen::Start;
        self.panel_mode = DiagramPanelMode::Diagram;
        self.is_generating = false;
        self.history.clear();
        self.rendered_diagram = None;
        self.rendered_image = None;
        self.zoom = 1.0;
        self.fit_to_view = true;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.is_panning = false;
        self.last_pan_pos = None;
        self.feed.clear();
        self.active_project_dir = None;
        self.active_description.clear();
        self.cumulative_usage = None;
        self.active_db_session_id = None;
        self.last_retry_turn = None;
        self.failed_validation = None;
        self.showing_broken_source = false;
        self.open_menu = None;
        self.feedback_notice = None;
        self.validation_error = None;
        self.status_notice = None;

        self.description_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.project_dir_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.change_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        let default_model =
            normalize_default_model(&self.app_state.read(cx).settings.copilot_model);
        self.user_overrode_model = false;
        self.selected_model = default_model.clone();
        self.model_dropdown.update(cx, |dd, cx| {
            dd.set_selected_model(default_model, window, cx);
        });

        self.user_overrode_diagram_theme = false;
        self.diagram_theme =
            DiagramTheme::from_dark(matches!(cx.theme().mode, UiThemeMode::Dark));

        self.set_task(None, cx);
        cx.notify();
    }

    fn push_user_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        self.feed.push(FeedItem::UserPrompt(text));
        self.feed_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn push_status(&mut self, text: String, is_error: bool, cx: &mut Context<Self>) {
        self.feed.push(FeedItem::Status { text, is_error });
        self.feed_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn handle_workspace_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.screen != ViewScreen::Workspace || self.is_generating {
            return;
        }
        if self
            .change_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
        {
            return;
        }

        let mods = &event.keystroke.modifiers;
        #[cfg(target_os = "macos")]
        let primary_mod = mods.platform && !mods.control && !mods.alt;
        #[cfg(not(target_os = "macos"))]
        let primary_mod = mods.control && !mods.platform && !mods.alt;

        if !primary_mod {
            return;
        }

        let key = event.keystroke.key.as_str();
        if key.eq_ignore_ascii_case("z") && !mods.shift {
            self.undo(window, cx);
            cx.stop_propagation();
        } else if (key.eq_ignore_ascii_case("z") && mods.shift)
            || (cfg!(not(target_os = "macos")) && key.eq_ignore_ascii_case("y") && !mods.shift)
        {
            self.redo(window, cx);
            cx.stop_propagation();
        }
    }

    fn render_start_screen(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let can_start = !self.description_input.read(cx).value().trim().is_empty();

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

        div()
            .size_full()
            .overflow_y_scrollbar()
            .p_8()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(680.))
                    .mx_auto()
                    .p_6()
                    .gap_5()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(label("Describe Your Diagram"))
                            .child(Textarea::new(&self.description_input).h(px(180.)))
                            .child(hint(
                                "Describe the flow, system architecture, sequence, or data model you want to visualize.",
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(label("Project Directory (Optional)"))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(Input::new(&self.project_dir_input)),
                                    )
                                    .child(
                                        Button::new("diagram-browse-project")
                                            .small()
                                            .icon(Icon::new(LucideIcon::FolderOpen).small())
                                            .label("Browse")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.browse_project_dir(window, cx);
                                            })),
                                    ),
                            )
                            .child(hint(
                                "Optional — attach a local project folder for read-only codebase investigation.",
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
                        Button::new("diagram-create")
                            .primary()
                            .w_full()
                            .disabled(!can_start)
                            .icon(Icon::new(LucideIcon::Sparkles).small())
                            .label("Create Diagram")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.handle_start(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_diagram_toolbar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let has_diagram = !self.history.is_empty();
        let has_png = self.rendered_diagram.is_some();
        let can_undo = self.history.can_undo() && !self.is_generating;
        let can_redo = self.history.can_redo() && !self.is_generating;
        let zoom_label = if self.fit_to_view {
            "Fit".to_string()
        } else {
            format!("{}%", (self.zoom * 100.0).round() as i32)
        };
        let theme_icon = if self.diagram_theme.is_dark() {
            LucideIcon::Moon
        } else {
            LucideIcon::Sun
        };
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        h_flex()
            .relative()
            .items_center()
            .justify_between()
            .flex_wrap()
            .gap_2()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .p_0p5()
                            .gap_0p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child({
                                let btn = Button::new("diagram-tab-preview")
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::Eye).xsmall())
                                    .label("Diagram")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.panel_mode = DiagramPanelMode::Diagram;
                                        this.open_menu = None;
                                        cx.notify();
                                    }));
                                if self.panel_mode == DiagramPanelMode::Diagram {
                                    btn.primary()
                                } else {
                                    btn.ghost()
                                }
                            })
                            .child({
                                let btn = Button::new("diagram-tab-code")
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::FileCode).xsmall())
                                    .label("Code")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.panel_mode = DiagramPanelMode::Code;
                                        this.open_menu = None;
                                        cx.notify();
                                    }));
                                if self.panel_mode == DiagramPanelMode::Code {
                                    btn.primary()
                                } else {
                                    btn.ghost()
                                }
                            }),
                    )
                    .child(div().w(px(1.)).h(px(18.)).bg(theme.border))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("diagram-undo")
                                    .ghost()
                                    .xsmall()
                                    .disabled(!can_undo)
                                    .icon(Icon::new(LucideIcon::Undo2).xsmall())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.undo(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("diagram-redo")
                                    .ghost()
                                    .xsmall()
                                    .disabled(!can_redo)
                                    .icon(Icon::new(LucideIcon::RotateCw).xsmall())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.redo(window, cx);
                                    })),
                            ),
                    )
                    .when(self.panel_mode == DiagramPanelMode::Diagram, |row| {
                        row.child(div().w(px(1.)).h(px(18.)).bg(theme.border))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        Button::new("diagram-zoom-out")
                                            .ghost()
                                            .xsmall()
                                            .disabled(!has_png)
                                            .label("−")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.zoom_out(cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("diagram-zoom-reset")
                                            .ghost()
                                            .xsmall()
                                            .disabled(!has_png)
                                            .label(zoom_label)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.reset_zoom(cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("diagram-zoom-in")
                                            .ghost()
                                            .xsmall()
                                            .disabled(!has_png)
                                            .icon(Icon::new(LucideIcon::Plus).xsmall())
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.zoom_in(cx);
                                            })),
                                    ),
                            )
                    }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .when_some(self.feedback_notice.clone(), |row, fb| {
                        let (color, icon, msg) = match fb {
                            Ok(m) => (green, LucideIcon::Check, m),
                            Err(e) => (red, LucideIcon::CircleAlert, e),
                        };
                        row.child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .max_w(px(220.))
                                .px_2()
                                .py_0p5()
                                .rounded(theme.radius)
                                .bg(color.opacity(0.12))
                                .text_xs()
                                .text_color(color)
                                .child(Icon::new(icon).xsmall())
                                .child(div().truncate().child(msg)),
                        )
                    })
                    .child(
                        Button::new("diagram-theme-toggle")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(theme_icon).xsmall())
                            .label(self.diagram_theme.label())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_diagram_theme(window, cx);
                            })),
                    )
                    .child(
                        Button::new("diagram-copy-menu-btn")
                            .small()
                            .disabled(!has_diagram)
                            .icon(Icon::new(LucideIcon::Copy).xsmall())
                            .label("Copy")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_menu = match this.open_menu {
                                    Some(OpenMenu::Copy) => None,
                                    _ => Some(OpenMenu::Copy),
                                };
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("diagram-save-menu-btn")
                            .primary()
                            .small()
                            .disabled(!has_diagram)
                            .icon(Icon::new(LucideIcon::Save).xsmall())
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_menu = match this.open_menu {
                                    Some(OpenMenu::Save) => None,
                                    _ => Some(OpenMenu::Save),
                                };
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_open_menu_overlay(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.open_menu?;
        let has_png = self.rendered_diagram.is_some();

        let panel = match menu {
            OpenMenu::Copy => v_flex()
                .w(px(210.))
                .p_1p5()
                .gap_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    Button::new("diagram-copy-mmd")
                        .ghost()
                        .small()
                        .w_full()
                        .icon(Icon::new(LucideIcon::FileCode).small())
                        .label("Copy Mermaid Syntax")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_mermaid_text(cx);
                        })),
                )
                .child(
                    Button::new("diagram-copy-png")
                        .ghost()
                        .small()
                        .w_full()
                        .disabled(!has_png)
                        .icon(Icon::new(LucideIcon::Copy).small())
                        .label("Copy PNG Image")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_png_image(cx);
                        })),
                ),
            OpenMenu::Save => v_flex()
                .w(px(210.))
                .p_1p5()
                .gap_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .child(
                    Button::new("diagram-save-mmd")
                        .ghost()
                        .small()
                        .w_full()
                        .icon(Icon::new(LucideIcon::FileCode).small())
                        .label("Save as Mermaid (.mmd)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_export("mmd", window, cx);
                        })),
                )
                .child(
                    Button::new("diagram-save-svg")
                        .ghost()
                        .small()
                        .w_full()
                        .icon(Icon::new(LucideIcon::FileText).small())
                        .label("Save as SVG (.svg)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_export("svg", window, cx);
                        })),
                )
                .child(
                    Button::new("diagram-save-png")
                        .ghost()
                        .small()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Save).small())
                        .label("Save as PNG (.png)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_export("png", window, cx);
                        })),
                ),
        };

        Some(
            div()
                .absolute()
                .top_2()
                .right_4()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_failed_validation_banner(
        &self,
        failed: &FailedValidation,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let mono_font = theme.mono_font_family.clone();
        let broken_for_copy = failed.broken_mermaid.clone();

        v_flex()
            .w_full()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(red.opacity(0.35))
            .bg(red.opacity(0.08))
            .child(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .items_start()
                            .gap_2()
                            .text_xs()
                            .text_color(red)
                            .child(Icon::new(LucideIcon::CircleAlert).small())
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                                        "Failed to render valid Mermaid after {} auto-fix attempts",
                                        failed.attempts
                                    )))
                                    .child(div().child(failed.error.clone())),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .flex_shrink_0()
                            .child(
                                Button::new("diagram-retry-fix")
                                    .primary()
                                    .xsmall()
                                    .disabled(self.is_generating)
                                    .icon(Icon::new(LucideIcon::RefreshCw).xsmall())
                                    .label("Retry Fix")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.trigger_retry_fix(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("diagram-toggle-broken-code")
                                    .ghost()
                                    .xsmall()
                                    .icon(
                                        Icon::new(if self.showing_broken_source {
                                            LucideIcon::EyeOff
                                        } else {
                                            LucideIcon::Eye
                                        })
                                        .xsmall(),
                                    )
                                    .label(if self.showing_broken_source {
                                        "Hide Broken Code"
                                    } else {
                                        "View Broken Code"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.showing_broken_source = !this.showing_broken_source;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("diagram-dismiss-broken")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::X).xsmall())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.failed_validation = None;
                                        this.showing_broken_source = false;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when(self.showing_broken_source, |col| {
                col.child(
                    v_flex()
                        .gap_1p5()
                        .p_2p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.muted_foreground)
                                        .child("Invalid Mermaid Source"),
                                )
                                .child(
                                    Button::new("diagram-copy-broken")
                                        .ghost()
                                        .xsmall()
                                        .icon(Icon::new(LucideIcon::Copy).xsmall())
                                        .label("Copy")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                broken_for_copy.clone(),
                                            ));
                                            this.set_feedback(
                                                Ok("Copied broken Mermaid".to_string()),
                                                cx,
                                            );
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .max_h(px(160.))
                                .overflow_y_scrollbar()
                                .font_family(mono_font)
                                .text_xs()
                                .text_color(theme.foreground)
                                .child(failed.broken_mermaid.clone()),
                        ),
                )
            })
    }

    fn render_diagram_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let canvas_bg = if self.diagram_theme.is_dark() {
            hsla(215.0 / 360.0, 0.21, 0.11, 1.0)
        } else {
            gpui_kit::white()
        };

        let content: AnyElement = match self.panel_mode {
            DiagramPanelMode::Diagram => {
                if let (Some(rendered), Some(image)) =
                    (self.rendered_diagram.as_ref(), self.rendered_image.as_ref())
                {
                    let img_el = if self.fit_to_view {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .p_6()
                            .child(
                                img(image.clone())
                                    .max_w_full()
                                    .max_h_full()
                                    .object_fit(ObjectFit::Contain),
                            )
                            .into_any_element()
                    } else {
                        let w = (rendered.width * self.zoom).max(20.0);
                        let h = (rendered.height * self.zoom).max(20.0);
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .overflow_hidden()
                            .child(
                                div()
                                    .ml(px(self.pan_x))
                                    .mt(px(self.pan_y))
                                    .child(img(image.clone()).w(px(w)).h(px(h))),
                            )
                            .into_any_element()
                    };

                    div()
                        .id("diagram-canvas")
                        .relative()
                        .size_full()
                        .bg(canvas_bg)
                        .overflow_hidden()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.open_menu = None;
                                this.root_focus_handle.focus(window, cx);
                                this.is_panning = true;
                                this.last_pan_pos =
                                    Some((event.position.x.into(), event.position.y.into()));
                                cx.notify();
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                            if !this.is_panning {
                                return;
                            }
                            let cur: (f32, f32) =
                                (event.position.x.into(), event.position.y.into());
                            if let Some((lx, ly)) = this.last_pan_pos {
                                this.fit_to_view = false;
                                this.pan_x += cur.0 - lx;
                                this.pan_y += cur.1 - ly;
                            }
                            this.last_pan_pos = Some(cur);
                            cx.notify();
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.is_panning = false;
                                this.last_pan_pos = None;
                                cx.notify();
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                this.is_panning = false;
                                this.last_pan_pos = None;
                                cx.notify();
                            }),
                        )
                        .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                            let delta = event.delta.pixel_delta(px(16.0));
                            let dy: f32 = delta.y.into();
                            if dy.abs() > 0.0 {
                                this.fit_to_view = false;
                                let factor = if dy > 0.0 { 1.08 } else { 1.0 / 1.08 };
                                this.zoom = (this.zoom * factor).clamp(0.2, 4.0);
                                cx.notify();
                            }
                        }))
                        .child(img_el)
                        .when(self.is_generating, |canvas| {
                            canvas.child(
                                h_flex()
                                    .absolute()
                                    .bottom_4()
                                    .left_4()
                                    .items_center()
                                    .gap_2()
                                    .px_3()
                                    .py_1p5()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .text_xs()
                                    .text_color(theme.foreground)
                                    .child(Spinner::new().xsmall().color(theme.primary))
                                    .child("Updating diagram..."),
                            )
                        })
                        .into_any_element()
                } else if self.is_generating {
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .child(Spinner::new().large().color(theme.primary))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.muted_foreground)
                                .child("Generating Mermaid diagram..."),
                        )
                        .into_any_element()
                } else {
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .p_8()
                        .child(Icon::new(LucideIcon::Route).large())
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("No valid diagram rendered yet."),
                        )
                        .into_any_element()
                }
            }
            DiagramPanelMode::Code => {
                let code = self.history.current().unwrap_or("");
                let fenced = format!("```mermaid\n{code}\n```");
                div()
                    .size_full()
                    .overflow_y_scrollbar()
                    .p_5()
                    .bg(theme.background)
                    .child(
                        TextView::markdown(("diagram-code-view", self.render_generation), fenced)
                            .selectable(true),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .child(self.render_diagram_toolbar(theme, cx))
            .when_some(self.failed_validation.as_ref(), |col, failed| {
                col.child(self.render_failed_validation_banner(failed, theme, cx))
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(content)
                    .when_some(self.render_open_menu_overlay(theme, cx), |pane, overlay| {
                        pane.child(overlay)
                    }),
            )
            .into_any_element()
    }

    fn render_chat_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let indigo = hsla(243.0 / 360.0, 0.75, 0.59, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let mono_font = theme.mono_font_family.clone();
        let has_change_text = !self.change_input.read(cx).value().trim().is_empty();
        let repo_name = self
            .active_project_dir
            .as_ref()
            .map(|p| display_repo_name(&p.to_string_lossy()))
            .unwrap_or_default();

        let feed_elements: Vec<AnyElement> = self
            .feed
            .iter()
            .map(|item| match item {
                FeedItem::UserPrompt(text) => h_flex()
                    .w_full()
                    .min_w_0()
                    .justify_end()
                    .child(
                        v_flex()
                            .max_w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .p_3()
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
                            .child(div().w_full().min_w_0().text_sm().child(text.clone())),
                    )
                    .into_any_element(),
                FeedItem::Status { text, is_error } => {
                    let color = if *is_error { red } else { theme.muted_foreground };
                    let icon = if *is_error {
                        LucideIcon::CircleAlert
                    } else {
                        LucideIcon::Terminal
                    };
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .justify_start()
                        .child(
                            h_flex()
                                .items_start()
                                .gap_2()
                                .w_full()
                                .min_w_0()
                                .px_2p5()
                                .py_1p5()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(if *is_error {
                                    red.opacity(0.35)
                                } else {
                                    theme.border
                                })
                                .bg(if *is_error {
                                    red.opacity(0.08)
                                } else {
                                    theme.sidebar
                                })
                                .font_family(mono_font.clone())
                                .text_xs()
                                .text_color(color)
                                .child(Icon::new(icon).xsmall().flex_shrink_0())
                                .child(div().flex_1().min_w_0().child(text.clone())),
                        )
                        .into_any_element()
                }
            })
            .collect();

        v_flex()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .child(
                v_flex()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(Icon::new(LucideIcon::MessageSquareText).small())
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Changes"),
                                    ),
                            )
                            .child(render_context_pill(
                                LucideIcon::Cpu,
                                self.selected_model.clone(),
                                false,
                                cx,
                            )),
                    )
                    .when(!repo_name.is_empty(), |hdr| {
                        hdr.child(render_context_pill(
                            LucideIcon::Folder,
                            repo_name,
                            false,
                            cx,
                        ))
                    }),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .id("diagram-feed-scroll")
                            .size_full()
                            .min_w_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.feed_scroll_handle)
                            .p_4()
                            .gap_2p5()
                            .children(feed_elements)
                            .when(self.is_generating, |col| {
                                col.child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .pt_1()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(Spinner::new().small().color(theme.primary))
                                        .child("Working..."),
                                )
                            }),
                    )
                    .vertical_scrollbar(&self.feed_scroll_handle),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.change_input).disabled(self.is_generating)),
                    )
                    .child(if self.is_generating {
                        Button::new("diagram-cancel-turn")
                            .danger()
                            .small()
                            .icon(Icon::new(LucideIcon::Square).small())
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_active_turn(cx);
                            }))
                    } else {
                        Button::new("diagram-send-change")
                            .primary()
                            .small()
                            .disabled(!has_change_text)
                            .icon(Icon::new(LucideIcon::Send).small())
                            .label("Send")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_change_input(window, cx);
                            }))
                    }),
            )
            .into_any_element()
    }
}

impl Render for DiagramBuilderView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let reset_state = self.reset_state(cx);
        let view_for_reset = cx.entity();

        v_flex()
            .id("diagram-builder-root")
            .track_focus(&self.root_focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_workspace_key_down(event, window, cx);
            }))
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(theme.border)
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
                                    .rounded(theme.radius_lg)
                                    .bg(theme.secondary)
                                    .text_color(theme.accent_foreground)
                                    .child(
                                        Icon::new(ActiveView::DiagramBuilder.icon())
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
                                            .child(ActiveView::DiagramBuilder.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(ActiveView::DiagramBuilder.subtitle()),
                                    ),
                            ),
                    )
                    .child(render_tool_reset_button(
                        "diagram-header-reset",
                        reset_state,
                        move |window, cx| {
                            view_for_reset.update(cx, |this, cx| {
                                this.request_reset(window, cx);
                            });
                        },
                    )),
            )
            .child(div().flex_1().min_h_0().w_full().child(match self.screen {
                ViewScreen::Start => self.render_start_screen(cx),
                ViewScreen::Workspace => h_resizable("diagram-builder-split")
                    .child(resizable_panel().child(self.render_diagram_panel(&theme, cx)))
                    .child(
                        resizable_panel()
                            .size(px(360.))
                            .size_range(px(280.)..px(540.))
                            .child(self.render_chat_panel(&theme, cx)),
                    )
                    .into_any_element(),
            }))
    }
}
