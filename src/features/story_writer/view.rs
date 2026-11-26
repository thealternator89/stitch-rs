use std::collections::HashMap;

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
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window, div, hsla, prelude::FluentBuilder as _, px,
};

use crate::app_state::{ActiveView, AppState};
use crate::features::settings::SettingsTab;
use crate::features::story_writer::service::{
    self, ActiveStoryGenerationHandle, StoryGenerationRequest, StoryGenerationStreamEvent,
};
use crate::infrastructure::notifications::NotificationService;
use crate::infrastructure::providers::confluence::strip_html_for_preview;
use crate::infrastructure::providers::{build_documentation_provider, build_issue_tracker_provider};
use crate::infrastructure::runtime::spawn_tokio;
use crate::types::{AppSettings, CopilotUsage, DocPageData, StoryData, TicketData};
use crate::ui::components::{
    AutocompleteEvent, AutocompleteInput, AutocompleteOption, DirectIdMode, ModelDropdown,
    ModelDropdownEvent, ModelDropdownMode, ResetState, normalize_default_model,
    open_reset_confirm_modal, open_timeout_modal, render_tool_reset_button,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryCardStatus {
    Draft,
    Creating,
    Created,
    Discarded,
}

struct StoryCardEditor {
    title_input: Entity<InputState>,
    description_input: Entity<TextareaState>,
    ac_input: Entity<TextareaState>,
    notes_input: Entity<TextareaState>,
}

pub struct StoryCardItem {
    pub id: usize,
    pub story: StoryData,
    pub collapsed: bool,
    pub status: StoryCardStatus,
    pub edited: bool,
    editing: Option<StoryCardEditor>,
}

pub struct StoryWriterView {
    app_state: Entity<AppState>,
    page_search: Entity<AutocompleteInput>,
    page_search_cache: HashMap<String, DocPageData>,
    selected_page_id: Option<String>,
    fetched_page: Option<DocPageData>,
    fetching_page: bool,
    page_fetch_gen: u64,
    feature_search: Entity<AutocompleteInput>,
    feature_search_cache: HashMap<String, TicketData>,
    selected_feature_id: Option<String>,
    fetched_feature: Option<TicketData>,
    fetching_feature: bool,
    feature_fetch_gen: u64,
    additional_context_input: Entity<TextareaState>,
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,
    stories: Vec<StoryCardItem>,
    next_story_id: usize,
    is_generating: bool,
    generation_id: u64,
    active_handle: Option<ActiveStoryGenerationHandle>,
    generation_usage: Option<CopilotUsage>,
    last_db_session_id: Option<i64>,
    card_errors: HashMap<usize, String>,
    created_ticket_ids: HashMap<usize, String>,
    validation_error: Option<String>,
    status_notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl StoryWriterView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_model = normalize_default_model(&initial_settings.copilot_model);

        let initial_feature_mode = feature_id_mode_for(initial_settings);

        let page_search = cx.new(|cx| {
            AutocompleteInput::new(
                "story-page-search",
                "Search Confluence page by title or ID...",
                DirectIdMode::NumericOnly,
                "No Confluence pages found.",
                "Searching pages...",
                window,
                cx,
            )
        });

        let feature_search = cx.new(|cx| {
            AutocompleteInput::new(
                "story-feature-search",
                "Search features by title or ID...",
                initial_feature_mode,
                "No features found.",
                "Searching features...",
                window,
                cx,
            )
        });

        let additional_context_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("e.g., focus on backend APIs or split stories by component...")
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
                    feature_id_mode_for(settings),
                )
            };
            if !this.user_overrode_model && default_model != this.selected_model {
                this.selected_model = default_model.clone();
                this.model_dropdown.update(cx, |dd, cx| {
                    dd.set_selected_model(default_model, window, cx);
                });
            }
            this.feature_search.update(cx, |input, cx| {
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

        let page_sub = cx.subscribe_in(&page_search, window, |this, _, event, window, cx| {
            this.handle_page_autocomplete_event(event.clone(), window, cx);
        });

        let feature_sub =
            cx.subscribe_in(&feature_search, window, |this, _, event, window, cx| {
                this.handle_feature_autocomplete_event(event.clone(), window, cx);
            });

        let context_sub = cx.subscribe(&additional_context_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        Self {
            app_state,
            page_search,
            page_search_cache: HashMap::new(),
            selected_page_id: None,
            fetched_page: None,
            fetching_page: false,
            page_fetch_gen: 0,
            feature_search,
            feature_search_cache: HashMap::new(),
            selected_feature_id: None,
            fetched_feature: None,
            fetching_feature: false,
            feature_fetch_gen: 0,
            additional_context_input,
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            stories: Vec::new(),
            next_story_id: 1,
            is_generating: false,
            generation_id: 0,
            active_handle: None,
            generation_usage: None,
            last_db_session_id: None,
            card_errors: HashMap::new(),
            created_ticket_ids: HashMap::new(),
            validation_error: None,
            status_notice: None,
            _subscriptions: vec![state_sub, model_sub, page_sub, feature_sub, context_sub],
        }
    }

    fn reset_state(&self, cx: &App) -> ResetState {
        let any_creating = self
            .stories
            .iter()
            .any(|s| s.status == StoryCardStatus::Creating);
        if self.is_generating || any_creating {
            return ResetState::NeedsConfirmation { is_running: true };
        }
        if !self.stories.is_empty() {
            return ResetState::NeedsConfirmation { is_running: false };
        }
        let has_dirty_inputs = self.page_search.read(cx).has_value(cx)
            || self.feature_search.read(cx).has_value(cx)
            || self.selected_page_id.is_some()
            || self.fetched_page.is_some()
            || self.fetching_page
            || self.selected_feature_id.is_some()
            || self.fetched_feature.is_some()
            || self.fetching_feature
            || !self
                .additional_context_input
                .read(cx)
                .value()
                .trim()
                .is_empty()
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
                    "This will cancel the active operation and clear all drafted stories, edits, and selected Confluence/Feature inputs."
                } else {
                    "This will clear all generated stories, unsaved edits, and selected Confluence/Feature inputs."
                };
                open_reset_confirm_modal(
                    "Story Writer",
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
        self.generation_id = self.generation_id.wrapping_add(1);
        self.page_fetch_gen = self.page_fetch_gen.wrapping_add(1);
        self.feature_fetch_gen = self.feature_fetch_gen.wrapping_add(1);
        if let Some(handle) = self.active_handle.take() {
            handle.cancel();
        }
        self.page_search.update(cx, |input, cx| input.clear(window, cx));
        self.feature_search
            .update(cx, |input, cx| input.clear(window, cx));
        self.page_search_cache.clear();
        self.selected_page_id = None;
        self.fetched_page = None;
        self.fetching_page = false;
        self.feature_search_cache.clear();
        self.selected_feature_id = None;
        self.fetched_feature = None;
        self.fetching_feature = false;
        self.additional_context_input
            .update(cx, |input, cx| input.set_value("", window, cx));

        let default_model =
            normalize_default_model(&self.app_state.read(cx).settings.copilot_model);
        self.user_overrode_model = false;
        self.selected_model = default_model.clone();
        self.model_dropdown.update(cx, |dd, cx| {
            dd.set_selected_model(default_model, window, cx);
        });

        self.stories.clear();
        self.is_generating = false;
        self.generation_usage = None;
        self.last_db_session_id = None;
        self.card_errors.clear();
        self.created_ticket_ids.clear();
        self.validation_error = None;
        self.status_notice = None;

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::StoryWriter, None);
            cx.notify();
        });
        cx.notify();
    }

    pub fn upsert_story(&mut self, story: StoryData, cx: &mut Context<Self>) {
        upsert_story_in_list(&mut self.stories, &mut self.next_story_id, story);
        cx.notify();
    }

    fn handle_page_autocomplete_event(
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
                            let provider = build_documentation_provider(&settings)?;
                            provider.search_pages(&query).await
                        })
                        .await;

                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                let mapped = match res {
                                    Ok(Ok(pages)) => {
                                        let mut opts = Vec::with_capacity(pages.len());
                                        for page in pages {
                                            this.page_search_cache
                                                .insert(page.id.clone(), page.clone());
                                            opts.push(AutocompleteOption {
                                                id: page.id,
                                                title: page.title,
                                                subtitle: None,
                                            });
                                        }
                                        Ok(opts)
                                    }
                                    Ok(Err(err)) => Err(err.to_string()),
                                    Err(err) => Err(err.to_string()),
                                };
                                this.page_search.update(cx, |input, cx| {
                                    input.set_results(request_id, mapped, cx);
                                });
                            });
                        });
                    })
                    .detach();
            }
            AutocompleteEvent::Selected(opt) => {
                self.selected_page_id = Some(opt.id.clone());
                self.validation_error = None;
                self.status_notice = None;
                if let Some(cached) = self.page_search_cache.get(&opt.id).cloned() {
                    if !cached.body.is_empty() {
                        self.fetched_page = Some(cached);
                        cx.notify();
                        return;
                    }
                }
                self.fetch_page_by_id(&opt.id, window, cx);
            }
            AutocompleteEvent::DirectValueChanged(maybe_id) => {
                self.selected_page_id = maybe_id;
                if self.selected_page_id.is_none() {
                    self.page_fetch_gen = self.page_fetch_gen.wrapping_add(1);
                    self.fetched_page = None;
                    self.fetching_page = false;
                }
                cx.notify();
            }
            AutocompleteEvent::Cleared => {
                self.page_fetch_gen = self.page_fetch_gen.wrapping_add(1);
                self.selected_page_id = None;
                self.fetched_page = None;
                self.fetching_page = false;
                self.page_search_cache.clear();
                cx.notify();
            }
        }
    }

    fn handle_feature_autocomplete_event(
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
                            let feature_type = settings.feature_type.trim();
                            let filter = if feature_type.is_empty() {
                                None
                            } else {
                                Some(feature_type)
                            };
                            provider.search_tickets(&query, filter).await
                        })
                        .await;

                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                let mapped = match res {
                                    Ok(Ok(tickets)) => {
                                        let mut opts = Vec::with_capacity(tickets.len());
                                        for ticket in tickets {
                                            if let Some(id) = ticket.id.clone() {
                                                this.feature_search_cache
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
                                this.feature_search.update(cx, |input, cx| {
                                    input.set_results(request_id, mapped, cx);
                                });
                            });
                        });
                    })
                    .detach();
            }
            AutocompleteEvent::Selected(opt) => {
                self.selected_feature_id = Some(opt.id.clone());
                self.validation_error = None;
                self.status_notice = None;
                if let Some(cached) = self.feature_search_cache.get(&opt.id).cloned() {
                    self.fetched_feature = Some(cached);
                    cx.notify();
                } else {
                    self.fetch_feature_by_id(&opt.id, window, cx);
                }
            }
            AutocompleteEvent::DirectValueChanged(maybe_id) => {
                self.selected_feature_id = maybe_id;
                if self.selected_feature_id.is_none() {
                    self.feature_fetch_gen = self.feature_fetch_gen.wrapping_add(1);
                    self.fetched_feature = None;
                    self.fetching_feature = false;
                }
                cx.notify();
            }
            AutocompleteEvent::Cleared => {
                self.feature_fetch_gen = self.feature_fetch_gen.wrapping_add(1);
                self.selected_feature_id = None;
                self.fetched_feature = None;
                self.fetching_feature = false;
                self.feature_search_cache.clear();
                cx.notify();
            }
        }
    }

    fn fetch_page_by_id(&mut self, page_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.app_state.read(cx).settings.clone();
        let page_id_owned = page_id.trim().to_string();
        if page_id_owned.is_empty() {
            return;
        }

        self.page_fetch_gen = self.page_fetch_gen.wrapping_add(1);
        let generation = self.page_fetch_gen;
        self.fetching_page = true;
        self.validation_error = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_documentation_provider(&settings)?;
                    provider.fetch_page(&page_id_owned).await
                })
                .await;

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.page_fetch_gen != generation {
                            return;
                        }
                        this.fetching_page = false;
                        match res {
                            Ok(Ok(page)) => {
                                this.page_search_cache.insert(page.id.clone(), page.clone());
                                this.fetched_page = Some(page);
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

    fn fetch_feature_by_id(
        &mut self,
        feature_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.app_state.read(cx).settings.clone();
        let feature_id_owned = feature_id.trim().to_string();
        if feature_id_owned.is_empty() {
            return;
        }

        self.feature_fetch_gen = self.feature_fetch_gen.wrapping_add(1);
        let generation = self.feature_fetch_gen;
        self.fetching_feature = true;
        self.validation_error = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_issue_tracker_provider(&settings)?;
                    provider.fetch_ticket(&feature_id_owned).await
                })
                .await;

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.feature_fetch_gen != generation {
                            return;
                        }
                        this.fetching_feature = false;
                        match res {
                            Ok(Ok(ticket)) => {
                                if let Some(id) = ticket.id.clone() {
                                    this.feature_search_cache.insert(id, ticket.clone());
                                }
                                this.fetched_feature = Some(ticket);
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

    fn handle_generate_stories(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_generating {
            return;
        }

        let Some(page_id) = self
            .selected_page_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            self.validation_error =
                Some("Please search for a Confluence page or enter a numeric Page ID.".to_string());
            cx.notify();
            return;
        };

        let Some(feature_id) = self
            .selected_feature_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            self.validation_error = Some(
                "Please search for a parent Feature or enter a Feature ID.".to_string(),
            );
            cx.notify();
            return;
        };

        let additional_context = self.additional_context_input.read(cx).value().to_string();
        let model_override = self.selected_model.clone();
        let (copilot, settings) = {
            let state = self.app_state.read(cx);
            (state.copilot_service.clone(), state.settings.clone())
        };

        self.validation_error = None;
        self.status_notice = None;
        self.stories.clear();
        self.card_errors.clear();
        self.created_ticket_ids.clear();
        self.generation_usage = None;
        self.last_db_session_id = None;
        self.is_generating = true;
        self.fetching_page = true;
        self.fetching_feature = true;
        self.generation_id = self.generation_id.wrapping_add(1);
        self.page_fetch_gen = self.page_fetch_gen.wrapping_add(1);
        self.feature_fetch_gen = self.feature_fetch_gen.wrapping_add(1);
        let run_gen = self.generation_id;
        let page_gen = self.page_fetch_gen;
        let feature_gen = self.feature_fetch_gen;

        self.page_search.update(cx, |input, cx| input.set_disabled(true, cx));
        self.feature_search.update(cx, |input, cx| input.set_disabled(true, cx));

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::StoryWriter,
                Some("Fetching Confluence page & parent Feature...".to_string()),
            );
            cx.notify();
        });
        cx.notify();

        let view = cx.entity();
        let app_state = self.app_state.clone();

        window
            .spawn(cx, async move |cx| {
                let settings_for_fetch = settings.clone();
                let res = spawn_tokio(async move {
                    let doc_provider = build_documentation_provider(&settings_for_fetch)?;
                    let issue_provider = build_issue_tracker_provider(&settings_for_fetch)?;
                    let (page_res, feature_res) = tokio::join!(
                        doc_provider.fetch_page(&page_id),
                        issue_provider.fetch_ticket(&feature_id)
                    );
                    Ok::<_, anyhow::Error>((page_res?, feature_res?))
                })
                .await;

                let fetched_pair = match res {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(err)) => {
                        let msg = err.to_string();
                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                if this.generation_id != run_gen {
                                    return;
                                }
                                this.finish_generation_ui(page_gen, feature_gen, cx);
                                this.validation_error = Some(msg);
                                cx.notify();
                            });
                        });
                        return;
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                if this.generation_id != run_gen {
                                    return;
                                }
                                this.finish_generation_ui(page_gen, feature_gen, cx);
                                this.validation_error = Some(msg);
                                cx.notify();
                            });
                        });
                        return;
                    }
                };

                let (page, feature) = fetched_pair;
                let mut rx_opt = None;
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.generation_id != run_gen {
                            return;
                        }
                        if this.page_fetch_gen == page_gen {
                            this.fetching_page = false;
                        }
                        if this.feature_fetch_gen == feature_gen {
                            this.fetching_feature = false;
                        }
                        this.page_search_cache.insert(page.id.clone(), page.clone());
                        if let Some(id) = feature.id.clone() {
                            this.feature_search_cache.insert(id, feature.clone());
                        }
                        this.fetched_page = Some(page.clone());
                        this.fetched_feature = Some(feature);

                        app_state.update(cx, |state, cx| {
                            state.set_task_status(
                                ActiveView::StoryWriter,
                                Some("Generating stories with Copilot...".to_string()),
                            );
                            cx.notify();
                        });

                        let (handle, rx) = service::generate_stories(
                            copilot,
                            StoryGenerationRequest {
                                page_data: page,
                                additional_context,
                                model_override,
                                settings,
                            },
                        );
                        this.active_handle = Some(handle);
                        rx_opt = Some(rx);
                        cx.notify();
                    });
                });

                let Some(mut rx) = rx_opt else {
                    return;
                };

                while let Some(event) = rx.recv().await {
                    let _ = cx.update(|window, cx| {
                        let mut open_timeout_secs: Option<u64> = None;

                        view.update(cx, |this, cx| {
                            if this.generation_id != run_gen {
                                return;
                            }
                            match event {
                                StoryGenerationStreamEvent::Story(story) => {
                                    this.upsert_story(story, cx);
                                    let count = this.stories.len();
                                    app_state.update(cx, |state, cx| {
                                        state.set_task_status(
                                            ActiveView::StoryWriter,
                                            Some(format!(
                                                "Generating stories ({count} drafted)..."
                                            )),
                                        );
                                        cx.notify();
                                    });
                                }
                                StoryGenerationStreamEvent::Completed { story_count, usage } => {
                                    this.finish_generation_ui(page_gen, feature_gen, cx);
                                    let count = this.stories.len().max(story_count);
                                    let ai_output = if count == 1 {
                                        "1 story".to_string()
                                    } else {
                                        format!("{count} stories")
                                    };
                                    let ctx_ref = this
                                        .selected_page_id
                                        .as_deref()
                                        .map(|id| format!("Confluence - {id}"));
                                    let model_id = this.selected_model.clone();
                                    let db_session_id = app_state.update(cx, |state, cx| {
                                        let sid = state.record_completed_tool_session(
                                            "Story Writer",
                                            ctx_ref.as_deref(),
                                            Some(&ai_output),
                                            "Story Generation",
                                            &model_id,
                                            &usage,
                                            cx,
                                        );
                                        cx.notify();
                                        sid
                                    });
                                    this.last_db_session_id = db_session_id;
                                    this.generation_usage = Some(usage);
                                    if story_count == 0 {
                                        this.validation_error = Some(
                                            "Copilot completed without returning any structured user stories. Please check the page content and try again."
                                                .to_string(),
                                        );
                                    } else {
                                        NotificationService::show_if_unfocused(
                                            window,
                                            "Story Writer Complete",
                                            format!("Generated {ai_output}."),
                                        );
                                    }
                                    cx.notify();
                                }
                                StoryGenerationStreamEvent::TimedOut { timeout_secs } => {
                                    this.finish_generation_ui(page_gen, feature_gen, cx);
                                    this.validation_error = Some(format!(
                                        "Story generation timed out after {timeout_secs} seconds without Copilot output."
                                    ));
                                    open_timeout_secs = Some(timeout_secs);
                                    cx.notify();
                                }
                                StoryGenerationStreamEvent::Cancelled => {
                                    this.finish_generation_ui(page_gen, feature_gen, cx);
                                    this.status_notice =
                                        Some("Story generation cancelled.".to_string());
                                    cx.notify();
                                }
                                StoryGenerationStreamEvent::Error(msg) => {
                                    this.finish_generation_ui(page_gen, feature_gen, cx);
                                    this.validation_error = Some(msg);
                                    cx.notify();
                                }
                            }
                        });

                        if let Some(secs) = open_timeout_secs {
                            let view_for_retry = view.clone();
                            open_timeout_modal(
                                "Story Writer",
                                secs,
                                move |window, cx| {
                                    view_for_retry.update(cx, |this, cx| {
                                        this.handle_generate_stories(window, cx);
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

    fn finish_generation_ui(
        &mut self,
        page_gen: u64,
        feature_gen: u64,
        cx: &mut Context<Self>,
    ) {
        self.active_handle = None;
        self.is_generating = false;
        if self.page_fetch_gen == page_gen {
            self.fetching_page = false;
        }
        if self.feature_fetch_gen == feature_gen {
            self.fetching_feature = false;
        }
        self.page_search.update(cx, |input, cx| input.set_disabled(false, cx));
        self.feature_search.update(cx, |input, cx| input.set_disabled(false, cx));
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::StoryWriter, None);
            cx.notify();
        });
    }

    fn cancel_generation(&mut self, cx: &mut Context<Self>) {
        if !self.is_generating {
            return;
        }
        if let Some(handle) = self.active_handle.as_ref() {
            handle.cancel();
            self.app_state.update(cx, |state, cx| {
                state.set_task_status(
                    ActiveView::StoryWriter,
                    Some("Cancelling generation...".to_string()),
                );
                cx.notify();
            });
            cx.notify();
        } else {
            // Cancelled while still fetching Confluence / Feature context.
            self.generation_id = self.generation_id.wrapping_add(1);
            let page_gen = self.page_fetch_gen;
            let feature_gen = self.feature_fetch_gen;
            self.finish_generation_ui(page_gen, feature_gen, cx);
            self.status_notice = Some("Story generation cancelled.".to_string());
            cx.notify();
        }
    }

    fn toggle_collapse(&mut self, card_id: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.stories.iter_mut().find(|s| s.id == card_id) {
            item.collapsed = !item.collapsed;
            cx.notify();
        }
    }

    fn start_edit(&mut self, card_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.stories.iter_mut().find(|s| s.id == card_id) else {
            return;
        };
        let story = item.story.clone();
        let title_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Story title")
                .default_value(story.title)
        });
        let description_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("As a... I want to... So that...")
                .default_value(story.description)
        });
        let ac_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("* Acceptance criterion 1\n* Acceptance criterion 2")
                .default_value(story.acceptance_criteria)
        });
        let notes_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Optional implementation notes or assumptions...")
                .default_value(story.notes.unwrap_or_default())
        });

        item.collapsed = false;
        item.editing = Some(StoryCardEditor {
            title_input,
            description_input,
            ac_input,
            notes_input,
        });
        cx.notify();
    }

    fn cancel_edit(&mut self, card_id: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.stories.iter_mut().find(|s| s.id == card_id) {
            item.editing = None;
            cx.notify();
        }
    }

    fn save_edit(&mut self, card_id: usize, cx: &mut Context<Self>) {
        let Some(item) = self.stories.iter_mut().find(|s| s.id == card_id) else {
            return;
        };
        let Some(editor) = item.editing.take() else {
            return;
        };

        let title_raw = editor.title_input.read(cx).value().trim().to_string();
        if !title_raw.is_empty() {
            item.story.title = title_raw;
        }
        item.story.description = editor.description_input.read(cx).value().trim().to_string();
        item.story.acceptance_criteria = editor.ac_input.read(cx).value().trim().to_string();
        let notes_trimmed = editor.notes_input.read(cx).value().trim().to_string();
        item.story.notes = if notes_trimmed.is_empty() {
            None
        } else {
            Some(notes_trimmed)
        };
        item.edited = true;
        cx.notify();
    }

    fn discard_story(&mut self, card_id: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.stories.iter_mut().find(|s| s.id == card_id) {
            item.status = StoryCardStatus::Discarded;
            item.collapsed = true;
            item.editing = None;
            cx.notify();
        }
    }

    fn create_story(&mut self, card_id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent_feature_id) = self
            .selected_feature_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            self.card_errors.insert(
                card_id,
                "Please select or enter a parent Feature ID before creating this story."
                    .to_string(),
            );
            cx.notify();
            return;
        };

        let Some(item) = self.stories.iter_mut().find(|s| s.id == card_id) else {
            return;
        };
        if matches!(
            item.status,
            StoryCardStatus::Creating | StoryCardStatus::Created
        ) {
            return;
        }

        let ticket_data = TicketData {
            id: None,
            title: item.story.title.clone(),
            description: item.story.description.clone(),
            acceptance_criteria: Some(item.story.acceptance_criteria.clone())
                .filter(|s| !s.trim().is_empty()),
        };
        let edited = item.edited;
        item.status = StoryCardStatus::Creating;
        self.card_errors.remove(&card_id);
        cx.notify();

        let settings = self.app_state.read(cx).settings.clone();
        let story_type = if settings.story_type.trim().is_empty() {
            "Product Backlog Item".to_string()
        } else {
            settings.story_type.trim().to_string()
        };

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_issue_tracker_provider(&settings)?;
                    provider
                        .create_ticket(&story_type, &parent_feature_id, &ticket_data, edited)
                        .await
                })
                .await;

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        let Some(item) = this.stories.iter_mut().find(|s| s.id == card_id) else {
                            return;
                        };
                        match res {
                            Ok(Ok(created_id)) => {
                                item.status = StoryCardStatus::Created;
                                item.collapsed = true;
                                item.editing = None;
                                this.card_errors.remove(&card_id);
                                if let Some(id) = created_id {
                                    this.created_ticket_ids.insert(card_id, id);
                                }
                                if let Some(sid) = this.last_db_session_id {
                                    this.app_state.update(cx, |state, cx| {
                                        state.increment_story_push_for_session(sid, cx);
                                        cx.notify();
                                    });
                                }
                            }
                            Ok(Err(err)) => {
                                item.status = StoryCardStatus::Draft;
                                this.card_errors.insert(card_id, err.to_string());
                            }
                            Err(err) => {
                                item.status = StoryCardStatus::Draft;
                                this.card_errors.insert(card_id, err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn render_left_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let settings = &self.app_state.read(cx).settings;
        let docs_configured = settings.sources.docs == "atlassian"
            && settings.connectors.is_atlassian_configured();
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
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);

        let app_state_for_warn = self.app_state.clone();
        let direct_page_id = self.selected_page_id.clone();
        let direct_feature_id = self.selected_feature_id.clone();

        v_flex()
            .size_full()
            .overflow_y_scrollbar()
            .bg(theme.sidebar)
            .p_5()
            .gap_4()
            .when(!docs_configured || !issues_configured, |col| {
                let mut missing = Vec::new();
                if !docs_configured {
                    missing.push("Atlassian Confluence");
                }
                if !issues_configured {
                    missing.push("Issues Tracker (Azure DevOps / GitHub)");
                }
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
                                .child(format!("Missing configuration for: {}.", missing.join(" & "))),
                        )
                        .child(
                            Button::new("story-open-connectors")
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
                                    .child("Page Search (Confluence)"),
                            )
                            .when_some(direct_page_id, |row, page_id| {
                                row.child(
                                    Button::new("fetch-page-preview")
                                        .ghost()
                                        .xsmall()
                                        .disabled(self.fetching_page || self.is_generating)
                                        .loading(self.fetching_page)
                                        .label("Fetch")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.fetch_page_by_id(&page_id, window, cx);
                                        })),
                                )
                            }),
                    )
                    .child(self.page_search.clone()),
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
                                    .text_color(theme.foreground)
                                    .child(format!("Feature ID / Search ({issue_source_label})")),
                            )
                            .when_some(direct_feature_id, |row, feature_id| {
                                row.child(
                                    Button::new("fetch-feature-preview")
                                        .ghost()
                                        .xsmall()
                                        .disabled(self.fetching_feature || self.is_generating)
                                        .loading(self.fetching_feature)
                                        .label("Fetch")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.fetch_feature_by_id(&feature_id, window, cx);
                                        })),
                                )
                            }),
                    )
                    .child(self.feature_search.clone()),
            )
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
                    .child(
                        Textarea::new(&self.additional_context_input)
                            .h(px(110.))
                            .disabled(self.is_generating),
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
                div().pt_1().child(if self.is_generating {
                    Button::new("cancel-stories")
                        .danger()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Square).small())
                        .label("Cancel Generation")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_generation(cx);
                        }))
                } else {
                    Button::new("generate-stories")
                        .primary()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Sparkles).small())
                        .label("Generate Stories")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_generate_stories(window, cx);
                        }))
                }),
            )
            .when_some(self.fetched_feature.clone(), |col, feature| {
                let id_label = feature.id.unwrap_or_default();
                let preview = strip_html_for_preview(&feature.description);
                col.child(
                    v_flex()
                        .p_3p5()
                        .gap_1p5()
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
                                        .child(Icon::new(LucideIcon::Layers).xsmall())
                                        .child("PARENT FEATURE"),
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
                                .child(feature.title),
                        )
                        .when(!preview.is_empty(), |card| {
                            card.child(
                                div()
                                    .max_h(px(72.))
                                    .overflow_y_scrollbar()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(preview),
                            )
                        }),
                )
            })
            .when_some(self.fetched_page.clone(), |col, page| {
                let preview = strip_html_for_preview(&page.body);
                col.child(
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
                                        .child(Icon::new(LucideIcon::FileText).xsmall())
                                        .child("FETCHED PAGE"),
                                )
                                .child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(theme.radius)
                                        .bg(theme.secondary)
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("#{}", page.id)),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(page.title),
                        )
                        .child(
                            div()
                                .max_h(px(180.))
                                .overflow_y_scrollbar()
                                .p_2p5()
                                .rounded(theme.radius)
                                .bg(theme.sidebar)
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(if preview.is_empty() {
                                    "(Page body is empty)".to_string()
                                } else {
                                    preview
                                }),
                        ),
                )
            })
    }

    fn render_right_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let total = self.stories.len();
        let created_count = self
            .stories
            .iter()
            .filter(|s| s.status == StoryCardStatus::Created)
            .count();
        let discarded_count = self
            .stories
            .iter()
            .filter(|s| s.status == StoryCardStatus::Discarded)
            .count();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);

        let usage_summary = self.generation_usage.as_ref().and_then(|u| {
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

        let mut cards: Vec<AnyElement> = Vec::with_capacity(self.stories.len());
        for item in &self.stories {
            cards.push(self.render_story_card(item, &theme, cx).into_any_element());
        }

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
                            .child(Icon::new(LucideIcon::ClipboardList).small())
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Generated Stories"),
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
                                    .child(total.to_string()),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .when_some(usage_summary, |row, summary| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(summary),
                                )
                            })
                            .when(created_count > 0, |row| {
                                row.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(green.opacity(0.4))
                                        .bg(green.opacity(0.1))
                                        .text_xs()
                                        .text_color(green)
                                        .child(format!("{created_count} created")),
                                )
                            })
                            .when(discarded_count > 0, |row| {
                                row.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(theme.radius)
                                        .bg(theme.secondary)
                                        .border_1()
                                        .border_color(theme.border)
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{discarded_count} discarded")),
                                )
                            }),
                    ),
            )
            .child(
                div().flex_1().min_h_0().w_full().overflow_hidden().child(
                    if self.stories.is_empty() && self.is_generating {
                        v_flex()
                            .size_full()
                            .p_8()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(Spinner::new().large().color(theme.primary))
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if self.fetching_page || self.fetching_feature {
                                        "Fetching Confluence Page & Feature..."
                                    } else {
                                        "Asking Copilot to write user stories..."
                                    }),
                            )
                            .child(
                                div()
                                    .max_w(px(440.))
                                    .text_center()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Stories will stream in as structured cards as Copilot produces them."),
                            )
                            .into_any_element()
                    } else if self.stories.is_empty() {
                        v_flex()
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
                                    .child(Icon::new(LucideIcon::FileText).large()),
                            )
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("No Stories Generated Yet"),
                            )
                            .child(
                                div()
                                    .max_w(px(460.))
                                    .text_center()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Search for a Confluence page and parent Feature on the left, then click Generate Stories to draft user stories."),
                            )
                            .into_any_element()
                    } else {
                        v_flex()
                            .size_full()
                            .overflow_y_scrollbar()
                            .p_6()
                            .gap_4()
                            .children(cards)
                            .when(self.is_generating, |col| {
                                col.child(
                                    h_flex()
                                        .p_4()
                                        .items_center()
                                        .gap_2p5()
                                        .rounded(theme.radius_lg)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.sidebar)
                                        .child(Spinner::new().small().color(theme.primary))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child("Generating next story..."),
                                        ),
                                )
                            })
                            .into_any_element()
                    },
                ),
            )
    }

    fn render_story_card(
        &self,
        item: &StoryCardItem,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let card_id = item.id;
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let is_created = item.status == StoryCardStatus::Created;
        let is_creating = item.status == StoryCardStatus::Creating;
        let is_discarded = item.status == StoryCardStatus::Discarded;
        let created_label = match self.created_ticket_ids.get(&card_id) {
            Some(id) => format!("Created (#{id})"),
            None => "Created".to_string(),
        };
        let card_error = self.card_errors.get(&card_id).cloned();

        if item.collapsed {
            return h_flex()
                .p_3()
                .items_center()
                .justify_between()
                .gap_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .when(is_discarded, |row| row.opacity(0.72))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(LucideIcon::Book)
                                .small()
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .child(item.story.title.clone()),
                        )
                        .when(is_created, |row| {
                            row.child(render_status_pill(created_label.clone(), green, theme))
                        })
                        .when(is_discarded, |row| {
                            row.child(render_status_pill(
                                "Discarded",
                                theme.muted_foreground,
                                theme,
                            ))
                        })
                        .when(item.edited, |row| {
                            row.child(render_status_pill("Edited", amber, theme))
                        }),
                )
                .child(
                    Button::new(("story-expand", card_id))
                        .ghost()
                        .small()
                        .icon(Icon::new(LucideIcon::ChevronDown).small())
                        .label("Expand")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_collapse(card_id, cx);
                        })),
                )
                .into_any_element();
        }

        let card_border = if is_created {
            green.opacity(0.5)
        } else {
            theme.border
        };

        v_flex()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(card_border)
            .bg(theme.background)
            .overflow_hidden()
            .when(is_discarded, |card| card.opacity(0.75))
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .bg(theme.sidebar)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(LucideIcon::Book)
                                    .small()
                                    .text_color(if is_created {
                                        green
                                    } else {
                                        theme.primary
                                    }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(item.story.title.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .when(is_created, |row| {
                                row.child(render_status_pill(created_label.clone(), green, theme))
                            })
                            .when(is_discarded, |row| {
                                row.child(render_status_pill(
                                    "Discarded",
                                    theme.muted_foreground,
                                    theme,
                                ))
                            })
                            .when(item.edited, |row| {
                                row.child(render_status_pill("Edited", amber, theme))
                            })
                            .child(
                                Button::new(("story-collapse", card_id))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::ChevronUp).small())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_collapse(card_id, cx);
                                    })),
                            ),
                    ),
            )
            .child(if let Some(editor) = &item.editing {
                v_flex()
                    .p_4()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("TITLE"),
                            )
                            .child(Input::new(&editor.title_input)),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("DESCRIPTION"),
                            )
                            .child(Textarea::new(&editor.description_input).h(px(120.))),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("ACCEPTANCE CRITERIA (MARKDOWN)"),
                            )
                            .child(Textarea::new(&editor.ac_input).h(px(120.))),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("NOTES (OPTIONAL)"),
                            )
                            .child(Textarea::new(&editor.notes_input).h(px(76.))),
                    )
                    .child(
                        h_flex()
                            .pt_2()
                            .justify_end()
                            .gap_2()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new(("story-edit-cancel", card_id))
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel_edit(card_id, cx);
                                    })),
                            )
                            .child(
                                Button::new(("story-edit-save", card_id))
                                    .primary()
                                    .small()
                                    .icon(Icon::new(LucideIcon::Save).small())
                                    .label("Save Changes")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.save_edit(card_id, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            } else {
                let notes = item
                    .story
                    .notes
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);

                v_flex()
                    .p_4()
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("DESCRIPTION"),
                            )
                            .child(v_flex().gap_2().text_sm().children(
                                item.story.description.split("\n\n").map(|para| {
                                    div().text_color(theme.foreground).child(para.to_string())
                                }),
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("ACCEPTANCE CRITERIA"),
                            )
                            .child(
                                div()
                                    .p_3()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.sidebar)
                                    .child(
                                        TextView::markdown(
                                            ("story-ac", card_id),
                                            item.story.acceptance_criteria.clone(),
                                        )
                                        .selectable(true),
                                    ),
                            ),
                    )
                    .when_some(notes, |col, note_text| {
                        col.child(
                            v_flex()
                                .gap_1p5()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.muted_foreground)
                                        .child("NOTES"),
                                )
                                .child(
                                    div()
                                        .p_2p5()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.sidebar)
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(note_text),
                                ),
                        )
                    })
                    .when_some(card_error, |col, err| {
                        col.child(
                            h_flex()
                                .p_2p5()
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
                        h_flex()
                            .pt_3()
                            .justify_between()
                            .items_center()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Button::new(("story-edit", card_id))
                                    .ghost()
                                    .small()
                                    .disabled(is_created || is_creating)
                                    .icon(Icon::new(LucideIcon::Pencil).small())
                                    .label("Edit")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.start_edit(card_id, window, cx);
                                    })),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new(("story-discard", card_id))
                                            .ghost()
                                            .small()
                                            .disabled(is_created || is_creating || is_discarded)
                                            .icon(Icon::new(LucideIcon::Trash).small())
                                            .label("Discard")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.discard_story(card_id, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new(("story-create", card_id))
                                            .primary()
                                            .small()
                                            .disabled(is_created || is_creating)
                                            .loading(is_creating)
                                            .icon(
                                                Icon::new(if is_created {
                                                    LucideIcon::Check
                                                } else {
                                                    LucideIcon::Plus
                                                })
                                                .small(),
                                            )
                                            .label(if is_creating {
                                                "Creating..."
                                            } else if is_created {
                                                "Created"
                                            } else {
                                                "Create Story"
                                            })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.create_story(card_id, window, cx);
                                            })),
                                    ),
                            ),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }
}

impl Render for StoryWriterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.background;
        let fg = theme.foreground;
        let border = theme.border;
        let secondary = theme.secondary;
        let accent_fg = theme.accent_foreground;
        let muted_fg = theme.muted_foreground;
        let radius = theme.radius;

        let settings = &self.app_state.read(cx).settings;
        let connectors_ready = settings.sources.docs == "atlassian"
            && settings.connectors.is_atlassian_configured()
            && match settings.sources.issues.as_str() {
                "azureDevOps" => settings.connectors.is_azure_configured(),
                "github" => settings.connectors.is_github_configured(),
                _ => false,
            };
        let app_state_for_header = self.app_state.clone();
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
                                    .child(
                                        Icon::new(ActiveView::StoryWriter.icon())
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
                                            .child(ActiveView::StoryWriter.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::StoryWriter.subtitle()),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .when(!connectors_ready, |header| {
                                header.child(
                                    Button::new("story-header-connectors")
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
                                "story-header-reset",
                                reset_state,
                                move |window, cx| {
                                    view_for_reset.update(cx, |this, cx| {
                                        this.request_reset(window, cx);
                                    });
                                },
                            )),
                    ),
            )
            .child(
                div().flex_1().min_h_0().w_full().child(
                    h_resizable("story-writer-split")
                        .child(
                            resizable_panel()
                                .size(px(380.))
                                .size_range(px(300.)..px(540.))
                                .child(self.render_left_pane(cx)),
                        )
                        .child(resizable_panel().child(self.render_right_pane(cx))),
                ),
            )
    }
}

fn feature_id_mode_for(settings: &AppSettings) -> DirectIdMode {
    if settings.sources.issues.trim() == "github" {
        DirectIdMode::NumericOrRepoIssue
    } else {
        DirectIdMode::NumericOnly
    }
}

fn render_status_pill(
    label: impl Into<gpui_kit::SharedString>,
    color: gpui_kit::Hsla,
    theme: &Theme,
) -> impl IntoElement {
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
        .child(label.into())
}

pub(crate) fn upsert_story_in_list(
    stories: &mut Vec<StoryCardItem>,
    next_id: &mut usize,
    story: StoryData,
) {
    if let Some(existing) = stories.iter_mut().find(|item| item.story.title == story.title) {
        existing.story = story;
        return;
    }
    let id = *next_id;
    *next_id += 1;
    stories.push(StoryCardItem {
        id,
        story,
        collapsed: false,
        status: StoryCardStatus::Draft,
        edited: false,
        editing: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_story_deduplicates_by_title_and_preserves_id() {
        let mut stories = Vec::new();
        let mut next_id = 1;

        upsert_story_in_list(
            &mut stories,
            &mut next_id,
            StoryData {
                title: "User Login".to_string(),
                description: "Initial draft".to_string(),
                acceptance_criteria: "* AC 1".to_string(),
                notes: None,
            },
        );
        upsert_story_in_list(
            &mut stories,
            &mut next_id,
            StoryData {
                title: "Password Reset".to_string(),
                description: "Reset flow".to_string(),
                acceptance_criteria: "* AC 2".to_string(),
                notes: Some("Email link".to_string()),
            },
        );
        upsert_story_in_list(
            &mut stories,
            &mut next_id,
            StoryData {
                title: "User Login".to_string(),
                description: "Updated draft".to_string(),
                acceptance_criteria: "* AC 1\n* AC 3".to_string(),
                notes: None,
            },
        );

        assert_eq!(stories.len(), 2);
        assert_eq!(next_id, 3);
        assert_eq!(stories[0].id, 1);
        assert_eq!(stories[0].story.description, "Updated draft");
        assert_eq!(stories[0].story.acceptance_criteria, "* AC 1\n* AC 3");
        assert_eq!(stories[1].id, 2);
    }
}
