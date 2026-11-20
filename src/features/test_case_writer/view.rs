use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, Theme, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, hsla,
    prelude::FluentBuilder as _, px, relative,
};

use crate::app_state::{ActiveView, AppState};
use crate::features::settings::SettingsTab;
use crate::features::test_case_writer::service::{
    self, ActiveTestCaseGenerationHandle, TestCaseGenerationRequest, TestCaseGenerationStreamEvent,
};
use crate::infrastructure::providers::build_issue_tracker_provider;
use crate::infrastructure::providers::confluence::strip_html_for_preview;
use crate::infrastructure::runtime::spawn_tokio;
use crate::types::{AppSettings, CopilotUsage, TestCase, TicketData};
use crate::ui::components::{
    AutocompleteEvent, AutocompleteInput, AutocompleteOption, DirectIdMode, ModelDropdown,
    ModelDropdownEvent, ModelDropdownMode, open_timeout_modal,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishAction {
    Comment,
    Task,
}

#[derive(Debug, Clone)]
struct TestCaseRow {
    uid: usize,
    data: TestCase,
}

#[derive(Clone)]
struct DraggedTestCaseRow {
    from_index: usize,
    display_id: String,
    summary: String,
}

struct DragRowPreview {
    display_id: String,
    summary: String,
}

impl Render for DragRowPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .max_w(px(360.))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.primary)
            .bg(theme.background)
            .shadow_md()
            .text_xs()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.primary)
                    .child(self.display_id.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.foreground)
                    .child(self.summary.clone()),
            )
    }
}

pub struct TestCaseWriterView {
    app_state: Entity<AppState>,
    ticket_search: Entity<AutocompleteInput>,
    ticket_search_cache: HashMap<String, TicketData>,
    selected_ticket_id: Option<String>,
    fetched_ticket: Option<TicketData>,
    fetching_ticket: bool,
    ticket_fetch_generation: u64,
    additional_context_input: Entity<TextareaState>,
    model_dropdown: Entity<ModelDropdown>,
    selected_model: String,
    user_overrode_model: bool,
    rows: Vec<TestCaseRow>,
    next_row_uid: usize,
    selected_row_uid: Option<usize>,
    table_focus_handle: FocusHandle,
    test_cases_modified: bool,
    is_generating: bool,
    generation_id: u64,
    active_handle: Option<ActiveTestCaseGenerationHandle>,
    generation_usage: Option<CopilotUsage>,
    posting_action: Option<PublishAction>,
    publish_feedback: Option<Result<String, String>>,
    copied: bool,
    copy_generation: u64,
    validation_error: Option<String>,
    status_notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl TestCaseWriterView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_model = if initial_settings.copilot_model.trim().is_empty() {
            "auto".to_string()
        } else {
            initial_settings.copilot_model.trim().to_string()
        };

        let initial_mode = ticket_id_mode_for(initial_settings);
        let ticket_search = cx.new(|cx| {
            AutocompleteInput::new(
                "tc-ticket-search",
                "Search tickets by title, text or ID...",
                initial_mode,
                "No tickets found.",
                "Searching work items...",
                window,
                cx,
            )
        });

        let additional_context_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("e.g., focus on edge cases or accessibility requirements...")
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
                (settings.copilot_model.clone(), ticket_id_mode_for(settings))
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

        Self {
            app_state,
            ticket_search,
            ticket_search_cache: HashMap::new(),
            selected_ticket_id: None,
            fetched_ticket: None,
            fetching_ticket: false,
            ticket_fetch_generation: 0,
            additional_context_input,
            model_dropdown,
            selected_model: initial_model,
            user_overrode_model: false,
            rows: Vec::new(),
            next_row_uid: 1,
            selected_row_uid: None,
            table_focus_handle: cx.focus_handle(),
            test_cases_modified: false,
            is_generating: false,
            generation_id: 0,
            active_handle: None,
            generation_usage: None,
            posting_action: None,
            publish_feedback: None,
            copied: false,
            copy_generation: 0,
            validation_error: None,
            status_notice: None,
            _subscriptions: vec![state_sub, model_sub, ticket_sub],
        }
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

    fn handle_generate_test_cases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_generating {
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

        let additional_context = self.additional_context_input.read(cx).value().to_string();
        let model_override = self.selected_model.clone();
        let (copilot, settings) = {
            let state = self.app_state.read(cx);
            (state.copilot_service.clone(), state.settings.clone())
        };

        self.validation_error = None;
        self.status_notice = None;
        self.publish_feedback = None;
        self.rows.clear();
        self.selected_row_uid = None;
        self.test_cases_modified = false;
        self.generation_usage = None;
        self.copied = false;
        self.is_generating = true;
        self.fetching_ticket = true;
        self.generation_id = self.generation_id.wrapping_add(1);
        self.ticket_fetch_generation = self.ticket_fetch_generation.wrapping_add(1);
        let run_gen = self.generation_id;
        let fetch_gen = self.ticket_fetch_generation;

        self.ticket_search
            .update(cx, |input, cx| input.set_disabled(true, cx));

        self.app_state.update(cx, |state, cx| {
            state.set_task_status(
                ActiveView::TestCaseWriter,
                Some("Fetching ticket details...".to_string()),
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
                    let provider = build_issue_tracker_provider(&settings_for_fetch)?;
                    provider.fetch_ticket(&ticket_id).await
                })
                .await;

                let fetched_ticket = match res {
                    Ok(Ok(ticket)) => ticket,
                    Ok(Err(err)) => {
                        let msg = err.to_string();
                        let _ = cx.update(|_, cx| {
                            view.update(cx, |this, cx| {
                                if this.generation_id != run_gen {
                                    return;
                                }
                                this.finish_generation_ui(fetch_gen, cx);
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
                                this.finish_generation_ui(fetch_gen, cx);
                                this.validation_error = Some(msg);
                                cx.notify();
                            });
                        });
                        return;
                    }
                };

                let mut rx_opt = None;
                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        if this.generation_id != run_gen {
                            return;
                        }
                        if this.ticket_fetch_generation == fetch_gen {
                            this.fetching_ticket = false;
                        }
                        if let Some(id) = fetched_ticket.id.clone() {
                            this.ticket_search_cache
                                .insert(id, fetched_ticket.clone());
                        }
                        this.fetched_ticket = Some(fetched_ticket.clone());

                        app_state.update(cx, |state, cx| {
                            state.set_task_status(
                                ActiveView::TestCaseWriter,
                                Some("Generating test cases with Copilot...".to_string()),
                            );
                            cx.notify();
                        });

                        let (handle, rx) = service::generate_test_cases(
                            copilot,
                            TestCaseGenerationRequest {
                                ticket_data: fetched_ticket,
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
                                TestCaseGenerationStreamEvent::TestCase(tc) => {
                                    this.push_test_case(tc, cx);
                                    let count = this.active_test_case_count();
                                    app_state.update(cx, |state, cx| {
                                        state.set_task_status(
                                            ActiveView::TestCaseWriter,
                                            Some(format!(
                                                "Generating test cases ({count} drafted)..."
                                            )),
                                        );
                                        cx.notify();
                                    });
                                }
                                TestCaseGenerationStreamEvent::Completed { count, usage } => {
                                    this.finish_generation_ui(fetch_gen, cx);
                                    this.generation_usage = Some(usage);
                                    if count == 0 {
                                        this.validation_error = Some(
                                            "Copilot completed without returning any structured test cases. Please review the ticket content and try again."
                                                .to_string(),
                                        );
                                    }
                                    cx.notify();
                                }
                                TestCaseGenerationStreamEvent::TimedOut { timeout_secs } => {
                                    this.finish_generation_ui(fetch_gen, cx);
                                    this.validation_error = Some(format!(
                                        "Test case generation timed out after {timeout_secs} seconds without Copilot output."
                                    ));
                                    open_timeout_secs = Some(timeout_secs);
                                    cx.notify();
                                }
                                TestCaseGenerationStreamEvent::Cancelled => {
                                    this.finish_generation_ui(fetch_gen, cx);
                                    this.status_notice =
                                        Some("Test case generation cancelled.".to_string());
                                    cx.notify();
                                }
                                TestCaseGenerationStreamEvent::Error(msg) => {
                                    this.finish_generation_ui(fetch_gen, cx);
                                    this.validation_error = Some(msg);
                                    cx.notify();
                                }
                            }
                        });

                        if let Some(secs) = open_timeout_secs {
                            let view_for_retry = view.clone();
                            open_timeout_modal(
                                "Test Case Writer",
                                secs,
                                move |window, cx| {
                                    view_for_retry.update(cx, |this, cx| {
                                        this.handle_generate_test_cases(window, cx);
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

    fn finish_generation_ui(&mut self, fetch_gen: u64, cx: &mut Context<Self>) {
        self.active_handle = None;
        self.is_generating = false;
        if self.ticket_fetch_generation == fetch_gen {
            self.fetching_ticket = false;
        }
        self.ticket_search
            .update(cx, |input, cx| input.set_disabled(false, cx));
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::TestCaseWriter, None);
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
                    ActiveView::TestCaseWriter,
                    Some("Cancelling generation...".to_string()),
                );
                cx.notify();
            });
            cx.notify();
        } else {
            self.generation_id = self.generation_id.wrapping_add(1);
            let fetch_gen = self.ticket_fetch_generation;
            self.finish_generation_ui(fetch_gen, cx);
            self.status_notice = Some("Test case generation cancelled.".to_string());
            cx.notify();
        }
    }

    fn push_test_case(&mut self, tc: TestCase, cx: &mut Context<Self>) {
        let uid = self.next_row_uid;
        self.next_row_uid += 1;
        self.rows.push(TestCaseRow { uid, data: tc });
        cx.notify();
    }

    fn delete_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.data.deleted = true;
        if self.selected_row_uid == Some(row.uid) {
            self.selected_row_uid = None;
        }
        self.test_cases_modified = true;
        self.publish_feedback = None;
        cx.notify();
    }

    fn restore_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.data.deleted = false;
        self.selected_row_uid = Some(row.uid);
        self.test_cases_modified = true;
        self.publish_feedback = None;
        cx.notify();
    }

    fn move_row(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if self.is_generating {
            return;
        }
        if service::reorder_items(&mut self.rows, from, to) {
            self.selected_row_uid = Some(self.rows[to].uid);
            self.test_cases_modified = true;
            self.publish_feedback = None;
            cx.notify();
        }
    }

    fn handle_table_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key != "up" && key != "down" {
            return;
        }

        let mods = &event.keystroke.modifiers;
        if mods.control || mods.platform {
            return;
        }

        if mods.alt {
            if self.is_generating {
                return;
            }
            let Some(current_idx) = self
                .selected_row_uid
                .and_then(|uid| self.rows.iter().position(|r| r.uid == uid))
            else {
                return;
            };
            let target_idx = if key == "up" {
                current_idx.checked_sub(1)
            } else if current_idx + 1 < self.rows.len() {
                Some(current_idx + 1)
            } else {
                None
            };
            if let Some(to) = target_idx {
                self.move_row(current_idx, to, cx);
                cx.stop_propagation();
            }
            return;
        }

        if mods.shift {
            return;
        }

        let next_idx = match self
            .selected_row_uid
            .and_then(|uid| self.rows.iter().position(|r| r.uid == uid))
        {
            Some(current_idx) if key == "up" => {
                (0..current_idx).rev().find(|&i| !self.rows[i].data.deleted)
            }
            Some(current_idx) => (current_idx + 1..self.rows.len())
                .find(|&i| !self.rows[i].data.deleted),
            None => self.rows.iter().position(|r| !r.data.deleted),
        };

        if let Some(idx) = next_idx {
            self.selected_row_uid = Some(self.rows[idx].uid);
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn active_test_case_count(&self) -> usize {
        self.rows.iter().filter(|r| !r.data.deleted).count()
    }

    fn markdown_table(&self) -> String {
        let items: Vec<TestCase> = self.rows.iter().map(|r| r.data.clone()).collect();
        service::convert_to_markdown_table(&items)
    }

    fn copy_table(&mut self, cx: &mut Context<Self>) {
        let md_table = self.markdown_table();
        if md_table.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(md_table));
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

    fn publish_test_cases(
        &mut self,
        action: PublishAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_generating || self.posting_action.is_some() {
            return;
        }
        let md_table = self.markdown_table();
        if md_table.is_empty() {
            return;
        }

        let ticket_id = self
            .fetched_ticket
            .as_ref()
            .and_then(|t| t.id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.selected_ticket_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });

        let Some(ticket_id) = ticket_id else {
            self.publish_feedback = Some(Err("No ticket ID selected for publishing.".to_string()));
            cx.notify();
            return;
        };

        let settings = self.app_state.read(cx).settings.clone();
        let payload = service::format_test_cases_payload(&md_table);
        let edited = self.test_cases_modified;

        self.posting_action = Some(action);
        self.publish_feedback = None;
        let status_label = match action {
            PublishAction::Comment => "Adding test cases comment to ticket...",
            PublishAction::Task => "Creating testing task on ticket...",
        };
        self.app_state.update(cx, |state, cx| {
            state.set_task_status(ActiveView::TestCaseWriter, Some(status_label.to_string()));
            cx.notify();
        });
        cx.notify();

        let view = cx.entity();
        let app_state = self.app_state.clone();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let provider = build_issue_tracker_provider(&settings)?;
                    match action {
                        PublishAction::Comment => {
                            provider.add_comment(&ticket_id, &payload, edited).await?;
                            Ok::<_, anyhow::Error>(format!(
                                "Added test cases comment to ticket #{ticket_id}."
                            ))
                        }
                        PublishAction::Task => {
                            let task_type = {
                                let t = settings.task_type.trim();
                                if t.is_empty() { "Task" } else { t }
                            };
                            let task_title = {
                                let t = settings.test_task_title.trim();
                                if t.is_empty() { "Testing" } else { t }
                            };
                            let created_id = provider
                                .create_ticket(
                                    task_type,
                                    &ticket_id,
                                    &TicketData {
                                        id: None,
                                        title: task_title.to_string(),
                                        description: payload,
                                        acceptance_criteria: None,
                                    },
                                    edited,
                                )
                                .await?;
                            Ok(match created_id {
                                Some(id) => {
                                    format!("Created testing task #{id} under #{ticket_id}.")
                                }
                                None => format!("Created testing task under #{ticket_id}."),
                            })
                        }
                    }
                })
                .await;

                let outcome = match res {
                    Ok(Ok(msg)) => Ok(msg),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                };

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.posting_action = None;
                        this.publish_feedback = Some(outcome);
                        app_state.update(cx, |state, cx| {
                            state.set_task_status(ActiveView::TestCaseWriter, None);
                            cx.notify();
                        });
                        cx.notify();
                    });
                });
            })
            .detach();
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
                                .child("Configure Azure DevOps or GitHub and select Issues Tracker Source to search and update tickets."),
                        )
                        .child(
                            Button::new("tc-open-connectors")
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
                                    Button::new("tc-fetch-ticket-preview")
                                        .ghost()
                                        .xsmall()
                                        .disabled(self.fetching_ticket || self.is_generating)
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
                    Button::new("cancel-test-cases")
                        .danger()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Square).small())
                        .label("Cancel Generation")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cancel_generation(cx);
                        }))
                } else {
                    Button::new("generate-test-cases")
                        .primary()
                        .w_full()
                        .icon(Icon::new(LucideIcon::Sparkles).small())
                        .label("Generate Test Cases")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.handle_generate_test_cases(window, cx);
                        }))
                }),
            )
            .when_some(self.fetched_ticket.clone(), |col, ticket| {
                let id_label = ticket.id.unwrap_or_default();
                let preview = strip_html_for_preview(&ticket.description);
                let ac_preview = ticket
                    .acceptance_criteria
                    .as_deref()
                    .map(strip_html_for_preview)
                    .filter(|s| !s.is_empty());

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
                        }),
                )
            })
    }

    fn render_right_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let active_count = self.active_test_case_count();
        let deleted_count = self.rows.len().saturating_sub(active_count);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

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

        let is_posting = self.posting_action.is_some();

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
                                    .child("Generated Test Cases"),
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
                                    .child(active_count.to_string()),
                            )
                            .when(deleted_count > 0, |row| {
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
                                        .child(format!("{deleted_count} deleted")),
                                )
                            })
                            .when(self.test_cases_modified, |row| {
                                row.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(amber.opacity(0.4))
                                        .bg(amber.opacity(0.1))
                                        .text_xs()
                                        .text_color(amber)
                                        .child("Modified"),
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
                            .when(active_count > 0, |row| {
                                row.child(
                                    Button::new("tc-copy-table")
                                        .small()
                                        .disabled(self.is_generating)
                                        .icon(
                                            Icon::new(if self.copied {
                                                LucideIcon::Check
                                            } else {
                                                LucideIcon::Copy
                                            })
                                            .small(),
                                        )
                                        .label(if self.copied { "Copied!" } else { "Copy Table" })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.copy_table(cx);
                                        })),
                                )
                            }),
                    ),
            )
            .child(
                div().flex_1().min_h_0().w_full().overflow_hidden().child(
                    if self.rows.is_empty() && self.is_generating {
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
                                    .child(if self.fetching_ticket {
                                        "Fetching Ticket Details..."
                                    } else {
                                        "Streaming test cases from Copilot..."
                                    }),
                            )
                            .child(
                                div()
                                    .max_w(px(440.))
                                    .text_center()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Test scenarios are progressively parsed and rendered as structured table rows."),
                            )
                            .into_any_element()
                    } else if self.rows.is_empty() {
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
                                    .child(Icon::new(LucideIcon::ClipboardCheck).large()),
                            )
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("No Test Cases Generated Yet"),
                            )
                            .child(
                                div()
                                    .max_w(px(460.))
                                    .text_center()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Search for a ticket or enter a Ticket ID on the left, then click Generate Test Cases to stream scenarios here."),
                            )
                            .into_any_element()
                    } else {
                        self.render_test_case_table(&theme, cx).into_any_element()
                    },
                ),
            )
            .when(!self.rows.is_empty(), |pane| {
                pane.child(
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
                            div().flex_1().min_w_0().child(
                                if let Some(feedback) = &self.publish_feedback {
                                    let (color, icon, msg) = match feedback {
                                        Ok(m) => (green, LucideIcon::Check, m.clone()),
                                        Err(e) => (red, LucideIcon::CircleAlert, e.clone()),
                                    };
                                    h_flex()
                                        .items_center()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(color)
                                        .child(Icon::new(icon).small())
                                        .child(div().flex_1().min_w_0().truncate().child(msg))
                                        .into_any_element()
                                } else {
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Tip: drag the grip handle, click ↑/↓, or press Alt+↑/↓ on a selected row to reorder.")
                                        .into_any_element()
                                },
                            ),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .flex_shrink_0()
                                .child(
                                    Button::new("tc-add-comment")
                                        .small()
                                        .disabled(self.is_generating || is_posting || active_count == 0)
                                        .loading(self.posting_action == Some(PublishAction::Comment))
                                        .icon(Icon::new(LucideIcon::MessageSquare).small())
                                        .label("Add as Comment")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.publish_test_cases(PublishAction::Comment, window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("tc-add-task")
                                        .primary()
                                        .small()
                                        .disabled(self.is_generating || is_posting || active_count == 0)
                                        .loading(self.posting_action == Some(PublishAction::Task))
                                        .icon(Icon::new(LucideIcon::ListTodo).small())
                                        .label("Add as Testing Task")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.publish_test_cases(PublishAction::Task, window, cx);
                                        })),
                                ),
                        ),
                )
            })
    }

    fn render_test_case_table(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.rows.len();
        let mut active_num = 0usize;
        let mut row_elements: Vec<AnyElement> = Vec::with_capacity(total + 1);

        for (index, row) in self.rows.iter().enumerate() {
            if row.data.deleted {
                row_elements.push(
                    self.render_deleted_row(index, row.uid, theme, cx)
                        .into_any_element(),
                );
            } else {
                active_num += 1;
                let display_id = format!("TC-{active_num}");
                row_elements.push(
                    self.render_active_row(index, row, &display_id, total, theme, cx)
                        .into_any_element(),
                );
            }
        }

        div()
            .id("tc-table-scroll")
            .track_focus(&self.table_focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.handle_table_key_down(event, cx);
            }))
            .size_full()
            .overflow_scrollbar()
            .p_5()
            .child(
                v_flex()
                    .w_full()
                    .min_w(px(780.))
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .overflow_hidden()
                    .child(render_table_header(theme))
                    .children(row_elements)
                    .when(self.is_generating, |table| {
                        table.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_3()
                                .border_t_1()
                                .border_color(theme.border)
                                .bg(theme.sidebar.opacity(0.6))
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(Spinner::new().small().color(theme.primary))
                                .child("Generating next test case..."),
                        )
                    }),
            )
    }

    fn render_active_row(
        &self,
        index: usize,
        row: &TestCaseRow,
        display_id: &str,
        total: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let uid = row.uid;
        let is_selected = self.selected_row_uid == Some(uid);
        let is_generating = self.is_generating;
        let display_id_owned = display_id.to_string();
        let (prio_label, prio_color) = priority_badge_style(&row.data.priority, theme);
        let stripe_bg = if is_selected {
            theme.primary.opacity(0.08)
        } else if index % 2 == 1 {
            theme.sidebar.opacity(0.45)
        } else {
            theme.background
        };

        let drag_handle = div()
            .id(("tc-drag", uid))
            .p_1()
            .rounded(theme.radius)
            .text_color(if is_generating {
                theme.muted_foreground.opacity(0.4)
            } else {
                theme.muted_foreground
            })
            .when(!is_generating, |el| {
                el.cursor_grab()
                    .hover(|s| s.bg(theme.secondary).text_color(theme.foreground))
                    .on_drag(
                        DraggedTestCaseRow {
                            from_index: index,
                            display_id: display_id_owned.clone(),
                            summary: row.data.description.clone(),
                        },
                        |drag, _, _, cx| {
                            cx.new(|_| DragRowPreview {
                                display_id: drag.display_id.clone(),
                                summary: drag.summary.clone(),
                            })
                        },
                    )
            })
            .child(Icon::new(LucideIcon::GripVertical).xsmall());

        h_flex()
            .id(("tc-row", uid))
            .w_full()
            .items_start()
            .border_t_1()
            .border_color(theme.border)
            .bg(stripe_bg)
            .hover(|s| {
                if is_selected {
                    s
                } else {
                    s.bg(theme.secondary.opacity(0.35))
                }
            })
            .drag_over::<DraggedTestCaseRow>(move |style, dragged, _, cx| {
                if is_generating || dragged.from_index == index {
                    return style;
                }
                let primary = cx.theme().primary;
                let base = style.bg(primary.opacity(0.1)).border_color(primary);
                if dragged.from_index < index {
                    base.border_b_2()
                } else {
                    base.border_t_2()
                }
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedTestCaseRow, _, cx| {
                this.move_row(dragged.from_index, index, cx);
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_row_uid = Some(uid);
                window.focus(&this.table_focus_handle, cx);
                cx.notify();
            }))
            .child(
                div()
                    .w(relative(0.08))
                    .flex_shrink_0()
                    .min_w_0()
                    .px_2p5()
                    .py_2p5()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.primary)
                    .child(display_id_owned),
            )
            .child(table_text_cell(0.22, row.data.description.clone(), theme.foreground))
            .child(table_text_cell(
                0.17,
                if row.data.pre_conditions.is_empty() {
                    "—".to_string()
                } else {
                    row.data.pre_conditions.clone()
                },
                theme.muted_foreground,
            ))
            .child(table_text_cell(0.21, row.data.steps.clone(), theme.foreground))
            .child(table_text_cell(
                0.13,
                row.data.expected_result.clone(),
                theme.foreground,
            ))
            .child(
                h_flex()
                    .w(relative(0.09))
                    .flex_shrink_0()
                    .min_w_0()
                    .px_2()
                    .py_2()
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(prio_color.opacity(0.45))
                            .bg(prio_color.opacity(0.14))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(prio_color)
                            .truncate()
                            .child(prio_label),
                    ),
            )
            .child(
                h_flex()
                    .w(relative(0.10))
                    .flex_shrink_0()
                    .min_w_0()
                    .px_1p5()
                    .py_2()
                    .items_center()
                    .justify_end()
                    .gap_0p5()
                    .child(
                        Button::new(("tc-move-up", uid))
                            .ghost()
                            .xsmall()
                            .disabled(is_generating || index == 0)
                            .icon(Icon::new(LucideIcon::ChevronUp).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(target) = index.checked_sub(1) {
                                    this.move_row(index, target, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new(("tc-move-down", uid))
                            .ghost()
                            .xsmall()
                            .disabled(is_generating || index + 1 >= total)
                            .icon(Icon::new(LucideIcon::ChevronDown).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.move_row(index, index + 1, cx);
                            })),
                    )
                    .child(
                        Button::new(("tc-delete", uid))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(LucideIcon::Trash).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_row(index, cx);
                            })),
                    )
                    .child(drag_handle),
            )
    }

    fn render_deleted_row(
        &self,
        index: usize,
        uid: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_generating = self.is_generating;
        h_flex()
            .id(("tc-row-deleted", uid))
            .w_full()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.45))
            .drag_over::<DraggedTestCaseRow>(move |style, dragged, _, cx| {
                if is_generating || dragged.from_index == index {
                    return style;
                }
                let primary = cx.theme().primary;
                let base = style.bg(primary.opacity(0.1)).border_color(primary);
                if dragged.from_index < index {
                    base.border_b_2()
                } else {
                    base.border_t_2()
                }
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedTestCaseRow, _, cx| {
                this.move_row(dragged.from_index, index, cx);
            }))
            .child(
                div()
                    .w(relative(0.08))
                    .flex_shrink_0()
                    .px_2p5()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("-"),
            )
            .child(
                h_flex()
                    .w(relative(0.82))
                    .flex_shrink_0()
                    .min_w_0()
                    .px_2p5()
                    .py_2()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(LucideIcon::Trash).xsmall())
                    .child("Test scenario deleted."),
            )
            .child(
                h_flex()
                    .w(relative(0.10))
                    .flex_shrink_0()
                    .px_2()
                    .py_1p5()
                    .justify_end()
                    .child(
                        Button::new(("tc-undo", uid))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(LucideIcon::Undo2).xsmall())
                            .label("Undo")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.restore_row(index, cx);
                            })),
                    ),
            )
    }
}

impl Render for TestCaseWriterView {
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
                                        Icon::new(ActiveView::TestCaseWriter.icon())
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
                                            .child(ActiveView::TestCaseWriter.label()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(ActiveView::TestCaseWriter.subtitle()),
                                    ),
                            ),
                    )
                    .when(!connectors_ready, |header| {
                        header.child(
                            Button::new("tc-header-connectors")
                                .ghost()
                                .small()
                                .icon(Icon::new(LucideIcon::Plug).small())
                                .label("Connectors")
                                .on_click(move |_, _, cx| {
                                    app_state_for_header.update(cx, |state, cx| {
                                        state.navigate_to_settings_tab(SettingsTab::Connectors, cx);
                                        cx.notify();
                                    });
                                }),
                        )
                    }),
            )
            .child(
                div().flex_1().min_h_0().w_full().child(
                    h_resizable("test-case-writer-split")
                        .child(
                            resizable_panel()
                                .size(px(390.))
                                .size_range(px(320.)..px(540.))
                                .child(self.render_left_pane(cx)),
                        )
                        .child(resizable_panel().child(self.render_right_pane(cx))),
                ),
            )
    }
}

fn ticket_id_mode_for(settings: &AppSettings) -> DirectIdMode {
    if settings.sources.issues.trim() == "github" {
        DirectIdMode::NumericOrRepoIssue
    } else {
        DirectIdMode::NumericOnly
    }
}

fn render_table_header(theme: &Theme) -> impl IntoElement {
    let columns: &[(f32, &str, bool)] = &[
        (0.08, "ID", false),
        (0.22, "Description", false),
        (0.17, "Pre-conditions", false),
        (0.21, "Steps", false),
        (0.13, "Expected Result", false),
        (0.09, "Priority", false),
        (0.10, "Actions", true),
    ];

    h_flex()
        .w_full()
        .items_center()
        .bg(theme.secondary)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .children(columns.iter().map(|&(width_frac, label, align_right)| {
            div()
                .w(relative(width_frac))
                .flex_shrink_0()
                .min_w_0()
                .px_2p5()
                .py_2p5()
                .when(align_right, |cell| cell.text_right())
                .child(label)
        }))
}

fn table_text_cell(width_frac: f32, text: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    div()
        .w(relative(width_frac))
        .flex_shrink_0()
        .min_w_0()
        .px_2p5()
        .py_2p5()
        .text_xs()
        .text_color(color)
        .child(text.into())
}

fn priority_badge_style(raw_priority: &str, theme: &Theme) -> (String, Hsla) {
    let trimmed = raw_priority.trim();
    let label = if trimmed.is_empty() {
        "Medium".to_string()
    } else {
        trimmed.to_string()
    };
    let color = match label.to_ascii_lowercase().as_str() {
        "high" => hsla(0.0, 0.78, 0.55, 1.0),
        "medium" => hsla(38.0 / 360.0, 0.92, 0.50, 1.0),
        _ => theme.muted_foreground,
    };
    (label, color)
}
