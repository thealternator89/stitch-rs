use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, hsla, prelude::FluentBuilder as _, px,
};

use crate::app_state::{ActiveView, AppState, refresh_copilot_status};
use crate::features::settings::connectors::ConnectorsSettingsView;
use crate::features::settings::prompts::PromptsSettingsView;
use crate::infrastructure::git::GitService;
use crate::infrastructure::runtime::spawn_tokio;
use crate::types::{AppSettings, CopilotAuthStatus, ThemeMode};
use crate::ui::components::{ModelDropdown, ModelDropdownEvent, ModelDropdownMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Connectors,
    Copilot,
    Prompts,
    PrReviewer,
}

impl SettingsTab {
    const ALL: &'static [Self] = &[
        Self::General,
        Self::Connectors,
        Self::Copilot,
        Self::Prompts,
        Self::PrReviewer,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Connectors => "Connectors",
            Self::Copilot => "GitHub Copilot",
            Self::Prompts => "Prompts",
            Self::PrReviewer => "PR Reviewer",
        }
    }

    fn icon(self) -> LucideIcon {
        match self {
            Self::General => LucideIcon::SlidersHorizontal,
            Self::Connectors => LucideIcon::Plug,
            Self::Copilot => LucideIcon::Bot,
            Self::Prompts => LucideIcon::FileCode,
            Self::PrReviewer => LucideIcon::GitPullRequest,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Connectors => "settings-tab-connectors",
            Self::Copilot => "settings-tab-copilot",
            Self::Prompts => "settings-tab-prompts",
            Self::PrReviewer => "settings-tab-pr-reviewer",
        }
    }
}

#[derive(Debug, Clone)]
enum TokenFeedback {
    Saved(String),
    Error(String),
}

pub struct SettingsView {
    app_state: Entity<AppState>,
    git_service: Arc<GitService>,
    active_tab: SettingsTab,
    connectors_view: Entity<ConnectorsSettingsView>,
    prompts_view: Entity<PromptsSettingsView>,
    worktree_dir_input: Entity<InputState>,
    copilot_token_input: Entity<InputState>,
    default_model_dropdown: Entity<ModelDropdown>,
    token_feedback: Option<TokenFeedback>,
    orphaned_worktrees: Vec<PathBuf>,
    scanning_worktrees: bool,
    cleaning_worktrees: bool,
    worktree_cleanup_feedback: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        git_service: Arc<GitService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_worktree_dir_path = initial_settings.git_worktree_base_dir.clone();
        let initial_worktree_dir = initial_worktree_dir_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let initial_token = initial_settings.copilot_token.clone().unwrap_or_default();
        let initial_model = initial_settings.copilot_model.clone();

        let worktree_dir_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Select or enter a directory for Git worktrees...")
                .default_value(initial_worktree_dir)
        });

        let copilot_token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Optional GitHub token (stored securely in OS Keychain)")
                .masked(true)
                .default_value(initial_token)
        });

        let default_model_dropdown = cx.new(|cx| {
            ModelDropdown::new(
                app_state.clone(),
                initial_model,
                Vec::new(),
                ModelDropdownMode::SettingsDefault,
                window,
                cx,
            )
        });

        let state_sub = cx.observe(&app_state, |_, _, cx| cx.notify());

        let worktree_sub =
            cx.subscribe_in(&worktree_dir_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.save_worktree_dir_from_input(window, cx);
                }
            });

        let token_sub =
            cx.subscribe_in(&copilot_token_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_copilot_token(window, cx);
                }
            });

        let model_sub = cx.subscribe(&default_model_dropdown, |this, _, event, cx| {
            let ModelDropdownEvent::Changed(model_id) = event;
            let model_id = model_id.clone();
            this.app_state.update(cx, |state, cx| {
                state.settings.copilot_model = model_id;
                if let Err(err) = state.save_settings() {
                    eprintln!("Warning: failed to save default Copilot model: {err}");
                }
                cx.notify();
            });
        });

        let connectors_view = cx.new(|cx| {
            ConnectorsSettingsView::new(app_state.clone(), initial_settings, window, cx)
        });
        let prompts_view = cx.new(|cx| {
            PromptsSettingsView::new(app_state.clone(), initial_settings, window, cx)
        });

        let mut view = Self {
            app_state,
            git_service,
            active_tab: SettingsTab::default(),
            connectors_view,
            prompts_view,
            worktree_dir_input,
            copilot_token_input,
            default_model_dropdown,
            token_feedback: None,
            orphaned_worktrees: Vec::new(),
            scanning_worktrees: false,
            cleaning_worktrees: false,
            worktree_cleanup_feedback: None,
            _subscriptions: vec![state_sub, worktree_sub, token_sub, model_sub],
        };

        view.scan_worktrees(initial_worktree_dir_path, window, cx);
        view
    }

    pub fn set_tab(&mut self, tab: SettingsTab) {
        self.active_tab = tab;
    }

    fn scan_worktrees(
        &mut self,
        base_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = base_dir else {
            self.orphaned_worktrees.clear();
            self.scanning_worktrees = false;
            cx.notify();
            return;
        };

        self.scanning_worktrees = true;
        cx.notify();

        let git_service = self.git_service.clone();
        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move { git_service.check_worktrees(&dir) }).await;
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.scanning_worktrees = false;
                        if let Ok(check) = res {
                            this.orphaned_worktrees = check.directories;
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn confirm_and_clean_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.orphaned_worktrees.len();
        if count == 0 {
            return;
        }
        let Some(base_dir) = self.app_state.read(cx).settings.git_worktree_base_dir.clone() else {
            return;
        };

        let view = cx.entity();
        let git_service = self.git_service.clone();
        let dir_label = base_dir.to_string_lossy().to_string();

        window.open_dialog(cx, move |dialog, _window, cx| {
            let theme = cx.theme();
            let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
            let view_for_confirm = view.clone();
            let git_for_confirm = git_service.clone();
            let dir_for_confirm = base_dir.clone();

            dialog
                .w(px(460.))
                .overlay_closable(true)
                .close_button(true)
                .title(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(LucideIcon::TriangleAlert)
                                .with_size(Size::Medium)
                                .text_color(amber),
                        )
                        .child("Clean Up Orphaned Worktrees"),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(format!(
                            "Remove {count} orphaned worktree directory(s) from:"
                        ))
                        .child(
                            div()
                                .p_2p5()
                                .rounded(theme.radius)
                                .bg(theme.sidebar)
                                .border_1()
                                .border_color(theme.border)
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .child(dir_label.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Only inactive Stitch worktrees not attached to running tasks will be removed."),
                        ),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("worktree-cleanup-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("worktree-cleanup-confirm")
                                .danger()
                                .icon(Icon::new(LucideIcon::Trash).small())
                                .label("Clean Up")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let view = view_for_confirm.clone();
                                    let git = git_for_confirm.clone();
                                    let dir = dir_for_confirm.clone();
                                    view.update(cx, |this, cx| {
                                        this.cleaning_worktrees = true;
                                        this.worktree_cleanup_feedback = None;
                                        cx.notify();
                                    });
                                    window
                                        .spawn(cx, async move |cx| {
                                            let res = spawn_tokio(async move {
                                                git.clean_worktrees(&dir).await
                                            })
                                            .await;
                                            let _ = cx.update(|window, cx| {
                                                view.update(cx, |this, cx| {
                                                    this.cleaning_worktrees = false;
                                                    match res {
                                                        Ok(clean) if clean.errors.is_empty() => {
                                                            this.worktree_cleanup_feedback = Some(format!(
                                                                "Cleaned up {} directory(s).",
                                                                clean.removed
                                                            ));
                                                        }
                                                        Ok(clean) => {
                                                            this.worktree_cleanup_feedback = Some(format!(
                                                                "Removed {} directory(s) with {} error(s): {}",
                                                                clean.removed,
                                                                clean.errors.len(),
                                                                clean.errors.join("; ")
                                                            ));
                                                        }
                                                        Err(err) => {
                                                            this.worktree_cleanup_feedback = Some(format!(
                                                                "Cleanup failed: {err}"
                                                            ));
                                                        }
                                                    }
                                                    let current_dir = this
                                                        .app_state
                                                        .read(cx)
                                                        .settings
                                                        .git_worktree_base_dir
                                                        .clone();
                                                    this.scan_worktrees(current_dir, window, cx);
                                                });
                                            });
                                        })
                                        .detach();
                                }),
                        ),
                )
        });
    }

    fn save_worktree_dir_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.worktree_dir_input.read(cx).value().to_string();
        let trimmed = raw.trim();
        let next_dir = if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        };

        let changed = self.app_state.update(cx, |state, cx| {
            if state.settings.git_worktree_base_dir != next_dir {
                state.settings.git_worktree_base_dir = next_dir.clone();
                if let Err(err) = state.save_settings() {
                    eprintln!("Warning: failed to save worktree directory: {err}");
                }
                cx.notify();
                true
            } else {
                false
            }
        });

        if changed {
            self.worktree_cleanup_feedback = None;
            self.scan_worktrees(next_dir, window, cx);
        }
    }

    fn browse_worktree_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Worktree Base Directory".into()),
        });

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                if let Ok(Ok(Some(paths))) = receiver.await {
                    if let Some(selected) = paths.into_iter().next() {
                        let _ = cx.update(|window, cx| {
                            view.update(cx, |this, cx| {
                                let path_str = selected.to_string_lossy().to_string();
                                this.worktree_dir_input.update(cx, |input, cx| {
                                    input.set_value(path_str, window, cx);
                                });
                                this.app_state.update(cx, |state, cx| {
                                    state.settings.git_worktree_base_dir = Some(selected.clone());
                                    if let Err(err) = state.save_settings() {
                                        eprintln!(
                                            "Warning: failed to save worktree directory: {err}"
                                        );
                                    }
                                    cx.notify();
                                });
                                this.worktree_cleanup_feedback = None;
                                this.scan_worktrees(Some(selected), window, cx);
                            });
                        });
                    }
                }
            })
            .detach();
    }

    fn clear_worktree_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.worktree_dir_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        self.app_state.update(cx, |state, cx| {
            state.settings.git_worktree_base_dir = None;
            if let Err(err) = state.save_settings() {
                eprintln!("Warning: failed to clear worktree directory: {err}");
            }
            cx.notify();
        });
        self.worktree_cleanup_feedback = None;
        self.scan_worktrees(None, window, cx);
    }

    fn save_copilot_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.copilot_token_input.read(cx).value().to_string();
        let trimmed = raw.trim().to_string();
        let next_token = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.clone())
        };

        if raw != trimmed {
            self.copilot_token_input.update(cx, |input, cx| {
                input.set_value(trimmed.clone(), window, cx);
            });
        }

        let save_res = self.app_state.update(cx, |state, cx| {
            state.settings.copilot_token = next_token.clone();
            let res = state.save_settings();
            cx.notify();
            res
        });

        match save_res {
            Ok(()) => {
                self.token_feedback = Some(TokenFeedback::Saved(if next_token.is_some() {
                    "Copilot token saved securely to OS Keychain.".to_string()
                } else {
                    "Copilot token cleared from OS Keychain.".to_string()
                }));
                refresh_copilot_status(self.app_state.clone(), true, false, window, cx);
            }
            Err(err) => {
                self.token_feedback = Some(TokenFeedback::Error(format!(
                    "Failed to save token to OS Keychain: {err}"
                )));
            }
        }
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .w(px(208.))
            .h_full()
            .flex_shrink_0()
            .p_3()
            .gap_1()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_2p5()
                    .py_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("SETTINGS"),
            )
            .children(SettingsTab::ALL.iter().copied().map(|tab| {
                let is_active = self.active_tab == tab;
                let bg = if is_active {
                    theme.secondary
                } else {
                    theme.transparent
                };
                let fg = if is_active {
                    theme.foreground
                } else {
                    theme.muted_foreground
                };
                let weight = if is_active {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                };

                h_flex()
                    .id(tab.id())
                    .items_center()
                    .gap_2p5()
                    .px_2p5()
                    .py_2()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .bg(bg)
                    .text_sm()
                    .font_weight(weight)
                    .text_color(fg)
                    .hover(|s| s.bg(theme.secondary_hover).text_color(theme.foreground))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_tab = tab;
                        cx.notify();
                    }))
                    .child(Icon::new(tab.icon()).small())
                    .child(tab.label())
            }))
    }

    fn render_general_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.app_state.read(cx);
        let current_mode = state.settings.theme;
        let worktree_enabled = state.settings.git_worktree_enabled;
        let has_worktree_dir = state.settings.git_worktree_base_dir.is_some();

        let mode_button = |id: &'static str, mode: ThemeMode, icon: LucideIcon| {
            let is_active = current_mode == mode;
            let app_state = self.app_state.clone();
            let btn = Button::new(id)
                .icon(Icon::new(icon).small())
                .label(mode.label())
                .on_click(move |_, window, cx| {
                    app_state.update(cx, |state, cx| {
                        state.set_theme_mode(mode, window, cx);
                        cx.notify();
                    });
                });
            if is_active { btn.primary() } else { btn.ghost() }
        };

        let app_state_for_switch = self.app_state.clone();

        v_flex()
            .gap_5()
            .max_w(px(760.))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("General"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Appearance preferences and isolated Git worktree defaults."),
                    ),
            )
            // Interface Theme Card
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Interface Theme"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Choose Light, Dark, or Auto to track your system appearance."),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(mode_button("theme-auto", ThemeMode::Auto, LucideIcon::Monitor))
                            .child(mode_button("theme-light", ThemeMode::Light, LucideIcon::Sun))
                            .child(mode_button("theme-dark", ThemeMode::Dark, LucideIcon::Moon)),
                    ),
            )
            // Git Worktree Support Card
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Enable Git Worktree Support"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Run T-Shirt Estimation, Story Elaboration, and PR Reviews in isolated temporary Git worktrees so your working directory stays clean."),
                                    ),
                            )
                            .child(
                                Switch::new("worktree-enabled-switch")
                                    .checked(worktree_enabled)
                                    .on_change(move |checked, _, cx| {
                                        app_state_for_switch.update(cx, |state, cx| {
                                            state.settings.git_worktree_enabled = *checked;
                                            if let Err(err) = state.save_settings() {
                                                eprintln!(
                                                    "Warning: failed to save git_worktree_enabled: {err}"
                                                );
                                            }
                                            cx.notify();
                                        });
                                    }),
                            ),
                    )
                    .child(div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("Worktree Base Directory"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Directory where temporary detached worktrees are created when Git Worktree Support is enabled."),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(Input::new(&self.worktree_dir_input)),
                                    )
                                    .child(
                                        Button::new("browse-worktree-dir")
                                            .icon(Icon::new(LucideIcon::FolderOpen).small())
                                            .label("Browse...")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.browse_worktree_dir(window, cx);
                                            })),
                                    )
                                    .when(has_worktree_dir, |row| {
                                        row.child(
                                            Button::new("clear-worktree-dir")
                                                .ghost()
                                                .icon(Icon::new(LucideIcon::X).small())
                                                .label("Clear")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.clear_worktree_dir(window, cx);
                                                })),
                                        )
                                    }),
                            )
                            .child(
                                v_flex()
                                    .pt_2()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_3()
                                            .p_3()
                                            .rounded(theme.radius)
                                            .bg(theme.background)
                                            .border_1()
                                            .border_color(theme.border)
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .when(self.scanning_worktrees, |row| {
                                                        row.child(Spinner::new().xsmall())
                                                    })
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(theme.muted_foreground)
                                                            .child(if !has_worktree_dir {
                                                                "Set a Worktree Base Directory to scan for orphaned worktrees.".to_string()
                                                            } else if self.scanning_worktrees {
                                                                "Scanning for orphaned worktrees...".to_string()
                                                            } else if self.orphaned_worktrees.is_empty() {
                                                                "No orphaned worktrees found.".to_string()
                                                            } else {
                                                                format!(
                                                                    "Found {} directory(s) to clean up",
                                                                    self.orphaned_worktrees.len()
                                                                )
                                                            }),
                                                    ),
                                            )
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .child(
                                                        Button::new("rescan-worktrees")
                                                            .ghost()
                                                            .small()
                                                            .disabled(
                                                                !has_worktree_dir
                                                                    || self.scanning_worktrees
                                                                    || self.cleaning_worktrees,
                                                            )
                                                            .icon(
                                                                Icon::new(LucideIcon::RefreshCw)
                                                                    .xsmall(),
                                                            )
                                                            .label("Rescan")
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    let dir = this
                                                                        .app_state
                                                                        .read(cx)
                                                                        .settings
                                                                        .git_worktree_base_dir
                                                                        .clone();
                                                                    this.scan_worktrees(
                                                                        dir, window, cx,
                                                                    );
                                                                },
                                                            )),
                                                    )
                                                    .child(
                                                        Button::new("clean-worktrees")
                                                            .small()
                                                            .disabled(
                                                                self.orphaned_worktrees.is_empty()
                                                                    || self.scanning_worktrees
                                                                    || self.cleaning_worktrees,
                                                            )
                                                            .loading(self.cleaning_worktrees)
                                                            .icon(
                                                                Icon::new(LucideIcon::Trash)
                                                                    .xsmall(),
                                                            )
                                                            .label("Clean Up")
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.confirm_and_clean_worktrees(
                                                                        window, cx,
                                                                    );
                                                                },
                                                            )),
                                                    ),
                                            ),
                                    )
                                    .when_some(
                                        self.worktree_cleanup_feedback.clone(),
                                        |col, msg| {
                                            col.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(msg),
                                            )
                                        },
                                    ),
                            ),
                    ),
            )
    }

    fn render_copilot_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.app_state.read(cx);
        let auth_status = state.copilot_auth.clone();
        let models = state.copilot_models.clone();
        let models_loading = state.models_loading;
        let models_error = state.models_error.clone();
        let mono_font = theme.mono_font_family.clone();

        let is_checking = matches!(auth_status, CopilotAuthStatus::Checking);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        let (dot_color, badge_label, auth_info, auth_error) = match &auth_status {
            CopilotAuthStatus::Checking => (None, "Checking Auth...".to_string(), None, None),
            CopilotAuthStatus::Connected(info) => {
                let label = info
                    .login
                    .as_deref()
                    .map(|l| format!("Connected (@{l})"))
                    .unwrap_or_else(|| "Connected".to_string());
                (Some(green), label, Some(info.clone()), None)
            }
            CopilotAuthStatus::Disconnected { info, error } => {
                (Some(amber), "Disconnected".to_string(), info.clone(), error.clone())
            }
        };

        let app_state_for_auth = self.app_state.clone();
        let app_state_for_models = self.app_state.clone();

        let metadata_row = |label: &'static str, value: String| {
            h_flex()
                .items_center()
                .justify_between()
                .py_1()
                .text_xs()
                .child(div().text_color(theme.muted_foreground).child(label))
                .child(
                    div()
                        .font_family(mono_font.clone())
                        .text_color(theme.foreground)
                        .child(value),
                )
        };

        v_flex()
            .gap_5()
            .max_w(px(820.))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("GitHub Copilot"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Manage Copilot authentication, personal token overrides, default model selection, and available models."),
                    ),
            )
            // Card 1: Authentication Status
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Authentication Status"),
                                    )
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_1p5()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .bg(theme.secondary)
                                            .border_1()
                                            .border_color(theme.border)
                                            .when(is_checking, |this| {
                                                this.child(Spinner::new().xsmall())
                                            })
                                            .when_some(dot_color, |this, color| {
                                                this.child(div().size(px(8.)).rounded_full().bg(color))
                                            })
                                            .child(div().text_xs().child(badge_label)),
                                    ),
                            )
                            .child(
                                Button::new("settings-check-auth")
                                    .primary()
                                    .small()
                                    .disabled(is_checking)
                                    .loading(is_checking)
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label("Check Auth Status")
                                    .on_click(move |_, window, cx| {
                                        refresh_copilot_status(
                                            app_state_for_auth.clone(),
                                            true,
                                            false,
                                            window,
                                            cx,
                                        );
                                    }),
                            ),
                    )
                    .when_some(auth_info.clone(), |card, info| {
                        let has_meta = info.login.is_some()
                            || info.auth_type.is_some()
                            || info.host.is_some()
                            || info.cli_version.is_some()
                            || info.protocol_version.is_some();
                        card.when(has_meta, |card| {
                            card.child(
                                v_flex()
                                    .p_3()
                                    .gap_1()
                                    .rounded(theme.radius)
                                    .bg(theme.background)
                                    .border_1()
                                    .border_color(theme.border)
                                    .when_some(info.login.clone(), |list, login| {
                                        list.child(metadata_row("Account", format!("@{login}")))
                                    })
                                    .when_some(info.auth_type.clone(), |list, auth_type| {
                                        list.child(metadata_row("Auth Type", auth_type))
                                    })
                                    .when_some(info.host.clone(), |list, host| {
                                        list.child(metadata_row("Host", host))
                                    })
                                    .when_some(info.cli_version.clone(), |list, ver| {
                                        list.child(metadata_row("CLI Version", ver))
                                    })
                                    .when_some(info.protocol_version.clone(), |list, proto| {
                                        list.child(metadata_row("Protocol Version", proto))
                                    }),
                            )
                        })
                    })
                    .when(
                        !is_checking && matches!(auth_status, CopilotAuthStatus::Disconnected { .. }),
                        |card| {
                            let detail = auth_error.clone().or_else(|| {
                                auth_info.as_ref().and_then(|i| i.status_message.clone())
                            });
                            card.child(
                                v_flex()
                                    .p_3()
                                    .gap_1p5()
                                    .rounded(theme.radius)
                                    .bg(theme.background)
                                    .border_1()
                                    .border_color(theme.border)
                                    .when_some(detail, |box_el, msg| {
                                        box_el.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.foreground)
                                                .child(msg),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Run `copilot auth signin` or `gh auth login` in your terminal, or save a personal Copilot token below."),
                                    ),
                            )
                        },
                    ),
            )
            // Card 2: Copilot Token & Default Model
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Copilot Token (Optional Override)"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Leave blank to use your signed-in Copilot / GitHub CLI credentials. Tokens are stored in your OS Keychain and never written to settings.json."),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Input::new(&self.copilot_token_input)
                                                .mask_toggle()
                                                .cleanable(true),
                                        ),
                                    )
                                    .child(
                                        Button::new("save-copilot-token")
                                            .primary()
                                            .icon(Icon::new(LucideIcon::KeyRound).small())
                                            .label("Save Token")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.save_copilot_token(window, cx);
                                            })),
                                    ),
                            )
                            .when_some(self.token_feedback.clone(), |col, feedback| {
                                let (msg, color, icon) = match feedback {
                                    TokenFeedback::Saved(m) => (m, green, LucideIcon::Check),
                                    TokenFeedback::Error(m) => (m, red, LucideIcon::TriangleAlert),
                                };
                                col.child(
                                    h_flex()
                                        .items_center()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(color)
                                        .child(Icon::new(icon).xsmall())
                                        .child(msg),
                                )
                            }),
                    )
                    .child(div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Default Copilot Model"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Used across Stitch tools unless overridden per tool run."),
                            )
                            .child(self.default_model_dropdown.clone()),
                    ),
            )
            // Card 3: Available Models Table
            .child(
                v_flex()
                    .p_5()
                    .gap_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Available Models"),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .bg(theme.secondary)
                                            .border_1()
                                            .border_color(theme.border)
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(models.len().to_string()),
                                    ),
                            )
                            .child(
                                Button::new("refresh-copilot-models")
                                    .ghost()
                                    .small()
                                    .disabled(models_loading)
                                    .loading(models_loading)
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label("Refresh")
                                    .on_click(move |_, window, cx| {
                                        refresh_copilot_status(
                                            app_state_for_models.clone(),
                                            true,
                                            false,
                                            window,
                                            cx,
                                        );
                                    }),
                            ),
                    )
                    .child(if models_loading && models.is_empty() {
                        h_flex()
                            .py_6()
                            .justify_center()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Spinner::new().small())
                            .child("Loading available Copilot models...")
                            .into_any_element()
                    } else if models.is_empty() {
                        v_flex()
                            .py_6()
                            .gap_1()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("No models available.")
                            .when_some(models_error, |col, err| {
                                col.child(div().text_color(amber).child(err))
                            })
                            .into_any_element()
                    } else {
                        v_flex()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .px_3()
                                    .py_2()
                                    .bg(theme.secondary)
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(div().w(px(240.)).child("Name"))
                                    .child(div().flex_1().min_w_0().child("Model Key / ID"))
                                    .child(div().w(px(120.)).text_right().child("Multiplier")),
                            )
                            .children(models.iter().enumerate().map(|(idx, model)| {
                                h_flex()
                                    .px_3()
                                    .py_2()
                                    .items_center()
                                    .text_xs()
                                    .when(idx > 0, |row| {
                                        row.border_t_1().border_color(theme.border)
                                    })
                                    .child(
                                        div()
                                            .w(px(240.))
                                            .pr_2()
                                            .font_weight(FontWeight::MEDIUM)
                                            .truncate()
                                            .child(model.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .pr_2()
                                            .font_family(mono_font.clone())
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(model.id.clone()),
                                    )
                                    .child(
                                        h_flex().w(px(120.)).justify_end().child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(theme.radius)
                                                .bg(theme.secondary)
                                                .border_1()
                                                .border_color(theme.border)
                                                .font_family(mono_font.clone())
                                                .child(model.multiplier_label()),
                                        ),
                                    )
                            }))
                            .into_any_element()
                    }),
            )
    }

    fn render_placeholder_tab(
        &self,
        title: &'static str,
        phase: &'static str,
        description: &'static str,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .gap_4()
            .max_w(px(640.))
            .p_6()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .px_2p5()
                            .py_1()
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(phase),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(description),
            )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let header_border = theme.border;
        let header_bg = theme.background;
        let header_fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let secondary_bg = theme.secondary;
        let accent_fg = theme.accent_foreground;
        let radius = theme.radius;

        let active_content = match self.active_tab {
            SettingsTab::General => self.render_general_tab(cx).into_any_element(),
            SettingsTab::Connectors => self.connectors_view.clone().into_any_element(),
            SettingsTab::Copilot => self.render_copilot_tab(cx).into_any_element(),
            SettingsTab::Prompts => self.prompts_view.clone().into_any_element(),
            SettingsTab::PrReviewer => self
                .render_placeholder_tab(
                    "PR Reviewer Settings",
                    "Scheduled for Phase 10",
                    "Configure up to 5 custom review personas and default parallel review worker concurrency.",
                    cx,
                )
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(header_bg)
            .text_color(header_fg)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(header_border)
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
                                    .bg(secondary_bg)
                                    .text_color(accent_fg)
                                    .child(Icon::new(ActiveView::Settings.icon()).with_size(Size::Medium)),
                            )
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(ActiveView::Settings.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::Settings.subtitle()),
                                    ),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(
                                v_flex()
                                    .size_full()
                                    .overflow_y_scrollbar()
                                    .p_6()
                                    .child(active_content),
                            ),
                    ),
            )
    }
}
