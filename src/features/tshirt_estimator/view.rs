use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
    px, rgb,
};

use crate::app_state::{ActiveView, AppState};
use crate::features::tshirt_estimator::service::{
    self, ActiveEstimationHandle, EstimationRequest, EstimationResult, EstimationStreamEvent,
};
use crate::infrastructure::notifications::NotificationService;
use crate::ui::components::{
    ModelDropdown, ModelDropdownEvent, ModelDropdownMode, ResetState, normalize_default_model,
    open_reset_confirm_modal, open_timeout_modal, render_tool_reset_button,
};

#[derive(Debug, Clone)]
enum EstimatorStage {
    Idle,
    Estimating,
    Completed(EstimationResult),
    Error(String),
}

pub struct TShirtEstimatorView {
    app_state: Entity<AppState>,
    description_input: Entity<TextareaState>,
    repo_path_input: Entity<InputState>,
    branch_input: Entity<InputState>,
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,
    stage: EstimatorStage,
    validation_error: Option<String>,
    status_logs: Vec<String>,
    logs_expanded: bool,
    copied: bool,
    copy_generation: u64,
    estimation_gen: u64,
    active_handle: Option<ActiveEstimationHandle>,
    terminal_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl TShirtEstimatorView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_model = normalize_default_model(&initial_model);

        let description_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Describe the feature, bug fix, or refactoring you want to estimate...")
        });

        let repo_path_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("/path/to/local/repository")
        });

        let branch_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("develop")
                .default_value("develop")
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
            if !this.user_overrode_model {
                let default_model =
                    normalize_default_model(&app_state.read(cx).settings.copilot_model);
                if default_model != this.selected_model {
                    this.selected_model = default_model.clone();
                    this.model_dropdown.update(cx, |dd, cx| {
                        dd.set_selected_model(default_model, window, cx);
                    });
                }
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

        Self {
            app_state,
            description_input,
            repo_path_input,
            branch_input,
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            stage: EstimatorStage::Idle,
            validation_error: None,
            status_logs: Vec::new(),
            logs_expanded: false,
            copied: false,
            copy_generation: 0,
            estimation_gen: 0,
            active_handle: None,
            terminal_scroll_handle: ScrollHandle::new(),
            _subscriptions: vec![state_sub, model_sub, desc_sub, repo_sub, branch_sub],
        }
    }

    fn browse_repo_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn start_estimation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.stage, EstimatorStage::Estimating) {
            return;
        }

        let description = self.description_input.read(cx).value().to_string();
        if description.trim().is_empty() {
            self.validation_error = Some("Please enter a proposed change description.".to_string());
            cx.notify();
            return;
        }

        let repo_raw = self.repo_path_input.read(cx).value().to_string();
        let repo_trimmed = repo_raw.trim();
        if repo_trimmed.is_empty() {
            self.validation_error = Some("Please select a local repository directory.".to_string());
            cx.notify();
            return;
        }

        let repo_path = PathBuf::from(repo_trimmed);
        if !repo_path.is_dir() {
            self.validation_error = Some(format!(
                "Repository path does not exist or is not a directory: {}",
                repo_path.display()
            ));
            cx.notify();
            return;
        }

        let branch = {
            let raw = self.branch_input.read(cx).value().to_string();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                "develop".to_string()
            } else {
                trimmed.to_string()
            }
        };

        let (copilot, git, settings) = {
            let state = self.app_state.read(cx);
            (
                state.copilot_service.clone(),
                state.git_service.clone(),
                state.settings.clone(),
            )
        };

        self.validation_error = None;
        self.status_logs.clear();
        self.logs_expanded = false;
        self.copied = false;
        self.stage = EstimatorStage::Estimating;
        self.estimation_gen = self.estimation_gen.wrapping_add(1);
        let run_gen = self.estimation_gen;

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::TShirtEstimator,
                Some("Starting estimation...".to_string()),
            );
            cx.notify();
        });

        let req = EstimationRequest {
            description,
            repo_path,
            branch,
            model_override: self.selected_model.clone(),
            settings,
        };

        let (handle, mut rx) = service::start_estimation(copilot, git, req);
        self.active_handle = Some(handle);
        cx.notify();

        let view = cx.entity();
        let app_state = self.app_state.clone();

        window
            .spawn(cx, async move |cx| {
                while let Some(event) = rx.recv().await {
                    let _ = cx.update(|window, cx| {
                        let mut open_timeout_secs: Option<u64> = None;

                        view.update(cx, |this, cx| {
                            if this.estimation_gen != run_gen {
                                return;
                            }
                            match event {
                            EstimationStreamEvent::Status(line) => {
                                this.status_logs.push(line.clone());
                                this.terminal_scroll_handle.scroll_to_bottom();
                                app_state.update(cx, |state, cx| {
                                    state.set_task_status(ActiveView::TShirtEstimator, Some(line));
                                    cx.notify();
                                });
                                cx.notify();
                            }
                            EstimationStreamEvent::Completed(result) => {
                                let size_label = result.size.label();
                                let usage = result.usage.clone();
                                let model_id = this.selected_model.clone();
                                this.active_handle = None;
                                this.stage = EstimatorStage::Completed(result);
                                app_state.update(cx, |state, cx| {
                                    state.set_task_status(ActiveView::TShirtEstimator, None);
                                    state.record_completed_tool_session(
                                        "T-Shirt Size Estimator",
                                        None,
                                        Some("T-Shirt Estimate"),
                                        "T-Shirt Estimation Session",
                                        &model_id,
                                        &usage,
                                        cx,
                                    );
                                    cx.notify();
                                });
                                NotificationService::show_if_unfocused(
                                    window,
                                    "T-Shirt Estimation Complete",
                                    format!("Estimated effort size: {size_label}"),
                                );
                                cx.notify();
                            }
                            EstimationStreamEvent::TimedOut { timeout_secs } => {
                                this.active_handle = None;
                                this.stage = EstimatorStage::Error(format!(
                                    "Estimation timed out after {timeout_secs} seconds without Copilot output."
                                ));
                                app_state.update(cx, |state, cx| {
                                    state.set_task_status(ActiveView::TShirtEstimator, None);
                                    cx.notify();
                                });
                                open_timeout_secs = Some(timeout_secs);
                                cx.notify();
                            }
                            EstimationStreamEvent::Cancelled => {
                                this.active_handle = None;
                                this.stage = EstimatorStage::Idle;
                                app_state.update(cx, |state, cx| {
                                    state.set_task_status(ActiveView::TShirtEstimator, None);
                                    cx.notify();
                                });
                                cx.notify();
                            }
                            EstimationStreamEvent::Error(msg) => {
                                this.active_handle = None;
                                this.stage = EstimatorStage::Error(msg);
                                app_state.update(cx, |state, cx| {
                                    state.set_task_status(ActiveView::TShirtEstimator, None);
                                    cx.notify();
                                });
                                cx.notify();
                            }
                            }
                        });

                        if let Some(secs) = open_timeout_secs {
                            let view_for_retry = view.clone();
                            open_timeout_modal(
                                "T-Shirt Size Estimator",
                                secs,
                                move |window, cx| {
                                    view_for_retry.update(cx, |this, cx| {
                                        this.start_estimation(window, cx);
                                    });
                                },
                                window,
                                cx,
                            );
                        }
                    });
                }
            })
            .detach();
    }

    fn cancel_estimation(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.active_handle.as_ref() {
            handle.cancel();
            self.status_logs.push("Cancelling estimation...".to_string());
            self.terminal_scroll_handle.scroll_to_bottom();
            cx.notify();
        }
    }

    fn reset_state(&self, cx: &App) -> ResetState {
        match self.stage {
            EstimatorStage::Estimating => ResetState::NeedsConfirmation { is_running: true },
            EstimatorStage::Completed(_) => ResetState::NeedsConfirmation { is_running: false },
            EstimatorStage::Idle | EstimatorStage::Error(_) => {
                let has_dirty_inputs = !self.description_input.read(cx).value().trim().is_empty()
                    || !self.repo_path_input.read(cx).value().trim().is_empty()
                    || self.branch_input.read(cx).value().trim() != "develop"
                    || self.user_overrode_model
                    || matches!(self.stage, EstimatorStage::Error(_))
                    || self.validation_error.is_some()
                    || !self.status_logs.is_empty();
                if has_dirty_inputs {
                    ResetState::DirtyInputsOnly
                } else {
                    ResetState::Clean
                }
            }
        }
    }

    fn request_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.reset_state(cx) {
            ResetState::Clean => {}
            ResetState::DirtyInputsOnly => self.reset(window, cx),
            ResetState::NeedsConfirmation { is_running } => {
                let view = cx.entity();
                let detail = if is_running {
                    "This will cancel the running estimation, clean up any temporary worktree, and clear all inputs and logs."
                } else {
                    "This will clear the current estimation result, activity log, and all form inputs."
                };
                open_reset_confirm_modal(
                    "T-Shirt Estimator",
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
        self.estimation_gen = self.estimation_gen.wrapping_add(1);
        self.copy_generation = self.copy_generation.wrapping_add(1);
        if let Some(handle) = self.active_handle.take() {
            handle.cancel();
        }
        self.description_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.repo_path_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.branch_input
            .update(cx, |input, cx| input.set_value("develop", window, cx));

        let default_model =
            normalize_default_model(&self.app_state.read(cx).settings.copilot_model);
        self.user_overrode_model = false;
        self.selected_model = default_model.clone();
        self.model_dropdown.update(cx, |dd, cx| {
            dd.set_selected_model(default_model, window, cx);
        });

        self.stage = EstimatorStage::Idle;
        self.validation_error = None;
        self.status_logs.clear();
        self.logs_expanded = false;
        self.copied = false;
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::TShirtEstimator, None);
            cx.notify();
        });
        cx.notify();
    }

    fn copy_result(&mut self, result: &EstimationResult, cx: &mut Context<Self>) {
        let text = format!(
            "Estimated Size: {}\n\nReasoning:\n{}",
            result.size.label(),
            result.reasoning
        );
        cx.write_to_clipboard(ClipboardItem::new_string(text));
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

    fn render_left_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let is_estimating = matches!(self.stage, EstimatorStage::Estimating);
        let worktree_enabled = self.app_state.read(cx).settings.git_worktree_enabled;

        v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .bg(theme.sidebar)
            .p_5()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Proposed Change Description"),
                    )
                    .child(
                        Textarea::new(&self.description_input)
                            .h(px(190.))
                            .disabled(is_estimating),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Local Repository Path"),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.repo_path_input).disabled(is_estimating)),
                            )
                            .child(
                                Button::new("browse-tshirt-repo")
                                    .disabled(is_estimating)
                                    .icon(Icon::new(LucideIcon::FolderOpen).small())
                                    .label("Browse")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.browse_repo_dir(window, cx);
                                    })),
                            ),
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
                        .child(Input::new(&self.branch_input).disabled(is_estimating))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Analyzed inside an isolated temporary Git worktree."),
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
                            .child("Copilot Model"),
                    )
                    .child(self.model_dropdown.clone()),
            )
            .when_some(self.validation_error.clone(), |col, err| {
                let red = gpui_kit::hsla(0.0, 0.78, 0.55, 1.0);
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
                div().pt_2().child(if is_estimating {
                    Button::new("cancel-tshirt-estimation")
                        .danger()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Square).small())
                        .label("Cancel Estimation")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_estimation(cx);
                        }))
                } else {
                    Button::new("start-tshirt-estimation")
                        .primary()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Play).small())
                        .label("Start Estimation")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_estimation(window, cx);
                        }))
                }),
            )
    }

    fn render_terminal_box(&self, min_h: Option<f32>, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let mono_font = theme.mono_font_family.clone();
        let terminal_bg = rgb(0x0f172a);
        let terminal_fg = rgb(0xe2e8f0);
        let terminal_muted = rgb(0x64748b);

        div()
            .relative()
            .w_full()
            .when_some(min_h, |el, h| el.h(px(h)))
            .when(min_h.is_none(), |el| el.flex_1().min_h_0())
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(terminal_bg)
            .overflow_hidden()
            .child(
                v_flex()
                    .id("tshirt-terminal-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.terminal_scroll_handle)
                    .p_4()
                    .gap_1p5()
                    .font_family(mono_font)
                    .text_xs()
                    .text_color(terminal_fg)
                    .when(self.status_logs.is_empty(), |col| {
                        col.child(
                            div()
                                .text_color(terminal_muted)
                                .child("> Waiting for Copilot output..."),
                        )
                    })
                    .children(self.status_logs.iter().map(|line| {
                        h_flex()
                            .items_start()
                            .gap_2()
                            .child(div().text_color(rgb(0x38bdf8)).child(">"))
                            .child(div().flex_1().min_w_0().child(line.clone()))
                    })),
            )
            .vertical_scrollbar(&self.terminal_scroll_handle)
    }

    fn render_right_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        match &self.stage {
            EstimatorStage::Idle => v_flex()
                .size_full()
                .p_8()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(theme.background)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(56.))
                        .rounded(theme.radius_lg)
                        .bg(theme.secondary)
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(LucideIcon::Shirt).large()),
                )
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Ready to Estimate"),
                )
                .child(
                    div()
                        .max_w(px(440.))
                        .text_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Describe the proposed change and choose a local repository on the left. Copilot will inspect the codebase in read-only mode and estimate an XS–XL effort size with detailed reasoning."),
                )
                .into_any_element(),

            EstimatorStage::Estimating => v_flex()
                .size_full()
                .p_5()
                .gap_3()
                .bg(theme.background)
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(Icon::new(LucideIcon::SquareTerminal).small())
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Live Exploration Console"),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{} event(s)", self.status_logs.len())),
                        ),
                )
                .child(self.render_terminal_box(None, cx))
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.sidebar)
                        .border_1()
                        .border_color(theme.border)
                        .child(Spinner::new().small().color(theme.primary))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Copilot is exploring the codebase..."),
                        ),
                )
                .into_any_element(),

            EstimatorStage::Completed(result) => {
                let result_for_copy = result.clone();
                let logs_chevron = if self.logs_expanded {
                    LucideIcon::ChevronDown
                } else {
                    LucideIcon::ChevronRight
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
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Estimation Complete"),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("copy-tshirt-result")
                                            .ghost()
                                            .small()
                                            .icon(
                                                Icon::new(if self.copied {
                                                    LucideIcon::Check
                                                } else {
                                                    LucideIcon::Copy
                                                })
                                                .small(),
                                            )
                                            .label(if self.copied {
                                                "Copied!"
                                            } else {
                                                "Copy Result"
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_result(&result_for_copy, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                v_flex()
                                    .size_full()
                                    .overflow_y_scrollbar()
                                    .p_6()
                                    .gap_5()
                                    .child(
                                        h_flex()
                                            .p_5()
                                            .gap_5()
                                            .items_center()
                                            .rounded(theme.radius_lg)
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.sidebar)
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .flex_shrink_0()
                                                    .size(px(90.))
                                                    .rounded(px(14.))
                                                    .bg(result.size.badge_color())
                                                    .text_color(rgb(0xffffff))
                                                    .text_3xl()
                                                    .font_weight(FontWeight::BOLD)
                                                    .child(result.size.label()),
                                            )
                                            .child(
                                                v_flex()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .text_color(theme.muted_foreground)
                                                            .child("ESTIMATED SIZE"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_lg()
                                                            .font_weight(FontWeight::SEMIBOLD)
                                                            .child(result.size.effort_hint()),
                                                    )
                                                    .when(
                                                        result.usage.input_tokens > 0
                                                            || result.usage.output_tokens > 0,
                                                        |col| {
                                                            col.child(
                                                                div()
                                                                    .pt_1()
                                                                    .text_xs()
                                                                    .text_color(
                                                                        theme.muted_foreground,
                                                                    )
                                                                    .child(format!(
                                                                        "Tokens: {} in / {} out{}",
                                                                        result.usage.input_tokens,
                                                                        result.usage.output_tokens,
                                                                        result
                                                                            .usage
                                                                            .model
                                                                            .as_deref()
                                                                            .map(|m| format!(
                                                                                " • {m}"
                                                                            ))
                                                                            .unwrap_or_default()
                                                                    )),
                                                            )
                                                        },
                                                    ),
                                            ),
                                    )
                                    .child(
                                        v_flex()
                                            .p_5()
                                            .gap_3()
                                            .rounded(theme.radius_lg)
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.background)
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.muted_foreground)
                                                    .child("REASONING & COMPLEXITY ANALYSIS"),
                                            )
                                            .child(
                                                TextView::markdown(
                                                    "tshirt-reasoning-md",
                                                    result.reasoning.clone(),
                                                )
                                                .selectable(true),
                                            ),
                                    )
                                    .when(!self.status_logs.is_empty(), |col| {
                                        col.child(
                                            v_flex()
                                                .gap_2()
                                                .child(
                                                    h_flex()
                                                        .id("toggle-tshirt-logs")
                                                        .items_center()
                                                        .gap_1p5()
                                                        .px_2()
                                                        .py_1p5()
                                                        .rounded(theme.radius)
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(theme.secondary_hover))
                                                        .on_click(cx.listener(
                                                            |this, _, _, cx| {
                                                                this.logs_expanded =
                                                                    !this.logs_expanded;
                                                                cx.notify();
                                                            },
                                                        ))
                                                        .child(Icon::new(logs_chevron).small())
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .font_weight(FontWeight::MEDIUM)
                                                                .text_color(theme.muted_foreground)
                                                                .child(format!(
                                                                    "Agent Activity Log ({})",
                                                                    self.status_logs.len()
                                                                )),
                                                        ),
                                                )
                                                .when(self.logs_expanded, |col| {
                                                    col.child(
                                                        self.render_terminal_box(Some(220.0), cx),
                                                    )
                                                }),
                                        )
                                    }),
                            ),
                    )
                    .into_any_element()
            }

            EstimatorStage::Error(err_msg) => {
                let red = gpui_kit::hsla(0.0, 0.78, 0.55, 1.0);
                v_flex()
                    .size_full()
                    .p_6()
                    .gap_4()
                    .bg(theme.background)
                    .child(
                        v_flex()
                            .p_5()
                            .gap_3()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(red.opacity(0.4))
                            .bg(red.opacity(0.06))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .text_color(red)
                                    .child(Icon::new(LucideIcon::CircleAlert).with_size(Size::Medium))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Estimation Failed"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.foreground)
                                    .child(err_msg.clone()),
                            )
                            .child(
                                h_flex()
                                    .pt_1()
                                    .gap_2()
                                    .child(
                                        Button::new("tshirt-error-retry")
                                            .primary()
                                            .small()
                                            .icon(Icon::new(LucideIcon::RefreshCw).small())
                                            .label("Retry")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.start_estimation(window, cx);
                                            })),
                                    ),
                            ),
                    )
                    .when(!self.status_logs.is_empty(), |col| {
                        col.child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child("ACTIVITY LOG"),
                        )
                        .child(self.render_terminal_box(None, cx))
                    })
                    .into_any_element()
            }
        }
    }
}

impl Render for TShirtEstimatorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.background;
        let fg = theme.foreground;
        let border = theme.border;
        let secondary = theme.secondary;
        let accent_fg = theme.accent_foreground;
        let muted_fg = theme.muted_foreground;
        let radius = theme.radius;
        let reset_state = self.reset_state(cx);
        let view_for_reset = cx.entity();

        v_flex()
            .size_full()
            .bg(bg)
            .text_color(fg)
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
                                    .child(Icon::new(ActiveView::TShirtEstimator.icon()).with_size(Size::Medium)),
                            )
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(ActiveView::TShirtEstimator.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::TShirtEstimator.subtitle()),
                                    ),
                            ),
                    )
                    .child(render_tool_reset_button(
                        "tshirt-header-reset",
                        reset_state,
                        move |window, cx| {
                            view_for_reset.update(cx, |this, cx| {
                                this.request_reset(window, cx);
                            });
                        },
                    )),
            )
            .child(
                div().flex_1().min_h_0().w_full().child(
                    h_resizable("tshirt-estimator-split")
                        .child(
                            resizable_panel()
                                .size(px(370.))
                                .size_range(px(290.)..px(540.))
                                .child(self.render_left_pane(cx)),
                        )
                        .child(resizable_panel().child(self.render_right_pane(cx))),
                ),
            )
    }
}
