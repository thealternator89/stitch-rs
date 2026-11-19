use std::cell::Cell;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{
    SearchableVec, Select, SelectEvent, SelectItem, SelectState,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Sizable as _, Size, Theme,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, FontWeight, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, hsla,
    prelude::FluentBuilder as _, px,
};

use crate::app_state::AppState;
use crate::infrastructure::providers::AzureDevOpsClient;
use crate::infrastructure::runtime::spawn_tokio;
use crate::types::{AppSettings, ConnectorsConfig, SourcesConfig};

#[derive(Debug, Clone)]
enum ConnectorFeedback {
    Saved(String),
    Error(String),
}

#[derive(Clone)]
struct SourceRoutingOption {
    value: String,
    title: SharedString,
    is_disabled: bool,
}

impl SelectItem for SourceRoutingOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }

    fn disabled(&self) -> bool {
        self.is_disabled
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = if self.is_disabled {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };
        div().text_color(color).truncate().child(self.title.clone())
    }
}

#[derive(Clone)]
struct WorkItemTypeOption {
    value: String,
    title: SharedString,
}

impl SelectItem for WorkItemTypeOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// Settings > Connectors tab: Azure DevOps, Atlassian Confluence, GitHub, and Source Routing.
pub struct ConnectorsSettingsView {
    app_state: Entity<AppState>,
    issues_source_select: Entity<SelectState<SearchableVec<SourceRoutingOption>>>,
    code_source_select: Entity<SelectState<SearchableVec<SourceRoutingOption>>>,
    docs_source_select: Entity<SelectState<SearchableVec<SourceRoutingOption>>>,
    last_azure_configured: bool,
    last_atlassian_configured: bool,
    last_github_configured: bool,
    last_sources: SourcesConfig,
    feedback: Option<ConnectorFeedback>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectorsSettingsView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let issues_options = build_code_or_issues_options(&initial_settings.connectors);
        let issues_row = selected_option_row(&issues_options, &initial_settings.sources.issues);
        let issues_source_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(issues_options),
                Some(IndexPath::default().row(issues_row)),
                window,
                cx,
            )
        });

        let code_options = build_code_or_issues_options(&initial_settings.connectors);
        let code_row = selected_option_row(&code_options, &initial_settings.sources.code);
        let code_source_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(code_options),
                Some(IndexPath::default().row(code_row)),
                window,
                cx,
            )
        });

        let docs_options = build_docs_options(&initial_settings.connectors);
        let docs_row = selected_option_row(&docs_options, &initial_settings.sources.docs);
        let docs_source_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(docs_options),
                Some(IndexPath::default().row(docs_row)),
                window,
                cx,
            )
        });

        let issues_sub = cx.subscribe(&issues_source_select, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(value)) = event {
                let value = value.clone();
                this.update_source_routing(cx, move |sources| sources.issues = value);
            }
        });

        let code_sub = cx.subscribe(&code_source_select, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(value)) = event {
                let value = value.clone();
                this.update_source_routing(cx, move |sources| sources.code = value);
            }
        });

        let docs_sub = cx.subscribe(&docs_source_select, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(value)) = event {
                let value = value.clone();
                this.update_source_routing(cx, move |sources| sources.docs = value);
            }
        });

        let state_sub = cx.observe_in(&app_state, window, |this, app_state, window, cx| {
            let settings = &app_state.read(cx).settings;
            let azure_cfg = settings.connectors.is_azure_configured();
            let atlassian_cfg = settings.connectors.is_atlassian_configured();
            let github_cfg = settings.connectors.is_github_configured();
            let sources = settings.sources.clone();

            if azure_cfg != this.last_azure_configured
                || atlassian_cfg != this.last_atlassian_configured
                || github_cfg != this.last_github_configured
                || sources != this.last_sources
            {
                this.last_azure_configured = azure_cfg;
                this.last_atlassian_configured = atlassian_cfg;
                this.last_github_configured = github_cfg;
                this.last_sources = sources;
                this.sync_source_selects(window, cx);
            }
            cx.notify();
        });

        Self {
            app_state,
            issues_source_select,
            code_source_select,
            docs_source_select,
            last_azure_configured: initial_settings.connectors.is_azure_configured(),
            last_atlassian_configured: initial_settings.connectors.is_atlassian_configured(),
            last_github_configured: initial_settings.connectors.is_github_configured(),
            last_sources: initial_settings.sources.clone(),
            feedback: None,
            _subscriptions: vec![issues_sub, code_sub, docs_sub, state_sub],
        }
    }

    fn set_feedback(&mut self, feedback: ConnectorFeedback, cx: &mut Context<Self>) {
        self.feedback = Some(feedback);
        cx.notify();
    }

    fn update_source_routing<F>(&mut self, cx: &mut Context<Self>, apply: F)
    where
        F: FnOnce(&mut SourcesConfig),
    {
        let res = self.app_state.update(cx, |state, cx| {
            apply(&mut state.settings.sources);
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        match res {
            Ok(()) => {
                self.feedback = Some(ConnectorFeedback::Saved(
                    "Source routing updated.".to_string(),
                ));
            }
            Err(err) => {
                self.feedback = Some(ConnectorFeedback::Error(format!(
                    "Failed to save source routing: {err}"
                )));
            }
        }
        cx.notify();
    }

    fn sync_source_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.app_state.read(cx).settings.clone();

        let issues_options = build_code_or_issues_options(&settings.connectors);
        let issues_val = normalize_selected_value(&issues_options, &settings.sources.issues);
        self.issues_source_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(issues_options), window, cx);
            select.set_selected_value(&issues_val, window, cx);
        });

        let code_options = build_code_or_issues_options(&settings.connectors);
        let code_val = normalize_selected_value(&code_options, &settings.sources.code);
        self.code_source_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(code_options), window, cx);
            select.set_selected_value(&code_val, window, cx);
        });

        let docs_options = build_docs_options(&settings.connectors);
        let docs_val = normalize_selected_value(&docs_options, &settings.sources.docs);
        self.docs_source_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(docs_options), window, cx);
            select.set_selected_value(&docs_val, window, cx);
        });
    }

    fn disconnect_azure(&mut self, cx: &mut Context<Self>) {
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.connectors.azure_dev_ops.org.clear();
            state.settings.connectors.azure_dev_ops.project.clear();
            state.settings.connectors.azure_dev_ops.pat = None;
            clear_sources_for(&mut state.settings.sources, "azureDevOps");
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_disconnect_result("Azure DevOps", res, cx);
    }

    fn disconnect_atlassian(&mut self, cx: &mut Context<Self>) {
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.connectors.atlassian.url.clear();
            state.settings.connectors.atlassian.username.clear();
            state.settings.connectors.atlassian.token = None;
            clear_sources_for(&mut state.settings.sources, "atlassian");
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_disconnect_result("Atlassian Confluence", res, cx);
    }

    fn disconnect_github(&mut self, cx: &mut Context<Self>) {
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.connectors.github.token = None;
            state.settings.connectors.github.owner.clear();
            clear_sources_for(&mut state.settings.sources, "github");
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_disconnect_result("GitHub", res, cx);
    }

    fn apply_disconnect_result(
        &mut self,
        name: &str,
        res: anyhow::Result<()>,
        cx: &mut Context<Self>,
    ) {
        match res {
            Ok(()) => {
                self.feedback = Some(ConnectorFeedback::Saved(format!(
                    "{name} disconnected and credentials removed from OS Keychain."
                )));
            }
            Err(err) => {
                self.feedback = Some(ConnectorFeedback::Error(format!(
                    "Failed to disconnect {name}: {err}"
                )));
            }
        }
        cx.notify();
    }

    fn open_azure_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.app_state.read(cx).settings.clone();
        let app_state = self.app_state.clone();
        let parent_view = cx.entity();
        let modal = cx.new(|cx| AzureConnectorModal::new(app_state, parent_view, &settings, window, cx));
        let initial_focus = Cell::new(true);

        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                modal.read(cx).org_input.focus_handle(cx).focus(window, cx);
            }
            let modal_for_save = modal.clone();
            dialog
                .w(px(580.))
                .overlay_closable(true)
                .close_button(true)
                .title(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(LucideIcon::Cloud).with_size(Size::Medium))
                        .child("Configure Azure DevOps"),
                )
                .child(modal.clone())
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("azure-modal-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("azure-modal-save")
                                .primary()
                                .icon(Icon::new(LucideIcon::Save).small())
                                .label("Save")
                                .on_click(move |_, window, cx| {
                                    let closed = modal_for_save.update(cx, |m, cx| m.save(cx));
                                    if closed {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    fn open_atlassian_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.app_state.read(cx).settings.clone();
        let app_state = self.app_state.clone();
        let parent_view = cx.entity();
        let modal =
            cx.new(|cx| AtlassianConnectorModal::new(app_state, parent_view, &settings, window, cx));
        let initial_focus = Cell::new(true);

        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                modal.read(cx).url_input.focus_handle(cx).focus(window, cx);
            }
            let modal_for_save = modal.clone();
            dialog
                .w(px(520.))
                .overlay_closable(true)
                .close_button(true)
                .title(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(LucideIcon::BookOpen).with_size(Size::Medium))
                        .child("Configure Atlassian Confluence"),
                )
                .child(modal.clone())
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("atlassian-modal-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("atlassian-modal-save")
                                .primary()
                                .icon(Icon::new(LucideIcon::Save).small())
                                .label("Save")
                                .on_click(move |_, window, cx| {
                                    let closed = modal_for_save.update(cx, |m, cx| m.save(cx));
                                    if closed {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    fn open_github_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.app_state.read(cx).settings.clone();
        let app_state = self.app_state.clone();
        let parent_view = cx.entity();
        let modal =
            cx.new(|cx| GitHubConnectorModal::new(app_state, parent_view, &settings, window, cx));
        let initial_focus = Cell::new(true);

        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                modal
                    .read(cx)
                    .token_input
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            let modal_for_save = modal.clone();
            dialog
                .w(px(520.))
                .overlay_closable(true)
                .close_button(true)
                .title(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(LucideIcon::Github).with_size(Size::Medium))
                        .child("Configure GitHub"),
                )
                .child(modal.clone())
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("github-modal-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("github-modal-save")
                                .primary()
                                .icon(Icon::new(LucideIcon::Save).small())
                                .label("Save")
                                .on_click(move |_, window, cx| {
                                    let closed = modal_for_save.update(cx, |m, cx| m.save(cx));
                                    if closed {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    fn render_connector_card(
        &self,
        id_prefix: &'static str,
        icon: LucideIcon,
        name: &'static str,
        is_configured: bool,
        details: Vec<(&'static str, String)>,
        on_edit: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        on_disconnect: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);

        h_flex()
            .p_4()
            .items_center()
            .justify_between()
            .gap_4()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .flex_shrink_0()
                            .size(px(40.))
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .text_color(theme.accent_foreground)
                            .child(Icon::new(icon).with_size(Size::Medium)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(name),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(if is_configured {
                                                green.opacity(0.4)
                                            } else {
                                                theme.border
                                            })
                                            .bg(if is_configured {
                                                green.opacity(0.12)
                                            } else {
                                                theme.secondary
                                            })
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(if is_configured {
                                                green
                                            } else {
                                                theme.muted_foreground
                                            })
                                            .child(if is_configured {
                                                "Connected"
                                            } else {
                                                "Not Connected"
                                            }),
                                    ),
                            )
                            .when(is_configured && !details.is_empty(), |col| {
                                col.child(v_flex().gap_0p5().children(details.into_iter().map(
                                    |(label, val)| {
                                        h_flex()
                                            .gap_1p5()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(format!("{label}:")),
                                            )
                                            .child(div().truncate().child(val))
                                    },
                                )))
                            }),
                    ),
            )
            .child(if is_configured {
                h_flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new((id_prefix, 1usize))
                            .ghost()
                            .small()
                            .icon(Icon::new(LucideIcon::Pencil).small())
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_edit(this, window, cx);
                            })),
                    )
                    .child(
                        Button::new((id_prefix, 2usize))
                            .danger()
                            .small()
                            .icon(Icon::new(LucideIcon::Unlink).small())
                            .label("Disconnect")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                on_disconnect(this, cx);
                            })),
                    )
                    .into_any_element()
            } else {
                Button::new((id_prefix, 3usize))
                    .primary()
                    .small()
                    .icon(Icon::new(LucideIcon::Link).small())
                    .label("Connect")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        on_edit(this, window, cx);
                    }))
                    .into_any_element()
            })
    }
}

impl Render for ConnectorsSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let settings = self.app_state.read(cx).settings.clone();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        let azure_configured = settings.connectors.is_azure_configured();
        let mut azure_details = Vec::new();
        if azure_configured {
            azure_details.push(("Org", settings.connectors.azure_dev_ops.org.clone()));
            if !settings.connectors.azure_dev_ops.project.is_empty() {
                azure_details.push((
                    "Project",
                    settings.connectors.azure_dev_ops.project.clone(),
                ));
            }
            azure_details.push((
                "Types",
                format!(
                    "{} / {} / {}",
                    settings.feature_type, settings.story_type, settings.task_type
                ),
            ));
        }

        let atlassian_configured = settings.connectors.is_atlassian_configured();
        let mut atlassian_details = Vec::new();
        if atlassian_configured {
            atlassian_details.push(("URL", settings.connectors.atlassian.url.clone()));
            if !settings.connectors.atlassian.username.is_empty() {
                atlassian_details.push(("User", settings.connectors.atlassian.username.clone()));
            }
        }

        let github_configured = settings.connectors.is_github_configured();
        let mut github_details = Vec::new();
        if github_configured {
            if !settings.connectors.github.owner.is_empty() {
                github_details.push(("Default Owner", settings.connectors.github.owner.clone()));
            } else {
                github_details.push(("Auth", "Personal Access Token configured".to_string()));
            }
        }

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
                            .child("Connectors & Source Routing"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Connect external platforms (PATs and API tokens are stored in your OS Keychain) and route Issues, Code, and Documentation sources."),
                    ),
            )
            .when_some(self.feedback.clone(), |col, fb| {
                let (msg, color, icon) = match fb {
                    ConnectorFeedback::Saved(m) => (m, green, LucideIcon::Check),
                    ConnectorFeedback::Error(m) => (m, red, LucideIcon::CircleAlert),
                };
                col.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(color.opacity(0.4))
                        .bg(color.opacity(0.08))
                        .text_xs()
                        .text_color(color)
                        .child(Icon::new(icon).small())
                        .child(div().flex_1().child(msg)),
                )
            })
            // Integrations & Connectors Card
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
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Icon::new(LucideIcon::Plug).small())
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Integrations & Connectors"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Connect and configure external platforms, then select which data sources route through them below."),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_3()
                            .child(self.render_connector_card(
                                "azure",
                                LucideIcon::Cloud,
                                "Azure DevOps",
                                azure_configured,
                                azure_details,
                                |this, window, cx| this.open_azure_modal(window, cx),
                                |this, cx| this.disconnect_azure(cx),
                                cx,
                            ))
                            .child(self.render_connector_card(
                                "atlassian",
                                LucideIcon::BookOpen,
                                "Atlassian Confluence",
                                atlassian_configured,
                                atlassian_details,
                                |this, window, cx| this.open_atlassian_modal(window, cx),
                                |this, cx| this.disconnect_atlassian(cx),
                                cx,
                            ))
                            .child(self.render_connector_card(
                                "github",
                                LucideIcon::Github,
                                "GitHub",
                                github_configured,
                                github_details,
                                |this, window, cx| this.open_github_modal(window, cx),
                                |this, cx| this.disconnect_github(cx),
                                cx,
                            ))
                            // Notion (Coming Soon)
                            .child(
                                h_flex()
                                    .p_4()
                                    .items_center()
                                    .justify_between()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_dashed()
                                    .border_color(theme.border)
                                    .bg(theme.background.opacity(0.5))
                                    .opacity(0.75)
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .size(px(40.))
                                                    .rounded(theme.radius)
                                                    .bg(theme.secondary)
                                                    .text_color(theme.muted_foreground)
                                                    .child(
                                                        Icon::new(LucideIcon::FileText)
                                                            .with_size(Size::Medium),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(theme.muted_foreground)
                                                    .child("Notion"),
                                            ),
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
                                            .child("Coming Soon"),
                                    ),
                            ),
                    ),
            )
            // Source Routing Card
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
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(Icon::new(LucideIcon::Route).small())
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Source Routing"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Route data sources to your connected platforms. Unconfigured connectors are disabled until connected."),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_3()
                            .child(render_routing_row(
                                "Issues Tracker Source",
                                "Used by Story Writer, Test Case Writer, and Story Elaborator.",
                                Select::new(&self.issues_source_select).placeholder("None"),
                                &theme,
                            ))
                            .child(render_routing_row(
                                "Code Source",
                                "Used by PR Reviewer to discover and comment on Pull Requests.",
                                Select::new(&self.code_source_select).placeholder("None"),
                                &theme,
                            ))
                            .child(render_routing_row(
                                "Documentation Source",
                                "Used by Story Writer and Story Elaborator to read specs.",
                                Select::new(&self.docs_source_select).placeholder("None"),
                                &theme,
                            )),
                    ),
            )
    }
}

struct AzureConnectorModal {
    app_state: Entity<AppState>,
    parent_view: Entity<ConnectorsSettingsView>,
    org_input: Entity<InputState>,
    project_input: Entity<InputState>,
    pat_input: Entity<InputState>,
    feature_type_input: Entity<InputState>,
    story_type_input: Entity<InputState>,
    task_type_input: Entity<InputState>,
    test_task_title_input: Entity<InputState>,
    work_item_types: Vec<String>,
    feature_type_select: Entity<SelectState<SearchableVec<WorkItemTypeOption>>>,
    story_type_select: Entity<SelectState<SearchableVec<WorkItemTypeOption>>>,
    task_type_select: Entity<SelectState<SearchableVec<WorkItemTypeOption>>>,
    is_loading_types: bool,
    fetch_error: Option<String>,
    save_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl AzureConnectorModal {
    fn new(
        app_state: Entity<AppState>,
        parent_view: Entity<ConnectorsSettingsView>,
        settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ado = &settings.connectors.azure_dev_ops;
        let org_val = ado.org.clone();
        let project_val = ado.project.clone();
        let pat_val = ado.pat.clone().unwrap_or_default();

        let org_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://dev.azure.com/your-org")
                .default_value(org_val.clone())
        });
        let project_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("YourProject")
                .default_value(project_val.clone())
        });
        let pat_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Personal Access Token (stored in OS Keychain)")
                .masked(true)
                .default_value(pat_val.clone())
        });

        let feature_val = settings.feature_type.clone();
        let story_val = settings.story_type.clone();
        let task_val = settings.task_type.clone();
        let test_title_val = settings.test_task_title.clone();

        let feature_type_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Feature")
                .default_value(feature_val.clone())
        });
        let story_type_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Product Backlog Item")
                .default_value(story_val.clone())
        });
        let task_type_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Task")
                .default_value(task_val.clone())
        });
        let test_task_title_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Testing")
                .default_value(test_title_val)
        });

        let feature_type_select = create_type_select(&[], &feature_val, window, cx);
        let story_type_select = create_type_select(&[], &story_val, window, cx);
        let task_type_select = create_type_select(&[], &task_val, window, cx);

        let sub_org = cx.subscribe(&org_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let sub_proj = cx.subscribe(&project_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let sub_pat = cx.subscribe(&pat_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        let sub_feat_sel = cx.subscribe_in(&feature_type_select, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(val)) = event {
                let val = val.clone();
                this.feature_type_input.update(cx, |input, cx| {
                    input.set_value(val, window, cx);
                });
            }
        });
        let sub_story_sel = cx.subscribe_in(&story_type_select, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(val)) = event {
                let val = val.clone();
                this.story_type_input.update(cx, |input, cx| {
                    input.set_value(val, window, cx);
                });
            }
        });
        let sub_task_sel = cx.subscribe_in(&task_type_select, window, |this, _, event, window, cx| {
            if let SelectEvent::Confirm(Some(val)) = event {
                let val = val.clone();
                this.task_type_input.update(cx, |input, cx| {
                    input.set_value(val, window, cx);
                });
            }
        });

        let can_autofetch = !org_val.trim().is_empty()
            && !project_val.trim().is_empty()
            && !pat_val.trim().is_empty();

        let mut modal = Self {
            app_state,
            parent_view,
            org_input,
            project_input,
            pat_input,
            feature_type_input,
            story_type_input,
            task_type_input,
            test_task_title_input,
            work_item_types: Vec::new(),
            feature_type_select,
            story_type_select,
            task_type_select,
            is_loading_types: false,
            fetch_error: None,
            save_error: None,
            _subscriptions: vec![
                sub_org,
                sub_proj,
                sub_pat,
                sub_feat_sel,
                sub_story_sel,
                sub_task_sel,
            ],
        };

        if can_autofetch {
            modal.fetch_work_item_types(window, cx);
        }

        modal
    }

    fn can_fetch_types(&self, cx: &App) -> bool {
        !self.org_input.read(cx).value().trim().is_empty()
            && !self.project_input.read(cx).value().trim().is_empty()
            && !self.pat_input.read(cx).value().trim().is_empty()
    }

    fn fetch_work_item_types(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let org = self.org_input.read(cx).value().trim().to_string();
        let project = self.project_input.read(cx).value().trim().to_string();
        let pat = self.pat_input.read(cx).value().trim().to_string();

        if org.is_empty() || project.is_empty() || pat.is_empty() {
            self.fetch_error = Some(
                "Organization URL, Project Name, and PAT are required to fetch work item types."
                    .to_string(),
            );
            cx.notify();
            return;
        }

        self.is_loading_types = true;
        self.fetch_error = None;
        cx.notify();

        let modal = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    let client = AzureDevOpsClient::new(org, pat);
                    client.get_work_item_types(&project).await
                })
                .await;

                let _ = cx.update(|window, cx| {
                    modal.update(cx, |this, cx| {
                        this.is_loading_types = false;
                        match res {
                            Ok(Ok(types)) => {
                                this.work_item_types = types;
                                this.fetch_error = None;
                                this.sync_type_selects(window, cx);
                            }
                            Ok(Err(err)) => {
                                this.fetch_error = Some(err.to_string());
                            }
                            Err(err) => {
                                this.fetch_error = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn sync_type_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let feat = self.feature_type_input.read(cx).value().trim().to_string();
        let story = self.story_type_input.read(cx).value().trim().to_string();
        let task = self.task_type_input.read(cx).value().trim().to_string();

        update_type_select(&self.feature_type_select, &self.work_item_types, &feat, window, cx);
        update_type_select(&self.story_type_select, &self.work_item_types, &story, window, cx);
        update_type_select(&self.task_type_select, &self.work_item_types, &task, window, cx);
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let org = self.org_input.read(cx).value().trim().to_string();
        let project = self.project_input.read(cx).value().trim().to_string();
        let pat_raw = self.pat_input.read(cx).value().trim().to_string();
        let pat = if pat_raw.is_empty() {
            None
        } else {
            Some(pat_raw)
        };

        let feature_type = non_empty_or(&self.feature_type_input.read(cx).value(), "Feature");
        let story_type = non_empty_or(
            &self.story_type_input.read(cx).value(),
            "Product Backlog Item",
        );
        let task_type = non_empty_or(&self.task_type_input.read(cx).value(), "Task");
        let test_task_title =
            non_empty_or(&self.test_task_title_input.read(cx).value(), "Testing");

        let save_res = self.app_state.update(cx, |state, cx| {
            state.settings.connectors.azure_dev_ops.org = org;
            state.settings.connectors.azure_dev_ops.project = project;
            state.settings.connectors.azure_dev_ops.pat = pat;
            state.settings.feature_type = feature_type;
            state.settings.story_type = story_type;
            state.settings.task_type = task_type;
            state.settings.test_task_title = test_task_title;

            if state.settings.connectors.is_azure_configured() {
                if state.settings.sources.issues.trim().is_empty() {
                    state.settings.sources.issues = "azureDevOps".to_string();
                }
                if state.settings.sources.code.trim().is_empty() {
                    state.settings.sources.code = "azureDevOps".to_string();
                }
            } else {
                clear_sources_for(&mut state.settings.sources, "azureDevOps");
            }

            let res = state.save_settings();
            cx.notify();
            res
        });

        match save_res {
            Ok(()) => {
                self.save_error = None;
                self.parent_view.update(cx, |parent, cx| {
                    parent.set_feedback(
                        ConnectorFeedback::Saved(
                            "Azure DevOps configuration saved (PAT stored in OS Keychain)."
                                .to_string(),
                        ),
                        cx,
                    );
                });
                true
            }
            Err(err) => {
                self.save_error = Some(format!("Failed to save settings: {err}"));
                cx.notify();
                false
            }
        }
    }
}

impl Render for AzureConnectorModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let can_fetch = self.can_fetch_types(cx);
        let has_types = !self.work_item_types.is_empty();

        v_flex()
            .gap_3()
            .text_sm()
            .child(render_modal_field_row(
                "Organization URL",
                Input::new(&self.org_input).cleanable(true),
                theme,
            ))
            .child(render_modal_field_row(
                "Project Name",
                Input::new(&self.project_input).cleanable(true),
                theme,
            ))
            .child(render_modal_field_row(
                "Personal Access Token",
                Input::new(&self.pat_input).mask_toggle().cleanable(true),
                theme,
            ))
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                v_flex()
                    .gap_2p5()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Work Item Types"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Fetch types from your Azure DevOps project or enter custom names."),
                                    ),
                            )
                            .child(
                                Button::new("azure-fetch-types")
                                    .small()
                                    .disabled(!can_fetch || self.is_loading_types)
                                    .loading(self.is_loading_types)
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label(if self.is_loading_types {
                                        "Fetching..."
                                    } else {
                                        "Fetch Types"
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.fetch_work_item_types(window, cx);
                                    })),
                            ),
                    )
                    .when_some(self.fetch_error.clone(), |col, err| {
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
                    .child(render_modal_field_row(
                        "Feature Type",
                        if has_types {
                            Select::new(&self.feature_type_select)
                                .search_placeholder("Search work item types...")
                                .into_any_element()
                        } else {
                            Input::new(&self.feature_type_input).into_any_element()
                        },
                        theme,
                    ))
                    .child(render_modal_field_row(
                        "Story Type",
                        if has_types {
                            Select::new(&self.story_type_select)
                                .search_placeholder("Search work item types...")
                                .into_any_element()
                        } else {
                            Input::new(&self.story_type_input).into_any_element()
                        },
                        theme,
                    ))
                    .child(render_modal_field_row(
                        "Task Type",
                        if has_types {
                            Select::new(&self.task_type_select)
                                .search_placeholder("Search work item types...")
                                .into_any_element()
                        } else {
                            Input::new(&self.task_type_input).into_any_element()
                        },
                        theme,
                    ))
                    .child(render_modal_field_row(
                        "Test Task Title",
                        Input::new(&self.test_task_title_input),
                        theme,
                    )),
            )
            .when_some(self.save_error.clone(), |col, err| {
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
    }
}

struct AtlassianConnectorModal {
    app_state: Entity<AppState>,
    parent_view: Entity<ConnectorsSettingsView>,
    url_input: Entity<InputState>,
    username_input: Entity<InputState>,
    token_input: Entity<InputState>,
    save_error: Option<String>,
}

impl AtlassianConnectorModal {
    fn new(
        app_state: Entity<AppState>,
        parent_view: Entity<ConnectorsSettingsView>,
        settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let atl = &settings.connectors.atlassian;
        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://your-domain.atlassian.net/wiki")
                .default_value(atl.url.clone())
        });
        let username_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("user@example.com (Optional for Bearer PAT)")
                .default_value(atl.username.clone())
        });
        let token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("API Token / PAT (stored in OS Keychain)")
                .masked(true)
                .default_value(atl.token.clone().unwrap_or_default())
        });

        Self {
            app_state,
            parent_view,
            url_input,
            username_input,
            token_input,
            save_error: None,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let url = self.url_input.read(cx).value().trim().to_string();
        let username = self.username_input.read(cx).value().trim().to_string();
        let token_raw = self.token_input.read(cx).value().trim().to_string();
        let token = if token_raw.is_empty() {
            None
        } else {
            Some(token_raw)
        };

        let save_res = self.app_state.update(cx, |state, cx| {
            state.settings.connectors.atlassian.url = url;
            state.settings.connectors.atlassian.username = username;
            state.settings.connectors.atlassian.token = token;

            if state.settings.connectors.is_atlassian_configured() {
                if state.settings.sources.docs.trim().is_empty() {
                    state.settings.sources.docs = "atlassian".to_string();
                }
            } else {
                clear_sources_for(&mut state.settings.sources, "atlassian");
            }

            let res = state.save_settings();
            cx.notify();
            res
        });

        match save_res {
            Ok(()) => {
                self.save_error = None;
                self.parent_view.update(cx, |parent, cx| {
                    parent.set_feedback(
                        ConnectorFeedback::Saved(
                            "Atlassian Confluence configuration saved (token stored in OS Keychain)."
                                .to_string(),
                        ),
                        cx,
                    );
                });
                true
            }
            Err(err) => {
                self.save_error = Some(format!("Failed to save settings: {err}"));
                cx.notify();
                false
            }
        }
    }
}

impl Render for AtlassianConnectorModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        v_flex()
            .gap_3()
            .text_sm()
            .child(render_modal_field_row(
                "Confluence URL",
                Input::new(&self.url_input).cleanable(true),
                theme,
            ))
            .child(render_modal_field_row(
                "Email / Username",
                Input::new(&self.username_input).cleanable(true),
                theme,
            ))
            .child(render_modal_field_row(
                "API Token / PAT",
                Input::new(&self.token_input).mask_toggle().cleanable(true),
                theme,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Leave Email / Username blank to authenticate with a Bearer PAT; provide your Atlassian email with an API Token for Basic Auth."),
            )
            .when_some(self.save_error.clone(), |col, err| {
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
    }
}

struct GitHubConnectorModal {
    app_state: Entity<AppState>,
    parent_view: Entity<ConnectorsSettingsView>,
    token_input: Entity<InputState>,
    owner_input: Entity<InputState>,
    feature_label_input: Entity<InputState>,
    story_label_input: Entity<InputState>,
    task_label_input: Entity<InputState>,
    save_error: Option<String>,
}

impl GitHubConnectorModal {
    fn new(
        app_state: Entity<AppState>,
        parent_view: Entity<ConnectorsSettingsView>,
        settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let gh = &settings.connectors.github;
        let token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("ghp_... (stored in OS Keychain)")
                .masked(true)
                .default_value(gh.token.clone().unwrap_or_default())
        });
        let owner_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. octocat")
                .default_value(gh.owner.clone())
        });
        let feature_label_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("feature")
                .default_value(settings.feature_type.clone())
        });
        let story_label_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("story")
                .default_value(settings.story_type.clone())
        });
        let task_label_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("task")
                .default_value(settings.task_type.clone())
        });

        Self {
            app_state,
            parent_view,
            token_input,
            owner_input,
            feature_label_input,
            story_label_input,
            task_label_input,
            save_error: None,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let token_raw = self.token_input.read(cx).value().trim().to_string();
        let token = if token_raw.is_empty() {
            None
        } else {
            Some(token_raw)
        };
        let owner = self.owner_input.read(cx).value().trim().to_string();
        let feature_type = non_empty_or(&self.feature_label_input.read(cx).value(), "Feature");
        let story_type = non_empty_or(
            &self.story_label_input.read(cx).value(),
            "Product Backlog Item",
        );
        let task_type = non_empty_or(&self.task_label_input.read(cx).value(), "Task");

        let save_res = self.app_state.update(cx, |state, cx| {
            state.settings.connectors.github.token = token;
            state.settings.connectors.github.owner = owner;
            state.settings.feature_type = feature_type;
            state.settings.story_type = story_type;
            state.settings.task_type = task_type;

            if state.settings.connectors.is_github_configured() {
                if state.settings.sources.issues.trim().is_empty() {
                    state.settings.sources.issues = "github".to_string();
                }
                if state.settings.sources.code.trim().is_empty() {
                    state.settings.sources.code = "github".to_string();
                }
            } else {
                clear_sources_for(&mut state.settings.sources, "github");
            }

            let res = state.save_settings();
            cx.notify();
            res
        });

        match save_res {
            Ok(()) => {
                self.save_error = None;
                self.parent_view.update(cx, |parent, cx| {
                    parent.set_feedback(
                        ConnectorFeedback::Saved(
                            "GitHub configuration saved (token stored in OS Keychain).".to_string(),
                        ),
                        cx,
                    );
                });
                true
            }
            Err(err) => {
                self.save_error = Some(format!("Failed to save settings: {err}"));
                cx.notify();
                false
            }
        }
    }
}

impl Render for GitHubConnectorModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        v_flex()
            .gap_3()
            .text_sm()
            .child(render_modal_field_row(
                "Personal Access Token",
                Input::new(&self.token_input).mask_toggle().cleanable(true),
                theme,
            ))
            .child(render_modal_field_row(
                "Default Owner / Org",
                Input::new(&self.owner_input).cleanable(true),
                theme,
            ))
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                v_flex()
                    .gap_2p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Labels Mapping"),
                    )
                    .child(render_modal_field_row(
                        "Feature Label",
                        Input::new(&self.feature_label_input),
                        theme,
                    ))
                    .child(render_modal_field_row(
                        "Story Label",
                        Input::new(&self.story_label_input),
                        theme,
                    ))
                    .child(render_modal_field_row(
                        "Task Label",
                        Input::new(&self.task_label_input),
                        theme,
                    )),
            )
            .when_some(self.save_error.clone(), |col, err| {
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
    }
}

fn render_routing_row<T: IntoElement>(
    label: &'static str,
    hint: &'static str,
    control: T,
    theme: &Theme,
) -> impl IntoElement + use<T> {
    h_flex()
        .items_center()
        .justify_between()
        .gap_4()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(label),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(hint),
                ),
        )
        .child(div().w(px(260.)).flex_shrink_0().child(control))
}

fn render_modal_field_row<T: IntoElement>(
    label: &'static str,
    control: T,
    theme: &Theme,
) -> impl IntoElement + use<T> {
    h_flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(164.))
                .flex_shrink_0()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(control))
}

fn build_code_or_issues_options(connectors: &ConnectorsConfig) -> Vec<SourceRoutingOption> {
    let azure_ok = connectors.is_azure_configured();
    let github_ok = connectors.is_github_configured();
    vec![
        SourceRoutingOption {
            value: String::new(),
            title: "None".into(),
            is_disabled: false,
        },
        SourceRoutingOption {
            value: "azureDevOps".to_string(),
            title: if azure_ok {
                "Azure DevOps".into()
            } else {
                "Azure DevOps (Unconfigured)".into()
            },
            is_disabled: !azure_ok,
        },
        SourceRoutingOption {
            value: "github".to_string(),
            title: if github_ok {
                "GitHub".into()
            } else {
                "GitHub (Unconfigured)".into()
            },
            is_disabled: !github_ok,
        },
    ]
}

fn build_docs_options(connectors: &ConnectorsConfig) -> Vec<SourceRoutingOption> {
    let atlassian_ok = connectors.is_atlassian_configured();
    vec![
        SourceRoutingOption {
            value: String::new(),
            title: "None".into(),
            is_disabled: false,
        },
        SourceRoutingOption {
            value: "atlassian".to_string(),
            title: if atlassian_ok {
                "Atlassian Confluence".into()
            } else {
                "Atlassian Confluence (Unconfigured)".into()
            },
            is_disabled: !atlassian_ok,
        },
    ]
}

fn selected_option_row(options: &[SourceRoutingOption], value: &str) -> usize {
    options.iter().position(|o| o.value == value).unwrap_or(0)
}

fn normalize_selected_value(options: &[SourceRoutingOption], value: &str) -> String {
    if options.iter().any(|o| o.value == value) {
        value.to_string()
    } else {
        String::new()
    }
}

fn build_work_item_type_options(types: &[String], current: &str) -> Vec<WorkItemTypeOption> {
    let clean_current = current.trim();
    let mut options = Vec::with_capacity(types.len() + 1);
    if !clean_current.is_empty() && !types.iter().any(|t| t == clean_current) {
        options.push(WorkItemTypeOption {
            value: clean_current.to_string(),
            title: format!("{clean_current} (custom)").into(),
        });
    }
    for t in types {
        options.push(WorkItemTypeOption {
            value: t.clone(),
            title: t.clone().into(),
        });
    }
    options
}

fn create_type_select(
    types: &[String],
    current: &str,
    window: &mut Window,
    cx: &mut Context<AzureConnectorModal>,
) -> Entity<SelectState<SearchableVec<WorkItemTypeOption>>> {
    let options = build_work_item_type_options(types, current);
    let selected_row = options.iter().position(|o| o.value == current.trim());
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(options),
            selected_row.map(|row| IndexPath::default().row(row)),
            window,
            cx,
        )
        .searchable(true)
    })
}

fn update_type_select(
    select: &Entity<SelectState<SearchableVec<WorkItemTypeOption>>>,
    types: &[String],
    current: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let options = build_work_item_type_options(types, current);
    let target = current.trim().to_string();
    select.update(cx, |state, cx| {
        state.set_items(SearchableVec::new(options), window, cx);
        if !target.is_empty() {
            state.set_selected_value(&target, window, cx);
        }
    });
}

fn non_empty_or(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn clear_sources_for(sources: &mut SourcesConfig, connector_id: &str) {
    if sources.issues == connector_id {
        sources.issues.clear();
    }
    if sources.code == connector_id {
        sources.code.clear();
    }
    if sources.docs == connector_id {
        sources.docs.clear();
    }
}
