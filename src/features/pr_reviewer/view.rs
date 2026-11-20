use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Sizable as _, Size, Theme, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, hsla,
    prelude::FluentBuilder as _, px, relative,
};
use tokio::sync::{mpsc, watch};

use crate::app_state::{ActiveView, AppState};
use crate::features::pr_reviewer::phase_loader::{
    load_phases_from_disk, open_directory_in_os, pr_reviewer_root_dir,
};
use crate::features::pr_reviewer::service::{
    CheckoutError, PrReviewRequest, PrReviewStreamEvent, PrReviewerService, verify_repo_path,
};
use crate::features::settings::SettingsTab;
use crate::infrastructure::copilot::CopilotService;
use crate::infrastructure::git::GitService;
use crate::infrastructure::providers::build_code_review_provider;
use crate::infrastructure::runtime::{runtime, spawn_tokio};
use crate::types::{
    AppSettings, CodeContextLine, CopilotModelInfo, CopilotUsage, PhaseProgress, PhaseRunStatus,
    PostPrCommentRequest, PrDiffFile, PrDiffStatus, PrMetadata, PrSearchType, ReviewComment,
    ReviewCommentType, ReviewPersona, ReviewPhase, max_parallelism_limit,
    resolve_effective_parallelism, system_cpu_count,
};
use crate::ui::components::{ModelDropdown, ModelDropdownEvent, ModelDropdownMode};

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

#[derive(Clone)]
struct PersonaOption {
    value: String,
    label: SharedString,
}

impl SelectItem for PersonaOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

fn build_persona_options(personas: &[ReviewPersona]) -> Vec<PersonaOption> {
    let mut opts = Vec::with_capacity(personas.len() + 1);
    opts.push(PersonaOption {
        value: "None".to_string(),
        label: SharedString::from("None (Default)"),
    });
    for p in personas {
        opts.push(PersonaOption {
            value: p.name.clone(),
            label: SharedString::from(p.name.clone()),
        });
    }
    opts
}

fn pr_author_key(pr: &PrMetadata) -> String {
    let unique = pr.author_unique_name.trim();
    if !unique.is_empty() {
        return unique.to_string();
    }
    pr.author.trim().to_string()
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
    checkout_cleaned_up: bool,
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
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    cpu_count: usize,
    max_parallelism: usize,
    parallelism_slider: Entity<SliderState>,
    user_overrode_parallelism: bool,
    personas: Vec<ReviewPersona>,
    selected_persona: String,
    persona_select: Entity<SelectState<SearchableVec<PersonaOption>>>,
    custom_instructions_input: Entity<TextareaState>,
    show_diff_files: bool,
    is_reviewing: bool,
    has_reviewed: bool,
    review_cancel_tx: Option<watch::Sender<bool>>,
    phase_progress: Vec<PhaseProgress>,
    last_status_time: Option<String>,
    last_usage: Option<CopilotUsage>,
    comments: Vec<ReviewComment>,
    collapsed_comments: HashSet<String>,
    posting_comment_ids: HashSet<String>,
    comment_render_rev: u64,
    review_error: Option<String>,
    post_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl PrReviewerView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        copilot_service: Arc<CopilotService>,
        git_service: Arc<GitService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pr_service = Arc::new(PrReviewerService::new(
            Arc::clone(&git_service),
            Arc::clone(&copilot_service),
        ));

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

        let selected_model = if initial_settings.copilot_model.trim().is_empty() {
            "auto".to_string()
        } else {
            initial_settings.copilot_model.trim().to_string()
        };
        let model_dropdown = cx.new(|cx| {
            ModelDropdown::new(
                app_state.clone(),
                selected_model.clone(),
                Vec::new(),
                ModelDropdownMode::ToolOverride,
                window,
                cx,
            )
        });

        let cpu_count = system_cpu_count();
        let max_limit = max_parallelism_limit(cpu_count);
        let max_parallelism =
            resolve_effective_parallelism(None, initial_settings.max_parallelism, cpu_count);
        let parallelism_slider = cx.new(|_| {
            SliderState::new()
                .min(1.0)
                .max(max_limit.max(2) as f32)
                .step(1.0)
                .default_value(max_parallelism as f32)
        });

        let personas = initial_settings.pr_reviewer.personas.clone();
        let persona_options = build_persona_options(&personas);
        let persona_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(persona_options),
                Some(IndexPath::default().row(0)),
                window,
                cx,
            )
        });

        let custom_instructions_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "E.g., Focus on security, look out for proper error handling, verify database queries, etc.",
            )
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

        let model_sub = cx.subscribe(&model_dropdown, |this, _, event, cx| {
            let ModelDropdownEvent::Changed(model_id) = event;
            this.selected_model = model_id.clone();
            cx.notify();
        });

        let slider_sub = cx.subscribe(&parallelism_slider, |this, _, event, cx| {
            let limit = max_parallelism_limit(this.cpu_count);
            match event {
                SliderEvent::Change(val) | SliderEvent::Release(val) => {
                    let next = (val.start().round() as usize).clamp(1, limit);
                    this.max_parallelism = next;
                    this.user_overrode_parallelism = true;
                    cx.notify();
                }
            }
        });

        let persona_sub = cx.subscribe(&persona_select, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(value)) = event {
                let chosen = value.clone();
                this.selected_persona = chosen.clone();
                let active_pr = this
                    .checked_out
                    .as_ref()
                    .map(|c| &c.pr)
                    .or(this.selected_pr.as_ref());
                if let Some(author_key) = active_pr.map(pr_author_key).filter(|k| !k.is_empty()) {
                    this.app_state.update(cx, |state, cx| {
                        state.settings.author_personas.insert(author_key, chosen);
                        let _ = state.save_settings();
                        cx.notify();
                    });
                }
                cx.notify();
            }
        });

        let state_sub = cx.observe_in(&app_state, window, |this, app_state, window, cx| {
            let (is_active, code_ready, models, models_loading, next_personas, settings_parallel) = {
                let state = app_state.read(cx);
                (
                    state.active_view == ActiveView::PrReviewer,
                    is_code_source_configured(&state.settings),
                    state.copilot_models.clone(),
                    state.models_loading,
                    state.settings.pr_reviewer.personas.clone(),
                    state.settings.max_parallelism,
                )
            };
            this.reconcile_phase_models(&models, models_loading);
            if next_personas != this.personas {
                this.personas = next_personas;
                if this.selected_persona != "None"
                    && !this.personas.iter().any(|p| p.name == this.selected_persona)
                {
                    this.selected_persona = "None".to_string();
                }
                let options = build_persona_options(&this.personas);
                let chosen = this.selected_persona.clone();
                this.persona_select.update(cx, |select, cx| {
                    select.set_items(SearchableVec::new(options), window, cx);
                    select.set_selected_value(&chosen, window, cx);
                });
            }
            if !this.user_overrode_parallelism && this.cpu_count >= 4 {
                let synced =
                    resolve_effective_parallelism(None, settings_parallel, this.cpu_count);
                if synced != this.max_parallelism {
                    this.max_parallelism = synced;
                    this.parallelism_slider.update(cx, |slider, cx| {
                        slider.set_value(synced as f32, window, cx);
                    });
                }
            }
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
            model_dropdown,
            selected_model,
            cpu_count,
            max_parallelism,
            parallelism_slider,
            user_overrode_parallelism: false,
            personas,
            selected_persona: "None".to_string(),
            persona_select,
            custom_instructions_input,
            show_diff_files: false,
            is_reviewing: false,
            has_reviewed: false,
            review_cancel_tx: None,
            phase_progress: Vec::new(),
            last_status_time: None,
            last_usage: None,
            comments: Vec::new(),
            collapsed_comments: HashSet::new(),
            posting_comment_ids: HashSet::new(),
            comment_render_rev: 0,
            review_error: None,
            post_error: None,
            _subscriptions: vec![
                search_sub,
                manual_sub,
                repo_sub,
                model_sub,
                slider_sub,
                persona_sub,
                state_sub,
            ],
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

    fn apply_remembered_author_persona(
        &mut self,
        pr: &PrMetadata,
        settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let author_key = pr_author_key(pr);
        let remembered = if author_key.is_empty() {
            None
        } else {
            settings.author_personas.get(&author_key).or_else(|| {
                settings
                    .author_personas
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&author_key))
                    .map(|(_, v)| v)
            })
        };

        let chosen = match remembered {
            Some(name)
                if name == "None" || self.personas.iter().any(|p| p.name == *name) =>
            {
                name.clone()
            }
            _ => "None".to_string(),
        };

        self.selected_persona = chosen.clone();
        self.persona_select.update(cx, |select, cx| {
            select.set_selected_value(&chosen, window, cx);
        });
    }

    fn select_pr(&mut self, pr: PrMetadata, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cancel_tx) = self.review_cancel_tx.take() {
            let _ = cancel_tx.send(true);
        }

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
        self.comments.clear();
        self.collapsed_comments.clear();
        self.posting_comment_ids.clear();
        self.phase_progress.clear();
        self.last_status_time = None;
        self.last_usage = None;
        self.review_error = None;
        self.post_error = None;
        self.is_reviewing = false;
        self.has_reviewed = false;
        self.show_diff_files = false;

        self.apply_remembered_author_persona(&pr, &settings, window, cx);

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

            let settings_for_details = settings.clone();
            let view = cx.entity();
            window
                .spawn(cx, async move |cx| {
                    let res = spawn_tokio(async move {
                        let provider = build_code_review_provider(&settings_for_details)?;
                        provider.get_pr_details(&pr_lookup, None).await
                    })
                    .await;

                    let _ = cx.update(|window, cx| {
                        view.update(cx, |this, cx| {
                            if this.pr_detail_gen != detail_gen {
                                return;
                            }
                            this.loading_pr_details = false;
                            if let Ok(Ok(full_pr)) = res {
                                this.apply_remembered_author_persona(
                                    &full_pr,
                                    &settings,
                                    window,
                                    cx,
                                );
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
                                    checkout_cleaned_up: false,
                                });
                                this.is_header_collapsed = true;
                                this.checkout_error = None;
                                this.review_error = None;
                                this.post_error = None;
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
        if self.is_reviewing {
            return;
        }
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

    fn general_status_text(&self) -> String {
        let running: Vec<&PhaseProgress> = self
            .phase_progress
            .iter()
            .filter(|p| p.status == PhaseRunStatus::Running)
            .collect();
        if running.len() == 1 {
            let p = running[0];
            if let Some(ref st) = p.status_text {
                return format!("{}: {st}", p.title);
            }
            return format!("Running {}...", p.title);
        }
        if running.len() > 1 {
            return format!("Running {} review phases in parallel...", running.len());
        }
        if self.is_reviewing {
            return "Preparing review phases...".to_string();
        }
        "Idle".to_string()
    }

    fn pr_identifier_for_post(&self, checked: &CheckedOutState, cx: &App) -> String {
        checked
            .pr
            .url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if self.active_tab == PrSelectorTab::Manual {
                    let manual = self.manual_pr_input.read(cx).value().trim().to_string();
                    if !manual.is_empty() {
                        return manual;
                    }
                }
                checked.pr.id.clone()
            })
    }

    fn handle_start_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_reviewing {
            return;
        }
        let Some(checked) = self.checked_out.clone() else {
            return;
        };

        let enabled_phases: Vec<ReviewPhase> = self
            .phases
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.phase.clone())
            .collect();
        if enabled_phases.is_empty() {
            self.review_error =
                Some("Select at least one review phase in PR settings to start review.".to_string());
            cx.notify();
            return;
        }
        if let Some(bad) = enabled_phases.iter().find(|p| p.template_error.is_some()) {
            self.review_error = bad.template_error.clone();
            cx.notify();
            return;
        }

        let (settings, available_models) = {
            let state = self.app_state.read(cx);
            (state.settings.clone(), state.copilot_models.clone())
        };
        let custom_instructions = self.custom_instructions_input.read(cx).value().to_string();

        self.is_reviewing = true;
        self.has_reviewed = false;
        self.comments.clear();
        self.collapsed_comments.clear();
        self.posting_comment_ids.clear();
        self.review_error = None;
        self.post_error = None;
        self.last_usage = None;
        self.last_status_time = None;
        if let Some(ref mut co) = self.checked_out {
            co.checkout_cleaned_up = false;
        }
        self.phase_progress = enabled_phases
            .iter()
            .map(|phase| PhaseProgress {
                id: phase.id.clone(),
                title: phase.title.clone(),
                group: phase.group.clone(),
                status: PhaseRunStatus::Pending,
                status_text: None,
                reason: None,
                comment_count: 0,
            })
            .collect();

        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.review_cancel_tx = Some(cancel_tx);
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<PrReviewStreamEvent>();

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::PrReviewer,
                Some(format!("Reviewing PR #{}...", checked.pr.id)),
            );
            cx.notify();
        });
        cx.notify();

        let pr_service = Arc::clone(&self.pr_service);
        let req = PrReviewRequest {
            repo_path: checked.repo_path.clone(),
            pr: checked.pr.clone(),
            enabled_phase_ids: enabled_phases.into_iter().map(|p| p.id).collect(),
            model_override: self.selected_model.clone(),
            custom_instructions,
            max_parallelism: Some(self.max_parallelism),
            persona_name: self.selected_persona.clone(),
            settings: settings.clone(),
            available_models,
            skip_cleanup: false,
        };

        runtime().spawn(async move {
            if !pr_service.has_active_checkout(&req.repo_path).await {
                match req.pr.id.trim().parse::<u64>() {
                    Ok(pr_num) => {
                        let target_branch = if req.pr.target_branch.trim().is_empty() {
                            "main"
                        } else {
                            req.pr.target_branch.trim()
                        };
                        let expected_repo = (!req.pr.repository_name.trim().is_empty())
                            .then_some(req.pr.repository_name.as_str());
                        let code_provider = build_code_review_provider(&settings).ok();
                        if let Err(err) = pr_service
                            .checkout_pr(
                                &req.repo_path,
                                pr_num,
                                target_branch,
                                expected_repo,
                                code_provider.as_deref(),
                                &settings,
                            )
                            .await
                        {
                            let _ = ev_tx.send(PrReviewStreamEvent::Error {
                                message: format!("Failed to re-checkout PR before review: {err}"),
                                cleaned_up: false,
                            });
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = ev_tx.send(PrReviewStreamEvent::Error {
                            message: format!("Invalid PR ID: '{}'", req.pr.id),
                            cleaned_up: false,
                        });
                        return;
                    }
                }
            }

            pr_service.review_pr(req, cancel_rx, ev_tx).await;
        });

        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                while let Some(event) = ev_rx.recv().await {
                    let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
                    let _ = cx.update(|_, cx| {
                        view.update(cx, |this, cx| {
                            this.last_status_time = Some(timestamp);
                            match event {
                                PrReviewStreamEvent::PhaseSkipped {
                                    phase_id,
                                    phase_title,
                                    reason,
                                } => {
                                    if let Some(p) =
                                        this.phase_progress.iter_mut().find(|p| p.id == phase_id)
                                    {
                                        p.title = phase_title;
                                        p.status = PhaseRunStatus::Skipped;
                                        p.reason = Some(reason);
                                    }
                                }
                                PrReviewStreamEvent::PhaseStarted {
                                    phase_id,
                                    phase_title,
                                } => {
                                    if let Some(p) =
                                        this.phase_progress.iter_mut().find(|p| p.id == phase_id)
                                    {
                                        p.title = phase_title;
                                        p.status = PhaseRunStatus::Running;
                                    }
                                }
                                PrReviewStreamEvent::PhaseStatus {
                                    phase_id,
                                    phase_title,
                                    status,
                                } => {
                                    if let Some(p) =
                                        this.phase_progress.iter_mut().find(|p| p.id == phase_id)
                                    {
                                        p.title = phase_title;
                                        p.status_text = Some(status);
                                    }
                                }
                                PrReviewStreamEvent::Comment(comment) => {
                                    this.comments.push(comment);
                                }
                                PrReviewStreamEvent::PhaseCompleted {
                                    phase_id,
                                    phase_title,
                                    comment_count,
                                } => {
                                    if let Some(p) =
                                        this.phase_progress.iter_mut().find(|p| p.id == phase_id)
                                    {
                                        p.title = phase_title;
                                        p.status = PhaseRunStatus::Completed;
                                        p.comment_count = comment_count;
                                    }
                                }
                                PrReviewStreamEvent::Finished {
                                    comments,
                                    usage,
                                    cleaned_up,
                                } => {
                                    this.is_reviewing = false;
                                    this.has_reviewed = true;
                                    this.review_cancel_tx = None;
                                    this.last_usage = Some(usage);
                                    if this.comments.is_empty() && !comments.is_empty() {
                                        this.comments = comments;
                                    }
                                    if cleaned_up {
                                        if let Some(ref mut co) = this.checked_out {
                                            co.checkout_cleaned_up = true;
                                        }
                                    }
                                    app_state.update(cx, |state, cx| {
                                        state.set_task_status(ActiveView::PrReviewer, None);
                                        cx.notify();
                                    });
                                }
                                PrReviewStreamEvent::Cancelled { cleaned_up } => {
                                    this.is_reviewing = false;
                                    this.review_cancel_tx = None;
                                    if cleaned_up {
                                        if let Some(ref mut co) = this.checked_out {
                                            co.checkout_cleaned_up = true;
                                        }
                                    }
                                    app_state.update(cx, |state, cx| {
                                        state.set_task_status(ActiveView::PrReviewer, None);
                                        cx.notify();
                                    });
                                }
                                PrReviewStreamEvent::Error {
                                    message,
                                    cleaned_up,
                                } => {
                                    this.is_reviewing = false;
                                    this.review_cancel_tx = None;
                                    this.review_error = Some(message);
                                    if cleaned_up {
                                        if let Some(ref mut co) = this.checked_out {
                                            co.checkout_cleaned_up = true;
                                        }
                                    }
                                    app_state.update(cx, |state, cx| {
                                        state.set_task_status(ActiveView::PrReviewer, None);
                                        cx.notify();
                                    });
                                }
                            }
                            cx.notify();
                        });
                    });
                }

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.is_reviewing {
                            this.is_reviewing = false;
                            this.review_cancel_tx = None;
                            app_state.update(cx, |state, cx| {
                                state.set_task_status(ActiveView::PrReviewer, None);
                                cx.notify();
                            });
                            cx.notify();
                        }
                    });
                });
            })
            .detach();
    }

    fn handle_cancel_review(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel_tx) = self.review_cancel_tx.take() {
            let _ = cancel_tx.send(true);
            cx.notify();
        }
    }

    fn handle_post_comment(
        &mut self,
        comment: ReviewComment,
        edited_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(checked) = self.checked_out.as_ref() else {
            return;
        };
        if self.posting_comment_ids.contains(&comment.id) {
            return;
        }

        let final_text = edited_text.unwrap_or_else(|| comment.comment.clone());
        if final_text.trim().is_empty() {
            self.post_error = Some("Comment text cannot be empty.".to_string());
            cx.notify();
            return;
        }

        let is_edited = final_text != comment.comment;
        let request = PostPrCommentRequest {
            comment_type: comment.comment_type,
            file: comment.file.clone(),
            line: comment.line,
            comment: final_text.clone(),
            edited: is_edited,
            thread_id: None,
        };

        let pr_identifier = self.pr_identifier_for_post(checked, cx);
        let repo_path = checked.repo_path.clone();
        let settings = self.app_state.read(cx).settings.clone();
        let pr_service = Arc::clone(&self.pr_service);
        let comment_id = comment.id.clone();

        self.post_error = None;
        self.posting_comment_ids.insert(comment_id.clone());
        cx.notify();

        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let res = spawn_tokio(async move {
                pr_service
                    .post_pr_comment(&repo_path, &pr_identifier, &request, &settings)
                    .await
            })
            .await;

            let _ = view.update(cx, |this, cx| {
                this.posting_comment_ids.remove(&comment_id);
                match res {
                    Ok(Ok(())) => {
                        if let Some(existing) =
                            this.comments.iter_mut().find(|c| c.id == comment_id)
                        {
                            existing.comment = final_text;
                            existing.posted = true;
                        }
                        if is_edited {
                            this.comment_render_rev = this.comment_render_rev.wrapping_add(1);
                        }
                        this.collapsed_comments.insert(comment_id);
                    }
                    Ok(Err(err)) => {
                        this.post_error = Some(format!("Failed to post PR comment: {err}"));
                    }
                    Err(err) => {
                        this.post_error = Some(format!("Failed to post PR comment: {err}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_edit_comment_dialog(
        &mut self,
        comment: ReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        let initial_text = comment.comment.clone();
        let textarea = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Write your comment here...")
                .default_value(initial_text)
        });
        let file_label = match (comment.comment_type, comment.file.as_deref(), comment.line) {
            (ReviewCommentType::Line, Some(f), Some(l)) => Some(format!("{f}:{l}")),
            _ => None,
        };

        window.open_dialog(cx, move |dialog, _window, cx| {
            let theme = cx.theme();
            let textarea_for_body = textarea.clone();
            let textarea_for_post = textarea.clone();
            let view_for_post = view.clone();
            let comment_for_post = comment.clone();

            dialog
                .w(px(640.))
                .overlay_closable(true)
                .close_button(true)
                .title(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(LucideIcon::SquarePen).with_size(Size::Medium))
                        .child("Edit Comment"),
                )
                .child(
                    v_flex()
                        .gap_3()
                        .when_some(file_label.clone(), |col, loc| {
                            col.child(
                                div()
                                    .px_2p5()
                                    .py_1()
                                    .rounded(theme.radius)
                                    .bg(theme.sidebar)
                                    .border_1()
                                    .border_color(theme.border)
                                    .font_family(theme.mono_font_family.clone())
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(loc),
                            )
                        })
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child("Comment Content (Markdown Supported)"),
                        )
                        .child(Textarea::new(&textarea_for_body).h(px(240.))),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("pr-edit-comment-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("pr-edit-comment-post")
                                .primary()
                                .icon(Icon::new(LucideIcon::Send).small())
                                .label("Post")
                                .on_click(move |_, window, cx| {
                                    let updated_text =
                                        textarea_for_post.read(cx).value().to_string();
                                    window.close_dialog(cx);
                                    view_for_post.update(cx, |this, cx| {
                                        this.handle_post_comment(
                                            comment_for_post.clone(),
                                            Some(updated_text),
                                            cx,
                                        );
                                    });
                                }),
                        ),
                )
        });
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
                                        .label("Back to Review Workspace")
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
                                            .child(format!(
                                                "{} -> {}",
                                                checked.pr.source_branch, checked.pr.target_branch
                                            )),
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
                                            .border_color(if checked.checkout_cleaned_up {
                                                theme.border
                                            } else if checked.using_worktree {
                                                green.opacity(0.4)
                                            } else {
                                                amber.opacity(0.4)
                                            })
                                            .bg(if checked.checkout_cleaned_up {
                                                theme.secondary
                                            } else if checked.using_worktree {
                                                green.opacity(0.1)
                                            } else {
                                                amber.opacity(0.1)
                                            })
                                            .text_xs()
                                            .text_color(if checked.checkout_cleaned_up {
                                                theme.muted_foreground
                                            } else if checked.using_worktree {
                                                green
                                            } else {
                                                amber
                                            })
                                            .child(if checked.checkout_cleaned_up {
                                                "Checkout Cleaned"
                                            } else if checked.using_worktree {
                                                "Worktree"
                                            } else {
                                                "Local Checkout"
                                            }),
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
                                    .disabled(self.is_reviewing)
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
                        .mt_3()
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
                div().flex_1().min_h_0().w_full().child(
                    h_resizable("pr-reviewer-workspace-split")
                        .child(
                            resizable_panel()
                                .size(px(370.))
                                .size_range(px(300.)..px(520.))
                                .child(self.render_review_left_column(checked, &theme, cx)),
                        )
                        .child(
                            resizable_panel().child(self.render_review_comments_column(&theme, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_review_left_column(
        &self,
        checked: &CheckedOutState,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let enabled_phases = self.phases.iter().filter(|p| p.enabled).count();
        let show_progress = self.is_reviewing || !self.phase_progress.is_empty();

        if show_progress {
            let active_phases: Vec<&PhaseProgress> = self
                .phase_progress
                .iter()
                .filter(|p| p.status != PhaseRunStatus::Skipped)
                .collect();
            let total = active_phases.len();
            let completed = active_phases
                .iter()
                .filter(|p| p.status == PhaseRunStatus::Completed)
                .count();
            let percent = if total > 0 {
                (completed * 100) / total
            } else {
                0
            };

            return v_flex()
                .size_full()
                .p_5()
                .gap_4()
                .bg(theme.sidebar)
                .border_r_1()
                .border_color(theme.border)
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
                                .child(Icon::new(LucideIcon::Bot).small().text_color(theme.primary))
                                .child("Review Progress"),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded(theme.radius)
                                .bg(theme.primary.opacity(0.12))
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.primary)
                                .child(format!("{percent}% Complete")),
                        ),
                )
                .child(
                    div()
                        .h(px(8.))
                        .w_full()
                        .rounded(theme.radius)
                        .bg(theme.secondary)
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(relative(percent as f32 / 100.0))
                                .bg(theme.primary),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(
                            v_flex()
                                .gap_2()
                                .children(self.phase_progress.iter().map(|phase| {
                                    let is_running = phase.status == PhaseRunStatus::Running;
                                    let is_completed = phase.status == PhaseRunStatus::Completed;
                                    let is_skipped = phase.status == PhaseRunStatus::Skipped;

                                    let status_icon = match phase.status {
                                        PhaseRunStatus::Pending => Icon::new(LucideIcon::Clock)
                                            .small()
                                            .text_color(theme.muted_foreground)
                                            .into_any_element(),
                                        PhaseRunStatus::Running => Spinner::new()
                                            .small()
                                            .color(theme.primary)
                                            .into_any_element(),
                                        PhaseRunStatus::Completed => {
                                            Icon::new(LucideIcon::CircleCheck)
                                                .small()
                                                .text_color(green)
                                                .into_any_element()
                                        }
                                        PhaseRunStatus::Skipped => Icon::new(LucideIcon::SkipForward)
                                            .small()
                                            .text_color(amber)
                                            .into_any_element(),
                                    };

                                    let (badge_text, badge_color) = match phase.status {
                                        PhaseRunStatus::Pending => ("Pending".to_string(), theme.muted_foreground),
                                        PhaseRunStatus::Running => ("Running".to_string(), theme.primary),
                                        PhaseRunStatus::Completed => (
                                            format!(
                                                "Done ({} draft comment{})",
                                                phase.comment_count,
                                                if phase.comment_count == 1 { "" } else { "s" }
                                            ),
                                            green,
                                        ),
                                        PhaseRunStatus::Skipped => ("Skipped".to_string(), amber),
                                    };

                                    let subtitle = if is_running {
                                        Some(
                                            phase
                                                .status_text
                                                .clone()
                                                .unwrap_or_else(|| "Analyzing diffs...".to_string()),
                                        )
                                    } else if is_skipped {
                                        phase.reason.clone()
                                    } else {
                                        None
                                    };

                                    v_flex()
                                        .p_3()
                                        .gap_1p5()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(if is_running {
                                            theme.primary.opacity(0.45)
                                        } else {
                                            theme.border
                                        })
                                        .bg(if is_running {
                                            theme.primary.opacity(0.08)
                                        } else {
                                            theme.background
                                        })
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    h_flex()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .items_center()
                                                        .gap_2()
                                                        .child(status_icon)
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .text_xs()
                                                                .font_weight(FontWeight::SEMIBOLD)
                                                                .text_color(if is_completed || is_skipped {
                                                                    theme.muted_foreground
                                                                } else {
                                                                    theme.foreground
                                                                })
                                                                .truncate()
                                                                .child(phase.title.clone()),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded(theme.radius)
                                                        .border_1()
                                                        .border_color(badge_color.opacity(0.35))
                                                        .bg(badge_color.opacity(0.1))
                                                        .font_family(theme.mono_font_family.clone())
                                                        .text_xs()
                                                        .text_color(badge_color)
                                                        .child(badge_text),
                                                ),
                                        )
                                        .when(!phase.group.trim().is_empty(), |col| {
                                            col.child(
                                                div()
                                                    .pl_5()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(phase.group.clone()),
                                            )
                                        })
                                        .when_some(subtitle, |col, sub| {
                                            col.child(
                                                div()
                                                    .pl_5()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .truncate()
                                                    .child(sub),
                                            )
                                        })
                                })),
                        ),
                )
                .child(if self.is_reviewing {
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .p_2p5()
                                .gap_2()
                                .items_center()
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(Spinner::new().small().color(theme.primary))
                                .child(div().flex_1().truncate().child(self.general_status_text())),
                        )
                        .child(
                            Button::new("pr-cancel-review")
                                .danger()
                                .small()
                                .icon(Icon::new(LucideIcon::CircleX).small())
                                .label("Cancel Review")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.handle_cancel_review(cx);
                                })),
                        )
                        .into_any_element()
                } else {
                    Button::new("pr-back-to-review-settings")
                        .ghost()
                        .small()
                        .icon(Icon::new(LucideIcon::ArrowLeft).small())
                        .label("Back to Settings")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.phase_progress.clear();
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .into_any_element();
        }

        let max_limit = max_parallelism_limit(self.cpu_count);

        v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .p_5()
            .gap_4()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(Icon::new(LucideIcon::Bot).small().text_color(theme.primary))
                    .child("Review Settings"),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Copilot Model"),
                    )
                    .child(self.model_dropdown.clone()),
            )
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
                                    .text_color(theme.muted_foreground)
                                    .child("Review Agent Parallelism"),
                            )
                            .when(self.cpu_count >= 4, |row| {
                                row.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(theme.radius)
                                        .bg(theme.secondary)
                                        .font_family(theme.mono_font_family.clone())
                                        .text_xs()
                                        .child(format!("{}x", self.max_parallelism)),
                                )
                            }),
                    )
                    .child(if self.cpu_count < 4 {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Parallelism fixed at 1 (fewer than 4 CPU cores).")
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_1()
                            .child(Slider::new(&self.parallelism_slider).horizontal())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "Range: 1 to {max_limit} workers (CPUs: {})",
                                        self.cpu_count
                                    )),
                            )
                            .into_any_element()
                    }),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Review Persona (Optional)"),
                    )
                    .child(Select::new(&self.persona_select)),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Specific Instructions (Optional)"),
                    )
                    .child(Textarea::new(&self.custom_instructions_input).h(px(104.))),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        Button::new("pr-start-review")
                            .primary()
                            .disabled(self.is_reviewing || enabled_phases == 0)
                            .icon(Icon::new(LucideIcon::Play).small())
                            .label("Start Code Review")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.handle_start_review(window, cx);
                            })),
                    )
                    .when(enabled_phases == 0, |col| {
                        col.child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .text_xs()
                                .text_color(amber)
                                .child(Icon::new(LucideIcon::TriangleAlert).xsmall())
                                .child("Select at least one phase in PR settings to start review."),
                        )
                    }),
            )
            .child(
                v_flex()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(format!("Changed Files ({})", checked.diff_files.len())),
                            )
                            .child(
                                Button::new("pr-toggle-diff-files")
                                    .ghost()
                                    .xsmall()
                                    .icon(
                                        Icon::new(if self.show_diff_files {
                                            LucideIcon::ChevronUp
                                        } else {
                                            LucideIcon::ChevronDown
                                        })
                                        .xsmall(),
                                    )
                                    .label(if self.show_diff_files { "Hide" } else { "Show" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_diff_files = !this.show_diff_files;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(self.show_diff_files, |col| {
                        col.child(
                            v_flex()
                                .gap_1p5()
                                .when(!checked.checkout_cleaned_up, |inner| {
                                    inner.child(
                                        div()
                                            .text_xs()
                                            .font_family(theme.mono_font_family.clone())
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(format!(
                                                "Working dir: {}",
                                                checked.effective_repo_path.display()
                                            )),
                                    )
                                })
                                .child(
                                    v_flex()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.background)
                                        .overflow_hidden()
                                        .child(if checked.diff_files.is_empty() {
                                            div()
                                                .p_3()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child("No changed files detected against target branch.")
                                                .into_any_element()
                                        } else {
                                            v_flex()
                                                .children(checked.diff_files.iter().enumerate().map(|(i, file)| {
                                                    let (icon, color) = diff_status_style(file.status, theme);
                                                    h_flex()
                                                        .items_center()
                                                        .justify_between()
                                                        .gap_2()
                                                        .px_2p5()
                                                        .py_1p5()
                                                        .when(i > 0, |row| {
                                                            row.border_t_1().border_color(theme.border)
                                                        })
                                                        .child(
                                                            h_flex()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .items_center()
                                                                .gap_1p5()
                                                                .text_color(color)
                                                                .child(Icon::new(icon).xsmall())
                                                                .child(
                                                                    div()
                                                                        .flex_1()
                                                                        .min_w_0()
                                                                        .font_family(theme.mono_font_family.clone())
                                                                        .text_xs()
                                                                        .text_color(theme.foreground)
                                                                        .truncate()
                                                                        .child(file.path.clone()),
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(color)
                                                                .child(file.status.label()),
                                                        )
                                                }))
                                                .into_any_element()
                                        }),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_review_comments_column(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let blue = hsla(212.0 / 360.0, 0.88, 0.56, 1.0);
        let info_surface = hsla(212.0 / 360.0, 0.70, 0.50, 0.10);

        let token_summary = self.last_usage.as_ref().map(|u| {
            format!(
                "{} in / {} out / {} cached tokens",
                u.input_tokens, u.output_tokens, u.cache_read_tokens
            )
        });

        v_flex()
            .size_full()
            .p_5()
            .gap_3()
            .bg(theme.background)
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
                            .child(
                                Icon::new(LucideIcon::MessagesSquare)
                                    .small()
                                    .text_color(theme.primary),
                            )
                            .child(format!("Review Comments ({})", self.comments.len())),
                    )
                    .when(self.critic_enabled && !self.is_reviewing, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Drafts (Critic execution arrives in Phase 11)"),
                        )
                    }),
            )
            .when_some(self.review_error.clone(), |col, err| {
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
            .when_some(self.post_error.clone(), |col, err| {
                col.child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .items_center()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(red.opacity(0.4))
                        .bg(red.opacity(0.08))
                        .text_xs()
                        .text_color(red)
                        .child(Icon::new(LucideIcon::CircleAlert).small())
                        .child(div().flex_1().child(err))
                        .child(
                            Button::new("pr-dismiss-post-error")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(LucideIcon::X).xsmall())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.post_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when(self.is_reviewing && !self.comments.is_empty(), |col| {
                col.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(blue.opacity(0.35))
                        .bg(info_surface)
                        .text_xs()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .text_color(blue)
                                .child(Spinner::new().small().color(blue))
                                .child(format!("Status: {}", self.general_status_text())),
                        )
                        .when_some(self.last_status_time.clone(), |row, t| {
                            row.child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.muted_foreground)
                                    .child(format!("Last update: {t}")),
                            )
                        }),
                )
            })
            .when(!self.is_reviewing && self.has_reviewed && !self.comments.is_empty(), |col| {
                col.child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(green.opacity(0.35))
                        .bg(green.opacity(0.08))
                        .text_xs()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .text_color(green)
                                .child(Icon::new(LucideIcon::CircleCheck).small())
                                .child("Review complete"),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_3()
                                .text_color(theme.muted_foreground)
                                .when_some(token_summary.clone(), |row, summary| row.child(summary))
                                .when_some(self.last_status_time.clone(), |row, t| {
                                    row.child(
                                        div()
                                            .font_family(theme.mono_font_family.clone())
                                            .child(format!("Completed at: {t}")),
                                    )
                                }),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(if self.comments.is_empty() {
                        v_flex()
                            .size_full()
                            .min_h(px(280.))
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .p_6()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(if self.is_reviewing {
                                v_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Spinner::new().color(theme.primary))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(self.general_status_text()),
                                    )
                                    .when_some(self.last_status_time.clone(), |col, t| {
                                        col.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!("Last update: {t}")),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Copilot is analyzing the repository. Comments will appear here as they are generated."),
                                    )
                                    .into_any_element()
                            } else if self.has_reviewed {
                                v_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Icon::new(LucideIcon::CircleCheck).with_size(Size::Large).text_color(green))
                                    .child(
                                        div()
                                            .text_base()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(green)
                                            .child("Review complete"),
                                    )
                                    .when_some(token_summary, |col, summary| {
                                        col.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(summary),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("No comments were suggested."),
                                    )
                                    .into_any_element()
                            } else {
                                v_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Icon::new(LucideIcon::MessagesSquare)
                                            .with_size(Size::Large)
                                            .text_color(theme.muted_foreground),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("No comments generated yet"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Configure instructions on the left and click \"Start Code Review\" to begin."),
                                    )
                                    .into_any_element()
                            })
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_3()
                            .children(self.comments.iter().enumerate().map(|(idx, comment)| {
                                let comment_id = comment.id.clone();
                                let is_collapsed = self.collapsed_comments.contains(&comment_id);
                                let is_posting = self.posting_comment_ids.contains(&comment_id);
                                let is_line = comment.comment_type == ReviewCommentType::Line;
                                let location_label = match (comment.file.as_deref(), comment.line) {
                                    (Some(f), Some(l)) => Some(format!("{f}:{l}")),
                                    (Some(f), None) => Some(f.to_string()),
                                    _ => None,
                                };

                                if is_collapsed {
                                    let id_for_expand = comment_id.clone();
                                    return h_flex()
                                        .p_2p5()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.sidebar)
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .items_center()
                                                .gap_2()
                                                .when_some(comment.phase.clone(), |row, phase| {
                                                    row.child(
                                                        div()
                                                            .px_1p5()
                                                            .py_0p5()
                                                            .rounded(theme.radius)
                                                            .bg(info_surface)
                                                            .text_xs()
                                                            .text_color(blue)
                                                            .child(phase),
                                                    )
                                                })
                                                .child(if comment.posted {
                                                    h_flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .text_xs()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(green)
                                                        .child(Icon::new(LucideIcon::CircleCheck).xsmall())
                                                        .child("Posted to PR")
                                                        .into_any_element()
                                                } else {
                                                    h_flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child(Icon::new(LucideIcon::EyeOff).xsmall())
                                                        .child("Dismissed")
                                                        .into_any_element()
                                                })
                                                .when(is_line, |row| {
                                                    row.when_some(location_label.clone(), |r, loc| {
                                                        r.child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .font_family(theme.mono_font_family.clone())
                                                                .text_xs()
                                                                .text_color(theme.muted_foreground)
                                                                .truncate()
                                                                .child(loc),
                                                        )
                                                    })
                                                }),
                                        )
                                        .child(
                                            Button::new(("pr-expand-comment", idx))
                                                .ghost()
                                                .xsmall()
                                                .icon(Icon::new(LucideIcon::ChevronDown).xsmall())
                                                .label("Expand")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.collapsed_comments.remove(&id_for_expand);
                                                    cx.notify();
                                                })),
                                        )
                                        .into_any_element();
                                }

                                let id_for_dismiss = comment_id.clone();
                                let comment_for_post = comment.clone();
                                let comment_for_edit = comment.clone();
                                let md_id = SharedString::from(format!(
                                    "pr-comment-md-{}-{}",
                                    comment.id, self.comment_render_rev
                                ));

                                v_flex()
                                    .p_4()
                                    .gap_3()
                                    .rounded(theme.radius_lg)
                                    .border_1()
                                    .border_color(theme.border)
                                    .when(is_line, |card| {
                                        card.border_l(px(4.)).border_color(theme.primary)
                                    })
                                    .bg(theme.sidebar)
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .pb_2()
                                            .border_b_1()
                                            .border_color(theme.border)
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .when_some(comment.phase.clone(), |row, phase| {
                                                        row.child(
                                                            div()
                                                                .px_2()
                                                                .py_0p5()
                                                                .rounded(theme.radius)
                                                                .bg(info_surface)
                                                                .text_xs()
                                                                .font_weight(FontWeight::MEDIUM)
                                                                .text_color(blue)
                                                                .child(phase),
                                                        )
                                                    })
                                                    .when(!is_line, |row| {
                                                        row.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(theme.muted_foreground)
                                                                .child("General PR Comment"),
                                                        )
                                                    }),
                                            )
                                            .when_some(location_label, |row, loc| {
                                                row.child(
                                                    div()
                                                        .min_w_0()
                                                        .max_w(px(380.))
                                                        .font_family(theme.mono_font_family.clone())
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .truncate()
                                                        .child(loc),
                                                )
                                            }),
                                    )
                                    .when(
                                        is_line
                                            && comment
                                                .code_lines
                                                .as_ref()
                                                .is_some_and(|lines| !lines.is_empty()),
                                        |card| {
                                            let code_lines = comment
                                                .code_lines
                                                .as_ref()
                                                .map(|v| v.as_slice())
                                                .unwrap_or(&[]);
                                            card.child(render_code_context_block(
                                                code_lines,
                                                theme,
                                            ))
                                        },
                                    )
                                    .child(
                                        div().text_sm().text_color(theme.foreground).child(
                                            TextView::markdown(md_id, comment.comment.clone())
                                                .selectable(true),
                                        ),
                                    )
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .justify_end()
                                            .gap_2()
                                            .pt_2()
                                            .border_t_1()
                                            .border_color(theme.border)
                                            .child(
                                                Button::new(("pr-dismiss-comment", idx))
                                                    .ghost()
                                                    .small()
                                                    .icon(Icon::new(LucideIcon::EyeOff).small())
                                                    .label("Dismiss")
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.collapsed_comments
                                                            .insert(id_for_dismiss.clone());
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                Button::new(("pr-edit-comment", idx))
                                                    .small()
                                                    .disabled(is_posting)
                                                    .icon(Icon::new(LucideIcon::SquarePen).small())
                                                    .label("Edit")
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.open_edit_comment_dialog(
                                                                comment_for_edit.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                            .child(
                                                Button::new(("pr-post-comment", idx))
                                                    .primary()
                                                    .small()
                                                    .disabled(is_posting)
                                                    .loading(is_posting)
                                                    .icon(Icon::new(LucideIcon::Send).small())
                                                    .label(if is_posting { "Posting..." } else { "Post" })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.handle_post_comment(
                                                            comment_for_post.clone(),
                                                            None,
                                                            cx,
                                                        );
                                                    })),
                                            ),
                                    )
                                    .into_any_element()
                            }))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }
}

fn render_code_context_block(code_lines: &[CodeContextLine], theme: &Theme) -> AnyElement {
    let code_bg = hsla(0.0, 0.0, 0.12, 1.0);
    let line_num_fg = hsla(0.0, 0.0, 0.52, 1.0);
    let code_fg = hsla(0.0, 0.0, 0.90, 1.0);
    let target_bg = hsla(54.0 / 360.0, 1.0, 0.62, 0.15);
    let target_border = hsla(54.0 / 360.0, 1.0, 0.62, 1.0);

    div()
        .w_full()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(code_bg)
        .overflow_hidden()
        .child(
            div().w_full().py_1p5().overflow_x_scrollbar().child(
                v_flex().min_w_full().children(code_lines.iter().map(|line_obj| {
                    h_flex()
                        .min_w_full()
                        .items_center()
                        .py_0p5()
                        .pr_3()
                        .pl_1()
                        .border_l(px(3.))
                        .border_color(if line_obj.is_target {
                            target_border
                        } else {
                            target_border.opacity(0.0)
                        })
                        .when(line_obj.is_target, |row| row.bg(target_bg))
                        .child(
                            h_flex()
                                .w(px(40.))
                                .pr_3()
                                .justify_end()
                                .flex_shrink_0()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .text_color(line_num_fg)
                                .child(line_obj.line.to_string()),
                        )
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .text_color(code_fg)
                                .whitespace_nowrap()
                                .child(if line_obj.text.is_empty() {
                                    " ".to_string()
                                } else {
                                    line_obj.text.clone()
                                }),
                        )
                })),
            ),
        )
        .into_any_element()
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
