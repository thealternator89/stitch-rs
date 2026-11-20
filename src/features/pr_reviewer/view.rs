use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, Theme, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, hsla, prelude::FluentBuilder as _, px,
};

use crate::app_state::{ActiveView, AppState};
use crate::features::pr_reviewer::phase_loader::{
    load_phases_from_disk, open_directory_in_os, pr_reviewer_root_dir,
};
use crate::features::pr_reviewer::service::{
    CheckoutError, PrReviewerService, verify_repo_path,
};
use crate::features::settings::SettingsTab;
use crate::infrastructure::git::GitService;
use crate::infrastructure::providers::build_code_review_provider;
use crate::infrastructure::runtime::spawn_tokio;
use crate::types::{
    AppSettings, CopilotModelInfo, PrDiffFile, PrDiffStatus, PrMetadata, PrSearchType, ReviewPhase,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PrSelectorTab {
    #[default]
    Assigned,
    Created,
    All,
    Manual,
}

impl PrSelectorTab {
    const ALL: &'static [Self] = &[Self::Assigned, Self::Created, Self::All, Self::Manual];

    fn id(self) -> &'static str {
        match self {
            Self::Assigned => "pr-tab-assigned",
            Self::Created => "pr-tab-created",
            Self::All => "pr-tab-all",
            Self::Manual => "pr-tab-manual",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Assigned => "Assigned to Me",
            Self::Created => "Created by Me",
            Self::All => "All Active PRs",
            Self::Manual => "Manual ID/URL",
        }
    }

    fn as_search_type(self) -> Option<PrSearchType> {
        match self {
            Self::Assigned => Some(PrSearchType::Assigned),
            Self::Created => Some(PrSearchType::Created),
            Self::All => Some(PrSearchType::All),
            Self::Manual => None,
        }
    }
}

#[derive(Debug, Clone)]
struct PhaseChecklistItem {
    phase: ReviewPhase,
    enabled: bool,
}

#[derive(Debug, Clone)]
struct CheckedOutState {
    pr: PrMetadata,
    repo_path: PathBuf,
    commit_sha: String,
    effective_repo_path: PathBuf,
    diff_files: Vec<PrDiffFile>,
    using_worktree: bool,
}

pub struct PrReviewerView {
    app_state: Entity<AppState>,
    git_service: Arc<GitService>,
    pr_service: Arc<PrReviewerService>,
    active_tab: PrSelectorTab,
    pr_list: Vec<PrMetadata>,
    is_loading_prs: bool,
    pr_list_error: Option<String>,
    initial_prs_loaded: bool,
    prs_request_gen: u64,
    search_input: Entity<InputState>,
    manual_pr_input: Entity<InputState>,
    repo_path_input: Entity<InputState>,
    repo_path_modified: bool,
    selected_pr: Option<PrMetadata>,
    pr_detail_gen: u64,
    loading_pr_details: bool,
    phases: Vec<PhaseChecklistItem>,
    critic_enabled: bool,
    is_loading_checkout: bool,
    loading_status: Option<String>,
    checkout_error: Option<String>,
    status_notice: Option<String>,
    checked_out: Option<CheckedOutState>,
    is_header_collapsed: bool,
    _subscriptions: Vec<Subscription>,
}

impl PrReviewerView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        git_service: Arc<GitService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pr_service = Arc::new(PrReviewerService::new(Arc::clone(&git_service)));

        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter by title, PR ID, author, or repo...")
        });
        let manual_pr_input = cx.new(|cx| {
            let placeholder = if initial_settings.sources.code == "github" {
                "https://github.com/owner/repo/pull/123 or PR #"
            } else {
                "https://dev.azure.com/org/project/_git/repo/pullrequest/123 or PR #"
            };
            InputState::new(window, cx).placeholder(placeholder)
        });
        let repo_path_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("/path/to/local/repository")
        });

        let phases = load_phases_from_disk()
            .into_iter()
            .map(|phase| PhaseChecklistItem {
                phase,
                enabled: true,
            })
            .collect();

        let search_sub = cx.subscribe(&search_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        let manual_sub = cx.subscribe_in(&manual_pr_input, window, |this, _, event, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.handle_manual_pr_submit(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });

        let repo_sub = cx.subscribe(&repo_path_input, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.repo_path_modified = false;
                this.checkout_error = None;
                cx.notify();
            }
        });

        let state_sub = cx.observe_in(&app_state, window, |this, app_state, window, cx| {
            let (is_active, code_ready, models, models_loading) = {
                let state = app_state.read(cx);
                (
                    state.active_view == ActiveView::PrReviewer,
                    is_code_source_configured(&state.settings),
                    state.copilot_models.clone(),
                    state.models_loading,
                )
            };
            this.reconcile_phase_models(&models, models_loading);
            if is_active && code_ready && !this.initial_prs_loaded && !this.is_loading_prs {
                this.load_project_prs(window, cx);
            }
            cx.notify();
        });

        Self {
            app_state,
            git_service,
            pr_service,
            active_tab: PrSelectorTab::Assigned,
            pr_list: Vec::new(),
            is_loading_prs: false,
            pr_list_error: None,
            initial_prs_loaded: false,
            prs_request_gen: 0,
            search_input,
            manual_pr_input,
            repo_path_input,
            repo_path_modified: false,
            selected_pr: None,
            pr_detail_gen: 0,
            loading_pr_details: false,
            phases,
            critic_enabled: initial_settings.pr_reviewer.critic_enabled,
            is_loading_checkout: false,
            loading_status: None,
            checkout_error: None,
            status_notice: None,
            checked_out: None,
            is_header_collapsed: false,
            _subscriptions: vec![search_sub, manual_sub, repo_sub, state_sub],
        }
    }

    fn reconcile_phase_models(&mut self, models: &[CopilotModelInfo], models_loading: bool) {
        if models_loading || models.is_empty() {
            return;
        }
        for item in &mut self.phases {
            if is_phase_model_missing(&item.phase, models, models_loading) {
                item.enabled = false;
            }
        }
    }

    fn reload_phases(&mut self, cx: &mut Context<Self>) {
        let (models, models_loading) = {
            let state = self.app_state.read(cx);
            (state.copilot_models.clone(), state.models_loading)
        };
        self.phases = load_phases_from_disk()
            .into_iter()
            .map(|phase| {
                let enabled = !is_phase_model_missing(&phase, &models, models_loading);
                PhaseChecklistItem { phase, enabled }
            })
            .collect();
        self.status_notice = Some(format!(
            "Loaded {} review phase(s) from {}",
            self.phases.len(),
            pr_reviewer_root_dir().join("phases").display()
        ));
        cx.notify();
    }

    fn open_phases_folder(&mut self, cx: &mut Context<Self>) {
        let phases_dir = pr_reviewer_root_dir().join("phases");
        if let Err(err) = open_directory_in_os(&phases_dir) {
            self.checkout_error = Some(format!(
                "Failed to open {}: {err}",
                phases_dir.display()
            ));
            cx.notify();
        }
    }

    fn switch_selector_tab(
        &mut self,
        tab: PrSelectorTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        self.pr_list_error = None;
        if tab != PrSelectorTab::Manual {
            self.load_project_prs(window, cx);
        } else {
            cx.notify();
        }
    }

    fn load_project_prs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search_type) = self.active_tab.as_search_type() else {
            return;
        };
        let settings = self.app_state.read(cx).settings.clone();

        self.prs_request_gen = self.prs_request_gen.wrapping_add(1);
        let request_gen = self.prs_request_gen;
        self.is_loading_prs = true;
        self.initial_prs_loaded = true;
        self.pr_list_error = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_code_review_provider(&settings)?;
                    provider.get_project_prs(search_type).await
                })
                .await;

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.prs_request_gen != request_gen {
                            return;
                        }
                        this.is_loading_prs = false;
                        match res {
                            Ok(Ok(prs)) => {
                                this.pr_list = prs;
                                this.pr_list_error = None;
                            }
                            Ok(Err(err)) => {
                                this.pr_list.clear();
                                this.pr_list_error = Some(err.to_string());
                            }
                            Err(err) => {
                                this.pr_list.clear();
                                this.pr_list_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn handle_manual_pr_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw_input = self.manual_pr_input.read(cx).value().trim().to_string();
        if raw_input.is_empty() || self.is_loading_prs {
            return;
        }

        let settings = self.app_state.read(cx).settings.clone();
        let current_repo_raw = self.repo_path_input.read(cx).value().trim().to_string();
        let git = Arc::clone(&self.git_service);

        self.prs_request_gen = self.prs_request_gen.wrapping_add(1);
        let request_gen = self.prs_request_gen;
        self.is_loading_prs = true;
        self.pr_list_error = None;
        self.checkout_error = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_code_review_provider(&settings)?;
                    let remote_url = if !current_repo_raw.is_empty() {
                        git.get_remote_url(PathBuf::from(current_repo_raw).as_path())
                            .await
                    } else {
                        None
                    };
                    provider
                        .get_pr_details(&raw_input, remote_url.as_deref())
                        .await
                })
                .await;

                let _ = cx.update(|window, cx| {
                    view.update(cx, |this, cx| {
                        if this.prs_request_gen != request_gen {
                            return;
                        }
                        this.is_loading_prs = false;
                        match res {
                            Ok(Ok(pr)) => {
                                this.select_pr(pr, window, cx);
                            }
                            Ok(Err(err)) => {
                                this.pr_list_error = Some(err.to_string());
                                cx.notify();
                            }
                            Err(err) => {
                                this.pr_list_error = Some(err.to_string());
                                cx.notify();
                            }
                        }
                    });
                });
            })
            .detach();
    }

    fn select_pr(&mut self, pr: PrMetadata, window: &mut Window, cx: &mut Context<Self>) {
        let needs_details =
            pr.source_branch.trim().is_empty() || pr.target_branch.trim().is_empty();
        let pr_lookup = pr.url.clone().unwrap_or_else(|| pr.id.clone());

        let (remembered_path, settings) = {
            let state = self.app_state.read(cx);
            let mapped = lookup_remembered_repo_path(&state.settings, &pr.repository_name);
            (mapped, state.settings.clone())
        };

        self.selected_pr = Some(pr.clone());
        self.checked_out = None;
        self.is_header_collapsed = false;
        self.repo_path_modified = false;
        self.checkout_error = None;
        self.status_notice = None;
        self.repo_path_input.update(cx, |input, cx| {
            let val = remembered_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            input.set_value(val, window, cx);
        });

        self.pr_detail_gen = self.pr_detail_gen.wrapping_add(1);
        if needs_details && !pr_lookup.trim().is_empty() {
            let detail_gen = self.pr_detail_gen;
            self.loading_pr_details = true;
            cx.notify();

            let view = cx.entity();
            window
                .spawn(cx, async move |cx| {
                    let res = spawn_tokio(async move {
                        let provider = build_code_review_provider(&settings)?;
                        provider.get_pr_details(&pr_lookup, None).await
                    })
                    .await;

                    let _ = cx.update(|_, cx| {
                        view.update(cx, |this, cx| {
                            if this.pr_detail_gen != detail_gen {
                                return;
                            }
                            this.loading_pr_details = false;
                            if let Ok(Ok(full_pr)) = res {
                                this.selected_pr = Some(full_pr);
                            }
                            cx.notify();
                        });
                    });
                })
                .detach();
        } else {
            self.loading_pr_details = false;
            cx.notify();
        }
    }

    fn browse_repo_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_loading_checkout {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select Locally Cloned Repository Directory".into()),
        });

        let git = Arc::clone(&self.git_service);
        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = receiver.await else {
                    return;
                };
                let Some(selected) = paths.into_iter().next() else {
                    return;
                };

                let selected_for_verify = selected.clone();
                let verified =
                    spawn_tokio(async move { verify_repo_path(&git, &selected_for_verify).await })
                        .await
                        .unwrap_or_else(|_| crate::features::pr_reviewer::service::VerifiedRepoPath {
                            path: selected,
                            was_modified: false,
                        });

                let _ = cx.update(|window, cx| {
                    view.update(cx, |this, cx| {
                        let path_str = verified.path.to_string_lossy().to_string();
                        this.repo_path_input.update(cx, |input, cx| {
                            input.set_value(path_str, window, cx);
                        });
                        this.repo_path_modified = verified.was_modified;
                        this.checkout_error = None;
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn handle_checkout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_loading_checkout {
            return;
        }
        let Some(pr) = self.selected_pr.clone() else {
            self.checkout_error = Some("Please select a Pull Request first.".to_string());
            cx.notify();
            return;
        };

        let repo_raw = self.repo_path_input.read(cx).value().trim().to_string();
        if repo_raw.is_empty() {
            self.checkout_error = Some("Please select a local repository path.".to_string());
            cx.notify();
            return;
        }
        let repo_path = PathBuf::from(&repo_raw);
        if !repo_path.is_dir() {
            self.checkout_error = Some(format!(
                "Selected repository directory does not exist: {}",
                repo_path.display()
            ));
            cx.notify();
            return;
        }

        let Ok(pr_number) = pr.id.trim().parse::<u64>() else {
            self.checkout_error = Some(format!("Invalid PR ID: '{}'", pr.id));
            cx.notify();
            return;
        };

        let settings = self.app_state.read(cx).settings.clone();
        let target_branch = if pr.target_branch.trim().is_empty() {
            "main".to_string()
        } else {
            pr.target_branch.trim().to_string()
        };
        let expected_repo = if pr.repository_name.trim().is_empty() {
            None
        } else {
            Some(pr.repository_name.clone())
        };

        self.is_loading_checkout = true;
        self.loading_status = Some("Running repository checks and checking out PR...".to_string());
        self.checkout_error = None;
        self.status_notice = None;
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::PrReviewer,
                Some(format!("Fetching & checking out PR #{pr_number}...")),
            );
            cx.notify();
        });
        cx.notify();

        let pr_service = Arc::clone(&self.pr_service);
        let git = Arc::clone(&self.git_service);
        let view = cx.entity();
        let app_state = self.app_state.clone();

        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let verified = verify_repo_path(&git, &repo_path).await;
                    let code_provider = build_code_review_provider(&settings).ok();
                    let checkout = pr_service
                        .checkout_pr(
                            &verified.path,
                            pr_number,
                            &target_branch,
                            expected_repo.as_deref(),
                            code_provider.as_deref(),
                            &settings,
                        )
                        .await?;
                    Ok::<_, CheckoutError>((verified, checkout))
                })
                .await;

                let _ = cx.update(|window, cx| {
                    let mut dirty_message: Option<String> = None;

                    view.update(cx, |this, cx| {
                        this.is_loading_checkout = false;
                        this.loading_status = None;
                        app_state.update(cx, |state, cx| {
                            state.set_task_status(ActiveView::PrReviewer, None);
                            cx.notify();
                        });

                        match res {
                            Ok(Ok((verified, checkout))) => {
                                let canonical_str = verified.path.to_string_lossy().to_string();
                                if verified.was_modified {
                                    this.repo_path_input.update(cx, |input, cx| {
                                        input.set_value(canonical_str, window, cx);
                                    });
                                    this.repo_path_modified = true;
                                }
                                if !pr.repository_name.trim().is_empty() {
                                    app_state.update(cx, |state, cx| {
                                        state
                                            .settings
                                            .repo_paths
                                            .insert(pr.repository_name.clone(), verified.path.clone());
                                        let _ = state.save_settings();
                                        cx.notify();
                                    });
                                }
                                this.checked_out = Some(CheckedOutState {
                                    pr: pr.clone(),
                                    repo_path: verified.path,
                                    commit_sha: checkout.commit_sha,
                                    effective_repo_path: checkout.effective_repo_path,
                                    diff_files: checkout.diff_files,
                                    using_worktree: checkout.using_worktree,
                                });
                                this.is_header_collapsed = true;
                                this.checkout_error = None;
                            }
                            Ok(Err(CheckoutError::DirtyRepo(msg))) => {
                                dirty_message = Some(msg.clone());
                                this.checkout_error = Some(msg);
                            }
                            Ok(Err(CheckoutError::Other(msg))) => {
                                this.checkout_error = Some(msg);
                            }
                            Err(err) => {
                                this.checkout_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });

                    if let Some(msg) = dirty_message {
                        open_dirty_repo_dialog(msg, window, cx);
                    }
                });
            })
            .detach();
    }

    fn handle_cleanup_checkout(&mut self, cx: &mut Context<Self>) {
        let Some(checked) = self.checked_out.clone() else {
            return;
        };
        let pr_service = Arc::clone(&self.pr_service);
        let view = cx.entity();

        cx.spawn(async move |_, cx| {
            let res =
                spawn_tokio(async move { pr_service.cleanup_checkout(&checked.repo_path).await })
                    .await;

            let _ = view.update(cx, |this, cx| {
                match res {
                    Ok(Ok(())) => {
                        this.checked_out = None;
                        this.is_header_collapsed = false;
                        this.checkout_error = None;
                        this.status_notice = Some(if checked.using_worktree {
                            "Removed temporary PR worktree.".to_string()
                        } else {
                            "Restored repository to previous branch/ref.".to_string()
                        });
                    }
                    Ok(Err(err)) => {
                        this.checkout_error = Some(err.to_string());
                    }
                    Err(err) => {
                        this.checkout_error = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_group(&mut self, group_name: &str, checked: bool, cx: &mut Context<Self>) {
        let (models, models_loading) = {
            let state = self.app_state.read(cx);
            (state.copilot_models.clone(), state.models_loading)
        };
        for item in &mut self.phases {
            let item_group = if item.phase.group.trim().is_empty() {
                "Ungrouped"
            } else {
                item.phase.group.as_str()
            };
            if item_group == group_name {
                let missing = is_phase_model_missing(&item.phase, &models, models_loading);
                item.enabled = if missing { false } else { checked };
            }
        }
        cx.notify();
    }

    fn toggle_phase(&mut self, index: usize, checked: bool, cx: &mut Context<Self>) {
        let (models, models_loading) = {
            let state = self.app_state.read(cx);
            (state.copilot_models.clone(), state.models_loading)
        };
        if let Some(item) = self.phases.get_mut(index) {
            let missing = is_phase_model_missing(&item.phase, &models, models_loading);
            item.enabled = if missing { false } else { checked };
            cx.notify();
        }
    }

    fn toggle_critic(&mut self, checked: bool, cx: &mut Context<Self>) {
        self.critic_enabled = checked;
        self.app_state.update(cx, |state, cx| {
            state.settings.pr_reviewer.critic_enabled = checked;
            let _ = state.save_settings();
            cx.notify();
        });
        cx.notify();
    }

    fn filtered_prs(&self, cx: &App) -> Vec<PrMetadata> {
        let q = self.search_input.read(cx).value().trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.pr_list.clone();
        }
        self.pr_list
            .iter()
            .filter(|pr| {
                pr.title.to_ascii_lowercase().contains(&q)
                    || pr.id.to_ascii_lowercase().contains(&q)
                    || pr.repository_name.to_ascii_lowercase().contains(&q)
                    || pr.author.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    }

    fn render_selector_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let settings = &self.app_state.read(cx).settings;
        let code_configured = is_code_source_configured(settings);
        let code_source_label = match settings.sources.code.as_str() {
            "azureDevOps" => "Azure DevOps",
            "github" => "GitHub",
            _ => "Unconfigured",
        };

        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let app_state_for_warn = self.app_state.clone();
        let filtered = self.filtered_prs(cx);
        let selected_pr_id = self.selected_pr.as_ref().map(|p| p.id.clone());
        let manual_non_empty = !self.manual_pr_input.read(cx).value().trim().is_empty();

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .p_5()
            .gap_4()
            .when(!code_configured, |col| {
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
                                .child("Configure Azure DevOps or GitHub and select Code Source in Settings > Connectors to list and inspect PRs."),
                        )
                        .child(
                            Button::new("pr-open-connectors")
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
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(LucideIcon::GitPullRequest).small())
                            .child(format!("Select Pull Request ({code_source_label})")),
                    )
                    .when(self.active_tab != PrSelectorTab::Manual, |row| {
                        row.child(
                            Button::new("pr-refresh-list")
                                .ghost()
                                .xsmall()
                                .disabled(self.is_loading_prs)
                                .loading(self.is_loading_prs)
                                .icon(Icon::new(LucideIcon::RefreshCw).xsmall())
                                .label("Refresh")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.load_project_prs(window, cx);
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1p5()
                    .children(PrSelectorTab::ALL.iter().copied().map(|tab| {
                        let active = self.active_tab == tab;
                        let btn = Button::new(tab.id())
                            .small()
                            .label(tab.label())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.switch_selector_tab(tab, window, cx);
                            }));
                        if active { btn.primary() } else { btn.ghost() }
                    })),
            )
            .when_some(self.pr_list_error.clone(), |col, err| {
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
            .child(if self.active_tab == PrSelectorTab::Manual {
                v_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(format!("{code_source_label} PR URL or ID")),
                            )
                            .child(Input::new(&self.manual_pr_input)),
                    )
                    .child(
                        Button::new("pr-load-manual")
                            .primary()
                            .w_full()
                            .disabled(self.is_loading_prs || !manual_non_empty)
                            .loading(self.is_loading_prs)
                            .icon(Icon::new(LucideIcon::Search).small())
                            .label("Load PR Details")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.handle_manual_pr_submit(window, cx);
                            })),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(Input::new(&self.search_input))
                    .child(
                        div().flex_1().min_h_0().overflow_hidden().child(
                            if self.is_loading_prs {
                                v_flex()
                                    .py_8()
                                    .items_center()
                                    .justify_center()
                                    .gap_2()
                                    .child(Spinner::new().color(theme.primary))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("Querying {code_source_label}...")),
                                    )
                                    .into_any_element()
                            } else if filtered.is_empty() {
                                v_flex()
                                    .py_8()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("No active PRs found matching the criteria."),
                                    )
                                    .into_any_element()
                            } else {
                                v_flex()
                                    .size_full()
                                    .overflow_y_scrollbar()
                                    .gap_2()
                                    .children(filtered.into_iter().enumerate().map(|(idx, pr)| {
                                        let is_selected = selected_pr_id
                                            .as_deref()
                                            .is_some_and(|id| id == pr.id);
                                        let pr_click = pr.clone();
                                        let branch_summary = if pr.source_branch.is_empty()
                                            && pr.target_branch.is_empty()
                                        {
                                            None
                                        } else {
                                            Some(format!(
                                                "{} -> {}",
                                                pr.source_branch, pr.target_branch
                                            ))
                                        };

                                        v_flex()
                                            .id(("pr-card", idx))
                                            .w_full()
                                            .p_3()
                                            .gap_1()
                                            .cursor_pointer()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(if is_selected {
                                                theme.primary
                                            } else {
                                                theme.border
                                            })
                                            .bg(if is_selected {
                                                theme.primary.opacity(0.1)
                                            } else {
                                                theme.background
                                            })
                                            .hover(|s| {
                                                if is_selected {
                                                    s
                                                } else {
                                                    s.bg(theme.secondary.opacity(0.5))
                                                }
                                            })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.select_pr(pr_click.clone(), window, cx);
                                            }))
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(theme.primary)
                                                            .child(format!("PR #{}", pr.id)),
                                                    )
                                                    .when(!pr.repository_name.is_empty(), |row| {
                                                        row.child(
                                                            div()
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded(theme.radius)
                                                                .bg(theme.secondary)
                                                                .text_xs()
                                                                .text_color(theme.muted_foreground)
                                                                .child(pr.repository_name.clone()),
                                                        )
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.foreground)
                                                    .child(pr.title),
                                            )
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .gap_2()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!("By {}", pr.author))
                                                    .when_some(branch_summary, |row, branches| {
                                                        row.child(div().truncate().child(branches))
                                                    }),
                                            )
                                    }))
                                    .into_any_element()
                            },
                        ),
                    )
                    .into_any_element()
            })
    }

    fn render_details_and_checkout_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (models, models_loading) = {
            let state = self.app_state.read(cx);
            (state.copilot_models.clone(), state.models_loading)
        };
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        let Some(pr) = self.selected_pr.clone() else {
            return v_flex()
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
                        .child(Icon::new(LucideIcon::GitPullRequest).large()),
                )
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Select a Pull Request"),
                )
                .child(
                    div()
                        .max_w(px(460.))
                        .text_center()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Choose a Pull Request on the left or enter a PR URL/ID manually to inspect its local repository, review phases, and diff."),
                )
                .into_any_element();
        };

        let grouped_phases = group_phases_for_render(&self.phases, &models, models_loading);
        let repo_non_empty = !self.repo_path_input.read(cx).value().trim().is_empty();
        let phases_dir_label = pr_reviewer_root_dir().join("phases").display().to_string();

        v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .bg(theme.background)
            .p_6()
            .gap_4()
            .when_some(self.checkout_error.clone(), |col, err| {
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
                        .child(Icon::new(LucideIcon::Check).small())
                        .child(div().flex_1().child(note)),
                )
            })
            .child(
                v_flex()
                    .p_4()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .bg(theme.primary.opacity(0.14))
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.primary)
                                            .child(format!("PR #{}", pr.id)),
                                    )
                                    .when(!pr.repository_name.is_empty(), |row| {
                                        row.child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(theme.radius)
                                                .bg(theme.secondary)
                                                .border_1()
                                                .border_color(theme.border)
                                                .text_xs()
                                                .child(format!("Repo: {}", pr.repository_name)),
                                        )
                                    })
                                    .when(self.loading_pr_details, |row| {
                                        row.child(Spinner::new().small().color(theme.primary))
                                    }),
                            )
                            .when_some(pr.url.clone(), |row, url| {
                                row.child(
                                    Button::new("pr-open-web-setup")
                                        .ghost()
                                        .xsmall()
                                        .icon(Icon::new(LucideIcon::ExternalLink).xsmall())
                                        .label("Open")
                                        .on_click(move |_, _, cx| {
                                            cx.open_url(&url);
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(pr.title.clone()),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("Author: {}", pr.author))
                            .when(!pr.source_branch.is_empty() || !pr.target_branch.is_empty(), |row| {
                                row.child(
                                    h_flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded(theme.radius)
                                                .bg(theme.background)
                                                .border_1()
                                                .border_color(theme.border)
                                                .font_family(theme.mono_font_family.clone())
                                                .child(pr.source_branch.clone()),
                                        )
                                        .child(Icon::new(LucideIcon::ArrowRight).xsmall())
                                        .child(
                                            div()
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded(theme.radius)
                                                .bg(theme.background)
                                                .border_1()
                                                .border_color(theme.border)
                                                .font_family(theme.mono_font_family.clone())
                                                .child(pr.target_branch.clone()),
                                        ),
                                )
                            }),
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
                            .child(format!(
                                "Locally Cloned Repo Path for \"{}\"",
                                if pr.repository_name.is_empty() {
                                    "Repository"
                                } else {
                                    pr.repository_name.as_str()
                                }
                            )),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Input::new(&self.repo_path_input)
                                            .disabled(self.is_loading_checkout),
                                    ),
                            )
                            .child(
                                Button::new("pr-browse-repo")
                                    .small()
                                    .disabled(self.is_loading_checkout)
                                    .icon(Icon::new(LucideIcon::FolderOpen).small())
                                    .label("Browse")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.browse_repo_dir(window, cx);
                                    })),
                            ),
                    )
                    .when(self.repo_path_modified, |col| {
                        col.child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .text_xs()
                                .text_color(amber)
                                .child(Icon::new(LucideIcon::TriangleAlert).xsmall())
                                .child("Updated to repository root"),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if repo_non_empty {
                                "Select or confirm the local Git clone directory used to fetch and diff this PR.".to_string()
                            } else {
                                format!(
                                    "Choose the local clone directory for repository \"{}\".",
                                    pr.repository_name
                                )
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child("Active Review Phases"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "{}/{} enabled",
                                        self.phases.iter().filter(|p| p.enabled).count(),
                                        self.phases.len()
                                    )),
                            ),
                    )
                    .child(if self.phases.is_empty() {
                        v_flex()
                            .p_4()
                            .gap_3()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(Icon::new(LucideIcon::SlidersHorizontal).small())
                                    .child("Set Up Review Phases"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("No review phases found. Add Markdown phase files inside ~/.stitch/pr-reviewer/phases/ and click Check Again."),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .px_3()
                                    .py_2()
                                    .rounded(theme.radius)
                                    .bg(theme.background)
                                    .border_1()
                                    .border_color(theme.border)
                                    .font_family(theme.mono_font_family.clone())
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(LucideIcon::Folder).xsmall())
                                    .child(phases_dir_label),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("pr-empty-open-dir")
                                            .small()
                                            .icon(Icon::new(LucideIcon::FolderOpen).small())
                                            .label("Open Phases Folder")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.open_phases_folder(cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("pr-empty-check-again")
                                            .primary()
                                            .small()
                                            .icon(Icon::new(LucideIcon::RefreshCw).small())
                                            .label("Check Again")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.reload_phases(cx);
                                            })),
                                    ),
                            )
                            .into_any_element()
                    } else {
                        v_flex()
                            .p_4()
                            .gap_3()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .children(grouped_phases.into_iter().enumerate().map(|(group_idx, group)| {
                                let group_name_for_click = group.name.clone();
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Checkbox::new(("pr-group-check", group_idx))
                                                    .small()
                                                    .checked(group.all_checked)
                                                    .disabled(group.selectable_count == 0)
                                                    .label(group.name.clone())
                                                    .on_click(cx.listener(move |this, next: &bool, _, cx| {
                                                        this.toggle_group(&group_name_for_click, *next, cx);
                                                    })),
                                            )
                                            .when(group.some_checked, |row| {
                                                row.child(
                                                    div()
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded(theme.radius)
                                                        .bg(theme.secondary)
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child("partial"),
                                                )
                                            }),
                                    )
                                    .child(
                                        v_flex()
                                            .pl_5()
                                            .gap_1p5()
                                            .border_l_1()
                                            .border_color(theme.border)
                                            .children(group.items.into_iter().map(|entry| {
                                                let idx = entry.original_index;
                                                let item = entry.item;
                                                let tpl_err = item.phase.template_error.clone();
                                                let phase_model = item.phase.model.clone();
                                                let missing = entry.model_missing;
                                                let matched_label = entry.matched_model_name;

                                                v_flex()
                                                    .gap_0p5()
                                                    .child(
                                                        h_flex()
                                                            .flex_wrap()
                                                            .items_center()
                                                            .gap_2()
                                                            .child(
                                                                Checkbox::new(("pr-phase-check", idx))
                                                                    .small()
                                                                    .checked(!missing && item.enabled)
                                                                    .disabled(missing)
                                                                    .label(item.phase.title.clone())
                                                                    .on_click(cx.listener(move |this, next: &bool, _, cx| {
                                                                        this.toggle_phase(idx, *next, cx);
                                                                    })),
                                                            )
                                                            .when_some(phase_model, |row, model_id| {
                                                                if missing {
                                                                    row.child(
                                                                        div()
                                                                            .px_1p5()
                                                                            .py_0p5()
                                                                            .rounded(theme.radius)
                                                                            .border_1()
                                                                            .border_color(red.opacity(0.4))
                                                                            .bg(red.opacity(0.1))
                                                                            .font_family(theme.mono_font_family.clone())
                                                                            .text_xs()
                                                                            .text_color(red)
                                                                            .child(format!("Model not available: {model_id}")),
                                                                    )
                                                                } else {
                                                                    row.child(
                                                                        div()
                                                                            .px_1p5()
                                                                            .py_0p5()
                                                                            .rounded(theme.radius)
                                                                            .bg(theme.secondary)
                                                                            .border_1()
                                                                            .border_color(theme.border)
                                                                            .font_family(theme.mono_font_family.clone())
                                                                            .text_xs()
                                                                            .text_color(theme.muted_foreground)
                                                                            .child(matched_label.unwrap_or(model_id)),
                                                                    )
                                                                }
                                                            }),
                                                    )
                                                    .when_some(tpl_err, |col, err_msg| {
                                                        col.child(
                                                            h_flex()
                                                                .pl_5()
                                                                .items_center()
                                                                .gap_1()
                                                                .text_xs()
                                                                .text_color(red)
                                                                .child(Icon::new(LucideIcon::CircleAlert).xsmall())
                                                                .child(err_msg),
                                                        )
                                                    })
                                            })),
                                    )
                            }))
                            .child(div().h(px(1.)).w_full().bg(theme.border))
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        Checkbox::new("pr-critic-toggle")
                                            .small()
                                            .checked(self.critic_enabled)
                                            .label("Critic")
                                            .on_click(cx.listener(|this, next: &bool, _, cx| {
                                                this.toggle_critic(*next, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .pl_6()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Critiques and refines generated comments once all review phases finish."),
                                    ),
                            )
                            .into_any_element()
                    }),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .pt_1()
                    .child(
                        div().flex_1().min_w_0().when_some(self.loading_status.clone(), |el, msg| {
                            el.child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Spinner::new().small().color(theme.primary))
                                    .child(msg),
                            )
                        }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .when(self.checked_out.is_some(), |row| {
                                row.child(
                                    Button::new("pr-collapse-setup")
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(LucideIcon::Minimize2).small())
                                        .label("Collapse Settings")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.is_header_collapsed = true;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                Button::new("pr-fetch-checkout")
                                    .primary()
                                    .disabled(self.is_loading_checkout || !repo_non_empty)
                                    .loading(self.is_loading_checkout)
                                    .icon(Icon::new(LucideIcon::CloudDownload).small())
                                    .label(if self.is_loading_checkout {
                                        "Checking Out..."
                                    } else {
                                        "Fetch & Checkout PR"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.handle_checkout(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_collapsed_workspace(
        &self,
        checked: &CheckedOutState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let short_sha: String = checked.commit_sha.chars().take(8).collect();
        let enabled_phases = self.phases.iter().filter(|p| p.enabled).count();
        let pr_url = checked.pr.url.clone();
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px_6()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
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
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.primary)
                                            .child(format!("PR #{}", checked.pr.id)),
                                    )
                                    .when(!checked.pr.repository_name.is_empty(), |row| {
                                        row.child(
                                            div()
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded(theme.radius)
                                                .bg(theme.secondary)
                                                .font_family(theme.mono_font_family.clone())
                                                .text_xs()
                                                .child(checked.pr.repository_name.clone()),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("By {}", checked.pr.author)),
                                    )
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.background)
                                            .font_family(theme.mono_font_family.clone())
                                            .text_xs()
                                            .child(format!("{} -> {}", checked.pr.source_branch, checked.pr.target_branch)),
                                    )
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .bg(theme.secondary)
                                            .font_family(theme.mono_font_family.clone())
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(short_sha),
                                    )
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(if checked.using_worktree {
                                                green.opacity(0.4)
                                            } else {
                                                amber.opacity(0.4)
                                            })
                                            .bg(if checked.using_worktree {
                                                green.opacity(0.1)
                                            } else {
                                                amber.opacity(0.1)
                                            })
                                            .text_xs()
                                            .text_color(if checked.using_worktree { green } else { amber })
                                            .child(if checked.using_worktree { "Worktree" } else { "Local Checkout" }),
                                    ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(checked.pr.title.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .when_some(pr_url, |row, url| {
                                row.child(
                                    Button::new("pr-open-web-collapsed")
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(LucideIcon::ExternalLink).small())
                                        .label("Open")
                                        .on_click(move |_, _, cx| {
                                            cx.open_url(&url);
                                        }),
                                )
                            })
                            .child(
                                Button::new("pr-expand-selection")
                                    .small()
                                    .icon(Icon::new(LucideIcon::Maximize2).small())
                                    .label("Expand PR Selection")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.is_header_collapsed = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("pr-cleanup-checkout")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RotateCcw).small())
                                    .label("Clean Checkout")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.handle_cleanup_checkout(cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.checkout_error.clone(), |col, err| {
                col.child(
                    h_flex()
                        .mx_6()
                        .mt_4()
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
                div().flex_1().min_h_0().w_full().overflow_y_scrollbar().p_6().child(
                    v_flex()
                        .gap_4()
                        .max_w(px(920.))
                        .child(
                            v_flex()
                                .p_4()
                                .gap_2()
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
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(Icon::new(LucideIcon::GitBranch).small())
                                                .child("PR Checkout Ready"),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!(
                                                    "{} phase(s) selected{}",
                                                    enabled_phases,
                                                    if self.critic_enabled { " + Critic" } else { "" }
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(format!(
                                            "Working directory: {}",
                                            checked.effective_repo_path.display()
                                        )),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Parallel multi-phase Copilot review execution and comment posting arrive in Phase 10; AI Critic evaluation arrives in Phase 11."),
                                ),
                        )
                        .child(
                            v_flex()
                                .rounded(theme.radius_lg)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .overflow_hidden()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .justify_between()
                                        .px_4()
                                        .py_2p5()
                                        .bg(theme.secondary)
                                        .border_b_1()
                                        .border_color(theme.border)
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child("Changed Files in PR"),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!("{} file(s)", checked.diff_files.len())),
                                        ),
                                )
                                .child(if checked.diff_files.is_empty() {
                                    div()
                                        .p_4()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("No changed files detected against target branch.")
                                        .into_any_element()
                                } else {
                                    v_flex()
                                        .children(checked.diff_files.iter().enumerate().map(|(i, file)| {
                                            let (icon, color) = diff_status_style(file.status, &theme);
                                            h_flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_3()
                                                .px_4()
                                                .py_2()
                                                .when(i > 0, |row| {
                                                    row.border_t_1().border_color(theme.border)
                                                })
                                                .child(
                                                    h_flex()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .items_center()
                                                        .gap_2()
                                                        .text_xs()
                                                        .text_color(color)
                                                        .child(Icon::new(icon).xsmall())
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .font_family(theme.mono_font_family.clone())
                                                                .text_color(theme.foreground)
                                                                .truncate()
                                                                .child(file.path.clone()),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded(theme.radius)
                                                        .border_1()
                                                        .border_color(color.opacity(0.4))
                                                        .bg(color.opacity(0.1))
                                                        .text_xs()
                                                        .text_color(color)
                                                        .child(file.status.label()),
                                                )
                                        }))
                                        .into_any_element()
                                }),
                        ),
                ),
            )
            .into_any_element()
    }
}

impl Render for PrReviewerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, border, radius, secondary, accent_fg, muted_fg, code_ready) = {
            let theme = cx.theme();
            let settings = &self.app_state.read(cx).settings;
            (
                theme.background,
                theme.border,
                theme.radius_lg,
                theme.secondary,
                theme.accent_foreground,
                theme.muted_foreground,
                is_code_source_configured(settings),
            )
        };
        let app_state_for_header = self.app_state.clone();

        let body: AnyElement = if self.is_header_collapsed {
            if let Some(checked) = self.checked_out.as_ref() {
                self.render_collapsed_workspace(checked, cx)
            } else {
                h_resizable("pr-reviewer-setup-split")
                    .child(
                        resizable_panel()
                            .size(px(420.))
                            .size_range(px(330.)..px(600.))
                            .child(self.render_selector_pane(cx)),
                    )
                    .child(resizable_panel().child(self.render_details_and_checkout_pane(cx)))
                    .into_any_element()
            }
        } else {
            h_resizable("pr-reviewer-setup-split")
                .child(
                    resizable_panel()
                        .size(px(420.))
                        .size_range(px(330.)..px(600.))
                        .child(self.render_selector_pane(cx)),
                )
                .child(resizable_panel().child(self.render_details_and_checkout_pane(cx)))
                .into_any_element()
        };

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
                                        Icon::new(ActiveView::PrReviewer.icon())
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
                                            .child(ActiveView::PrReviewer.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::PrReviewer.subtitle()),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("pr-open-phases-folder")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::FolderOpen).small())
                                    .label("Open Phases Folder")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_phases_folder(cx);
                                    })),
                            )
                            .child(
                                Button::new("pr-reload-phases")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label("Reload Phases")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_phases(cx);
                                    })),
                            )
                            .when(!code_ready, |row| {
                                row.child(
                                    Button::new("pr-header-connectors")
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
                            }),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(body))
    }
}

struct PhaseGroupEntry {
    original_index: usize,
    item: PhaseChecklistItem,
    model_missing: bool,
    matched_model_name: Option<String>,
}

struct RenderedPhaseGroup {
    name: String,
    items: Vec<PhaseGroupEntry>,
    selectable_count: usize,
    all_checked: bool,
    some_checked: bool,
}

fn group_phases_for_render(
    phases: &[PhaseChecklistItem],
    models: &[CopilotModelInfo],
    models_loading: bool,
) -> Vec<RenderedPhaseGroup> {
    let mut groups: Vec<RenderedPhaseGroup> = Vec::new();
    for (idx, item) in phases.iter().enumerate() {
        let group_name = if item.phase.group.trim().is_empty() {
            "Ungrouped".to_string()
        } else {
            item.phase.group.clone()
        };
        let missing = is_phase_model_missing(&item.phase, models, models_loading);
        let matched_model_name = item.phase.model.as_deref().and_then(|pm| {
            models
                .iter()
                .find(|m| m.id.eq_ignore_ascii_case(pm))
                .map(|m| m.name.clone())
        });
        let entry = PhaseGroupEntry {
            original_index: idx,
            item: item.clone(),
            model_missing: missing,
            matched_model_name,
        };
        if let Some(g) = groups.iter_mut().find(|g| g.name == group_name) {
            g.items.push(entry);
        } else {
            groups.push(RenderedPhaseGroup {
                name: group_name,
                items: vec![entry],
                selectable_count: 0,
                all_checked: false,
                some_checked: false,
            });
        }
    }

    for g in &mut groups {
        let selectable: Vec<&PhaseGroupEntry> =
            g.items.iter().filter(|e| !e.model_missing).collect();
        g.selectable_count = selectable.len();
        let enabled_count = selectable.iter().filter(|e| e.item.enabled).count();
        g.all_checked = !selectable.is_empty() && enabled_count == selectable.len();
        g.some_checked = enabled_count > 0 && enabled_count < selectable.len();
    }
    groups
}

fn is_phase_model_missing(
    phase: &ReviewPhase,
    models: &[CopilotModelInfo],
    models_loading: bool,
) -> bool {
    let Some(ref pm) = phase.model else {
        return false;
    };
    if models_loading || models.is_empty() {
        return false;
    }
    !models.iter().any(|m| m.id.eq_ignore_ascii_case(pm))
}

fn is_code_source_configured(settings: &AppSettings) -> bool {
    match settings.sources.code.as_str() {
        "azureDevOps" => settings.connectors.is_azure_configured(),
        "github" => settings.connectors.is_github_configured(),
        _ => false,
    }
}

fn lookup_remembered_repo_path(settings: &AppSettings, repo_name: &str) -> Option<PathBuf> {
    let clean = repo_name.trim();
    if clean.is_empty() {
        return None;
    }
    if let Some(exact) = settings.repo_paths.get(clean) {
        return Some(exact.clone());
    }
    settings
        .repo_paths
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(clean))
        .map(|(_, v)| v.clone())
}

fn diff_status_style(status: PrDiffStatus, theme: &Theme) -> (LucideIcon, Hsla) {
    let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
    let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
    let red = hsla(0.0, 0.78, 0.55, 1.0);
    match status {
        PrDiffStatus::Added => (LucideIcon::FilePlus, green),
        PrDiffStatus::Deleted => (LucideIcon::FileMinus, red),
        PrDiffStatus::Modified => (LucideIcon::FileDiff, amber),
        PrDiffStatus::Renamed => (LucideIcon::ArrowRight, theme.primary),
        PrDiffStatus::TypeChanged | PrDiffStatus::Unknown => {
            (LucideIcon::FileCode, theme.muted_foreground)
        }
    }
}

fn open_dirty_repo_dialog(message: String, window: &mut Window, cx: &mut App) {
    let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
    let muted_fg = cx.theme().muted_foreground;

    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(520.))
            .overlay_closable(true)
            .close_button(true)
            .title(
                h_flex()
                    .items_center()
                    .gap_2()
                    .text_color(amber)
                    .child(Icon::new(LucideIcon::TriangleAlert).with_size(Size::Medium))
                    .child("Local Repository Is Dirty"),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .child("The local Git repository has uncommitted changes. Commit, stash, or revert your local changes (or enable Git Worktree Support in Settings > General) before checking out this Pull Request."),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(message.clone()),
                    ),
            )
            .footer(
                h_flex().w_full().justify_end().child(
                    Button::new("pr-dirty-dismiss")
                        .primary()
                        .label("Dismiss")
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                ),
            )
    });
}
