use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{Theme, WindowExt as _};
use gpui_kit::{App, AppContext as _, Entity, Window};

use crate::features::{
    history::UsageHistoryView,
    pr_reviewer::PrReviewerView,
    settings::{SettingsTab, SettingsView},
    story_elaborator::StoryElaboratorView,
    story_writer::StoryWriterView,
    test_case_writer::TestCaseWriterView,
    tshirt_estimator::TShirtEstimatorView,
};
use crate::infrastructure::copilot::CopilotService;
use crate::infrastructure::database::UsageDatabase;
use crate::infrastructure::git::GitService;
use crate::infrastructure::runtime::spawn_tokio;
use crate::infrastructure::settings::SettingsStore;
use crate::types::{
    AppSettings, CopilotAuthInfo, CopilotAuthStatus, CopilotModelInfo, CopilotUsage,
    LifecycleGroup, ThemeMode,
};
use crate::ui::components::UsageToastData;
use crate::ui::components::auth_modal::open_copilot_auth_modal;

/// Identifies which singleton tool view is currently active in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ActiveView {
    #[default]
    TShirtEstimator,
    StoryWriter,
    TestCaseWriter,
    StoryElaborator,
    PrReviewer,
    UsageHistory,
    Settings,
}

impl ActiveView {
    pub const LIFECYCLE_GROUPS: &'static [(LifecycleGroup, &'static [ActiveView])] = &[
        (LifecycleGroup::Ideation, &[ActiveView::TShirtEstimator]),
        (
            LifecycleGroup::SolutionDesign,
            &[ActiveView::StoryWriter, ActiveView::TestCaseWriter],
        ),
        (LifecycleGroup::Build, &[ActiveView::StoryElaborator]),
        (LifecycleGroup::Review, &[ActiveView::PrReviewer]),
    ];

    pub const UTILITY_VIEWS: &'static [ActiveView] =
        &[ActiveView::UsageHistory, ActiveView::Settings];

    pub fn label(self) -> &'static str {
        match self {
            Self::TShirtEstimator => "T-Shirt Estimator",
            Self::StoryWriter => "Story Writer",
            Self::TestCaseWriter => "Test Case Writer",
            Self::StoryElaborator => "Story Elaborator",
            Self::PrReviewer => "PR Reviewer",
            Self::UsageHistory => "Usage History",
            Self::Settings => "Settings",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::TShirtEstimator => {
                "Estimate effort size (XS–XL) and implementation complexity from a local codebase."
            }
            Self::StoryWriter => {
                "Generate structured user stories from Confluence specifications and push to your tracker."
            }
            Self::TestCaseWriter => {
                "Generate interactive test scenarios from tickets and export as Markdown or tasks."
            }
            Self::StoryElaborator => {
                "Run a multi-turn Q&A with Copilot to draft and refine an implementation plan."
            }
            Self::PrReviewer => {
                "Run parallel multi-phase local code reviews with an AI Critic evaluation pass."
            }
            Self::UsageHistory => {
                "Inspect 30-day token telemetry, cache savings, and per-phase LLM usage."
            }
            Self::Settings => {
                "Configure theme, Git worktrees, connectors, GitHub Copilot, and custom prompts."
            }
        }
    }

    pub fn icon(self) -> LucideIcon {
        match self {
            Self::TShirtEstimator => LucideIcon::Shirt,
            Self::StoryWriter => LucideIcon::FileText,
            Self::TestCaseWriter => LucideIcon::ClipboardCheck,
            Self::StoryElaborator => LucideIcon::MessageSquareText,
            Self::PrReviewer => LucideIcon::GitPullRequest,
            Self::UsageHistory => LucideIcon::ChartBar,
            Self::Settings => LucideIcon::Settings,
        }
    }
}

/// Global application state holding settings, Copilot status/models, and one singleton `Entity<T>` per view.
///
/// Switching `active_view` swaps which entity renders in the workspace without
/// dropping any background view or its in-flight async tasks.
pub struct AppState {
    pub active_view: ActiveView,
    pub settings_store: Arc<SettingsStore>,
    pub settings: AppSettings,
    pub copilot_service: Arc<CopilotService>,
    pub git_service: Arc<GitService>,
    pub usage_db: Arc<UsageDatabase>,
    pub copilot_auth: CopilotAuthStatus,
    pub copilot_models: Vec<CopilotModelInfo>,
    pub models_loading: bool,
    pub models_error: Option<String>,
    pub startup_auth_modal_shown: bool,
    pub auth_modal_open: bool,
    pub active_tasks: HashMap<ActiveView, String>,
    pub task_progress: HashMap<ActiveView, f32>,
    pub active_usage_toast: Option<UsageToastData>,
    pub version_updated_from: Option<String>,

    pub tshirt_estimator: Entity<TShirtEstimatorView>,
    pub story_writer: Entity<StoryWriterView>,
    pub test_case_writer: Entity<TestCaseWriterView>,
    pub story_elaborator: Entity<StoryElaboratorView>,
    pub pr_reviewer: Entity<PrReviewerView>,
    pub usage_history: Entity<UsageHistoryView>,
    pub settings_view: Entity<SettingsView>,
}

impl AppState {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let app_state_handle = cx.entity();

            let settings_store = Arc::new(SettingsStore::new());
            let mut settings = settings_store.load();
            let copilot_service = Arc::new(CopilotService::new());
            let git_service = Arc::new(GitService::new());
            let usage_db = Arc::new(
                UsageDatabase::new().unwrap_or_else(|err| {
                    eprintln!(
                        "Warning: failed to open history.db ({err}); falling back to in-memory SQLite."
                    );
                    UsageDatabase::open(":memory:").expect("open in-memory SQLite fallback")
                }),
            );

            // Check & update version status on startup
            let current_version = env!("CARGO_PKG_VERSION").to_string();
            let version_updated_from = match settings.last_run_version.as_deref() {
                Some(prev) if prev != current_version => Some(prev.to_string()),
                _ => None,
            };
            if settings.last_run_version.as_deref() != Some(current_version.as_str()) {
                settings.last_run_version = Some(current_version);
                let _ = settings_store.save(&mut settings);
            }

            apply_theme(settings.theme, Some(window), cx);

            let tshirt_estimator = cx.new(|cx| {
                TShirtEstimatorView::new(
                    app_state_handle.clone(),
                    settings.copilot_model.clone(),
                    window,
                    cx,
                )
            });
            let story_writer = cx.new(|cx| {
                StoryWriterView::new(app_state_handle.clone(), &settings, window, cx)
            });
            let test_case_writer = cx.new(|cx| {
                TestCaseWriterView::new(app_state_handle.clone(), &settings, window, cx)
            });
            let story_elaborator = cx.new(|cx| {
                StoryElaboratorView::new(
                    app_state_handle.clone(),
                    &settings,
                    copilot_service.clone(),
                    git_service.clone(),
                    window,
                    cx,
                )
            });
            let pr_reviewer = cx.new(|cx| {
                PrReviewerView::new(
                    app_state_handle.clone(),
                    &settings,
                    copilot_service.clone(),
                    git_service.clone(),
                    window,
                    cx,
                )
            });
            let usage_history =
                cx.new(|cx| UsageHistoryView::new(usage_db.clone(), window, cx));
            let settings_view = cx.new(|cx| {
                SettingsView::new(app_state_handle, &settings, git_service.clone(), window, cx)
            });

            // 2-second delayed startup retention cleanup (pruning records older than 30 days)
            let db_for_cleanup = usage_db.clone();
            let history_for_cleanup = usage_history.clone();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let pruned = spawn_tokio(async move { db_for_cleanup.prune_expired_sessions() })
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(0);
                if pruned > 0 {
                    let _ = history_for_cleanup.update(cx, |view, cx| {
                        view.refresh_from_db(cx);
                    });
                }
            })
            .detach();

            Self {
                active_view: ActiveView::default(),
                settings_store,
                settings,
                copilot_service,
                git_service,
                usage_db,
                copilot_auth: CopilotAuthStatus::Checking,
                copilot_models: Vec::new(),
                models_loading: false,
                models_error: None,
                startup_auth_modal_shown: false,
                auth_modal_open: false,
                active_tasks: HashMap::new(),
                task_progress: HashMap::new(),
                active_usage_toast: None,
                version_updated_from,
                tshirt_estimator,
                story_writer,
                test_case_writer,
                story_elaborator,
                pr_reviewer,
                usage_history,
                settings_view,
            }
        })
    }

    pub fn theme_mode(&self) -> ThemeMode {
        self.settings.theme
    }

    pub fn set_active_view(&mut self, view: ActiveView) {
        self.active_view = view;
    }

    pub fn navigate_to_settings_tab(&mut self, tab: SettingsTab, cx: &mut App) {
        self.active_view = ActiveView::Settings;
        self.settings_view.update(cx, |view, cx| {
            view.set_tab(tab);
            cx.notify();
        });
    }

    pub fn save_settings(&mut self) -> anyhow::Result<()> {
        self.settings_store.save(&mut self.settings)
    }

    pub fn set_theme_mode(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut App) {
        self.settings.theme = mode;
        apply_theme(mode, Some(window), cx);
        if let Err(err) = self.save_settings() {
            eprintln!("Warning: failed to save theme setting: {err}");
        }
    }

    pub fn cycle_theme_mode(&mut self, window: &mut Window, cx: &mut App) {
        let next = self.settings.theme.next();
        self.set_theme_mode(next, window, cx);
    }

    pub fn set_task_status(&mut self, view: ActiveView, status: Option<String>) {
        match status {
            Some(text) => {
                self.active_tasks.insert(view, text);
            }
            None => {
                self.active_tasks.remove(&view);
                self.task_progress.remove(&view);
            }
        }
    }

    pub fn set_task_progress(&mut self, view: ActiveView, progress: Option<f32>) {
        match progress {
            Some(p) => {
                self.task_progress.insert(view, p.clamp(0.0, 1.0));
            }
            None => {
                self.task_progress.remove(&view);
            }
        }
    }

    pub fn is_task_running(&self, view: ActiveView) -> bool {
        self.active_tasks.contains_key(&view)
    }

    /// Returns the active window progress bar fraction (`0.0..=1.0`) if a task is running.
    pub fn active_window_progress(&self) -> Option<f32> {
        if self.active_tasks.is_empty() {
            return None;
        }
        if let Some(&p) = self.task_progress.get(&self.active_view) {
            return Some(p);
        }
        if let Some(&p) = self.task_progress.values().next() {
            return Some(p);
        }
        // Indeterminate task running: show a 35% active progress bar indicator
        Some(0.35)
    }

    /// Looks up the billing multiplier for `model_id` from the cached Copilot models list.
    pub fn model_billing_multiplier(&self, model_id: &str) -> Option<f64> {
        let trimmed = model_id.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
            return Some(1.0);
        }
        self.copilot_models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(trimmed) || m.name.eq_ignore_ascii_case(trimmed))
            .and_then(|m| m.billing_multiplier)
            .or(Some(1.0))
    }

    /// Persists a completed tool session and its [`CopilotUsage`] to SQLite, displays the floating
    /// **`UsageStatsToast`**, refreshes [`UsageHistoryView`], and returns the `usage_sessions.id`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_completed_tool_session(
        &mut self,
        tool_name: &str,
        context_reference: Option<&str>,
        ai_output: Option<&str>,
        default_label: &str,
        default_model: &str,
        usage: &CopilotUsage,
        cx: &mut App,
    ) -> Option<i64> {
        let multiplier = self.model_billing_multiplier(default_model);
        let session_id = match self.usage_db.log_completed_session(
            tool_name,
            context_reference,
            ai_output,
            default_label,
            default_model,
            usage,
            multiplier,
        ) {
            Ok(id) => Some(id),
            Err(err) => {
                eprintln!("Warning: failed to log usage session for {tool_name}: {err}");
                None
            }
        };

        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            self.active_usage_toast = Some(UsageToastData {
                tool_name: tool_name.to_string(),
                context_reference: context_reference.map(str::to_string),
                default_label: default_label.to_string(),
                default_model: default_model.to_string(),
                default_multiplier: multiplier,
                usage: usage.clone(),
            });
        }

        self.usage_history.update(cx, |view, cx| {
            view.refresh_from_db(cx);
        });

        session_id
    }

    /// Creates or updates a multi-turn session in SQLite (used by Story Elaborator across Q&A and plan refinements).
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_tool_session_usage(
        &mut self,
        existing_session_id: Option<i64>,
        tool_name: &str,
        context_reference: Option<&str>,
        ai_output: Option<&str>,
        default_label: &str,
        default_model: &str,
        usage: &CopilotUsage,
        show_toast: bool,
        cx: &mut App,
    ) -> Option<i64> {
        let multiplier = self.model_billing_multiplier(default_model);
        let session_id = match existing_session_id {
            Some(id) => {
                if let Some(out) = ai_output {
                    let _ = self.usage_db.update_session_output(id, out);
                }
                if let Err(err) = self.usage_db.replace_session_copilot_usage(
                    id,
                    default_label,
                    default_model,
                    usage,
                    multiplier,
                ) {
                    eprintln!("Warning: failed to update session {id} usage: {err}");
                }
                Some(id)
            }
            None => self
                .usage_db
                .log_completed_session(
                    tool_name,
                    context_reference,
                    ai_output,
                    default_label,
                    default_model,
                    usage,
                    multiplier,
                )
                .ok(),
        };

        if show_toast && (usage.input_tokens > 0 || usage.output_tokens > 0) {
            self.active_usage_toast = Some(UsageToastData {
                tool_name: tool_name.to_string(),
                context_reference: context_reference.map(str::to_string),
                default_label: default_label.to_string(),
                default_model: default_model.to_string(),
                default_multiplier: multiplier,
                usage: usage.clone(),
            });
        }

        self.usage_history.update(cx, |view, cx| {
            view.refresh_from_db(cx);
        });

        session_id
    }

    pub fn record_push_for_session(&mut self, session_id: i64, text: &str, cx: &mut App) {
        if let Err(err) = self.usage_db.record_push(session_id, text) {
            eprintln!("Warning: failed to record push on session {session_id}: {err}");
        }
        self.usage_history.update(cx, |view, cx| {
            view.refresh_from_db(cx);
        });
    }

    pub fn increment_story_push_for_session(&mut self, session_id: i64, cx: &mut App) {
        if let Err(err) = self.usage_db.increment_pushed_story_count(session_id) {
            eprintln!("Warning: failed to increment pushed story count on session {session_id}: {err}");
        }
        self.usage_history.update(cx, |view, cx| {
            view.refresh_from_db(cx);
        });
    }

    pub fn increment_pr_comment_push_for_session(&mut self, session_id: i64, cx: &mut App) {
        if let Err(err) = self.usage_db.increment_pushed_comment_count(session_id) {
            eprintln!("Warning: failed to increment pushed PR comment count on session {session_id}: {err}");
        }
        self.usage_history.update(cx, |view, cx| {
            view.refresh_from_db(cx);
        });
    }

    pub fn dismiss_usage_toast(&mut self) {
        self.active_usage_toast = None;
    }

    pub fn dismiss_version_notice(&mut self) {
        self.version_updated_from = None;
    }

    /// Returns a summary string for the Footer Status Bar if any background task is active.
    pub fn active_task_summary(&self) -> Option<String> {
        let count = self.active_tasks.len();
        if count == 0 {
            return None;
        }
        if let Some(status) = self.active_tasks.get(&self.active_view) {
            if count == 1 {
                return Some(format!("{}: {}", self.active_view.label(), status));
            }
            return Some(format!(
                "{}: {} (+{} more)",
                self.active_view.label(),
                status,
                count - 1
            ));
        }
        let (view, status) = self.active_tasks.iter().next()?;
        if count == 1 {
            Some(format!("{}: {}", view.label(), status))
        } else {
            Some(format!(
                "{}: {} (+{} more)",
                view.label(),
                status,
                count - 1
            ))
        }
    }
}

/// Applies the selected `ThemeMode` to `gpui-kit`'s global `Theme`.
pub fn apply_theme(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    match mode {
        ThemeMode::Auto => Theme::sync_system_appearance(window, cx),
        ThemeMode::Light => Theme::change(gpui_kit::component::ThemeMode::Light, window, cx),
        ThemeMode::Dark => Theme::change(gpui_kit::component::ThemeMode::Dark, window, cx),
    }
}

/// Runs `CopilotService::check_auth_status` and (when authenticated) `list_models` in the background.
pub fn refresh_copilot_status(
    app_state: Entity<AppState>,
    force_refresh_models: bool,
    open_modal_on_unauth: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let (service, token) = app_state.update(cx, |state, cx| {
        state.copilot_auth = CopilotAuthStatus::Checking;
        state.models_loading = true;
        state.models_error = None;
        cx.notify();
        (
            state.copilot_service.clone(),
            state.settings.copilot_token.clone(),
        )
    });

    window
        .spawn(cx, async move |cx| {
            let service_for_auth = service.clone();
            let token_for_auth = token.clone();
            let auth_res: Result<CopilotAuthInfo, String> =
                match spawn_tokio(
                    async move { service_for_auth.check_auth_status(token_for_auth).await },
                )
                .await
                {
                    Ok(Ok(info)) => Ok(info),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                };

            match auth_res {
                Ok(info) if info.is_authenticated => {
                    let _ = cx.update(|window, cx| {
                        let should_close_modal = app_state.update(cx, |state, cx| {
                            state.copilot_auth = CopilotAuthStatus::Connected(info);
                            let was_open = state.auth_modal_open;
                            state.auth_modal_open = false;
                            cx.notify();
                            was_open
                        });
                        if should_close_modal {
                            window.close_dialog(cx);
                        }
                    });

                    let service_for_models = service.clone();
                    let token_for_models = token.clone();
                    let models_res: Result<Vec<CopilotModelInfo>, String> = match spawn_tokio(
                        async move {
                            service_for_models
                                .list_models(token_for_models, force_refresh_models)
                                .await
                        },
                    )
                    .await
                    {
                        Ok(Ok(models)) => Ok(models),
                        Ok(Err(err)) => Err(err.to_string()),
                        Err(err) => Err(err.to_string()),
                    };

                    let _ = cx.update(|_, cx| {
                        app_state.update(cx, |state, cx| {
                            state.models_loading = false;
                            match models_res {
                                Ok(models) => {
                                    state.copilot_models = models;
                                    state.models_error = None;
                                }
                                Err(err) => {
                                    state.models_error = Some(err);
                                }
                            }
                            cx.notify();
                        });
                    });
                }
                Ok(info) => {
                    let msg = info.status_message.clone();
                    let _ = cx.update(|window, cx| {
                        let should_open_modal = app_state.update(cx, |state, cx| {
                            state.copilot_auth = CopilotAuthStatus::Disconnected {
                                info: Some(info),
                                error: None,
                            };
                            state.copilot_models.clear();
                            state.models_loading = false;
                            state.models_error = msg;
                            let should_open =
                                open_modal_on_unauth && !state.startup_auth_modal_shown;
                            if should_open {
                                state.startup_auth_modal_shown = true;
                                state.auth_modal_open = true;
                            }
                            cx.notify();
                            should_open
                        });

                        if should_open_modal {
                            open_copilot_auth_modal(app_state.clone(), window, cx);
                        }
                    });
                }
                Err(err) => {
                    let _ = cx.update(|window, cx| {
                        let should_open_modal = app_state.update(cx, |state, cx| {
                            state.copilot_auth = CopilotAuthStatus::Disconnected {
                                info: None,
                                error: Some(err.clone()),
                            };
                            state.copilot_models.clear();
                            state.models_loading = false;
                            state.models_error = Some(err);
                            let should_open =
                                open_modal_on_unauth && !state.startup_auth_modal_shown;
                            if should_open {
                                state.startup_auth_modal_shown = true;
                                state.auth_modal_open = true;
                            }
                            cx.notify();
                            should_open
                        });

                        if should_open_modal {
                            open_copilot_auth_modal(app_state.clone(), window, cx);
                        }
                    });
                }
            }
        })
        .detach();
}
