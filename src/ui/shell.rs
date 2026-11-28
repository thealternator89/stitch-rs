use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarItem as _, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{
    ActiveTheme as _, Collapsible as _, Icon, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, hsla, img, prelude::FluentBuilder as _, px, relative,
};

use crate::app_state::{ActiveView, AppState, apply_theme, refresh_copilot_status};
use crate::types::{CopilotAuthStatus, ThemeMode};
use crate::ui::components::render_usage_stats_toast;

const REPORT_ISSUE_URL: &str = "https://github.com/thealternator89/stitch-rs/issues";
const SIDEBAR_WIDTH: f32 = 224.0;
const USAGE_TOAST_AUTO_DISMISS: Duration = Duration::from_secs(8);

/// Configures a titlebar button so the `TitleBar` drag container does not intercept its clicks on Windows.
///
/// `.occlude()` (`HitboxBehavior::BlockMouse`) prevents the ancestor `WindowControlArea::Drag`
/// hitbox from matching `WM_NCHITTEST` over the button, keeping it in `HTCLIENT` for normal
/// clicks/hover and preventing `#title-bar` from starting a window drag. `.on_mouse_down` with
/// `cx.stop_propagation()` also guards zero-hover clicks where `WM_NCHITTEST` ran before
/// `mouse_hit_test` updated so `WM_NCLBUTTONDOWN` is consumed instead of falling through to
/// `DefWindowProcW`.
fn title_bar_button(button: Button) -> Button {
    button
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
}

pub struct WorkspaceShell {
    app_state: Entity<AppState>,
    sidebar_collapsed: bool,
    toast_seen_seq: u64,
    toast_hovered: bool,
    toast_timer_gen: u64,
    _subscriptions: Vec<Subscription>,
}

impl WorkspaceShell {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state_sub = cx.observe(&app_state, |_, _, cx| cx.notify());

        let app_state_for_appearance = app_state.clone();
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            if app_state_for_appearance.read(cx).theme_mode() == ThemeMode::Auto {
                apply_theme(ThemeMode::Auto, Some(window), cx);
            }
        });

        refresh_copilot_status(app_state.clone(), false, true, window, cx);

        Self {
            app_state,
            sidebar_collapsed: false,
            toast_seen_seq: 0,
            toast_hovered: false,
            toast_timer_gen: 0,
            _subscriptions: vec![state_sub, appearance_sub],
        }
    }

    /// (Re)starts the auto-dismiss countdown for the usage toast.
    fn start_toast_timer(&mut self, cx: &mut Context<Self>) {
        self.toast_timer_gen += 1;
        let generation = self.toast_timer_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(USAGE_TOAST_AUTO_DISMISS).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast_timer_gen == generation && !this.toast_hovered {
                    this.app_state.update(cx, |s, cx| {
                        s.dismiss_usage_toast();
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let theme_mode = self.app_state.read(cx).theme_mode();
        let theme_icon = match theme_mode {
            ThemeMode::Auto => LucideIcon::Monitor,
            ThemeMode::Light => LucideIcon::Sun,
            ThemeMode::Dark => LucideIcon::Moon,
        };
        let sidebar_toggle_icon = if self.sidebar_collapsed {
            LucideIcon::PanelLeftOpen
        } else {
            LucideIcon::PanelLeftClose
        };

        let app_state_for_theme = self.app_state.clone();

        TitleBar::new()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(title_bar_button(
                        Button::new("toggle-sidebar")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(sidebar_toggle_icon).small())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_collapsed = !this.sidebar_collapsed;
                                cx.notify();
                            })),
                    ))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                img(crate::branding::icon_image())
                                    .size(px(20.))
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child("Stitch"),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .pr_2()
                    .child(title_bar_button(
                        Button::new("theme-mode-toggle")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(theme_icon).xsmall())
                            .label(theme_mode.label())
                            .on_click(move |_, window, cx| {
                                app_state_for_theme.update(cx, |state, cx| {
                                    state.cycle_theme_mode(window, cx);
                                    cx.notify();
                                });
                            }),
                    ))
                    .child(title_bar_button(
                        Button::new("report-issue")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(LucideIcon::ExternalLink).xsmall())
                            .label("Report an Issue")
                            .on_click(|_, _, cx| {
                                cx.open_url(REPORT_ISSUE_URL);
                            }),
                    )),
            )
    }

    fn build_nav_item(&self, view: ActiveView, cx: &App) -> SidebarMenuItem {
        let state = self.app_state.read(cx);
        let is_active = state.active_view == view;
        let is_running = state.is_task_running(view);
        let app_state = self.app_state.clone();

        let mut item = SidebarMenuItem::new(view.label())
            .icon(Icon::new(view.icon()).small())
            .active(is_active)
            .on_click(move |_, _, cx| {
                app_state.update(cx, |state, cx| {
                    state.set_active_view(view);
                    cx.notify();
                });
            });

        if is_running {
            item = item.suffix(|_, cx| Spinner::new().xsmall().color(cx.theme().primary));
        }

        item
    }

    fn render_activity_rail(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.sidebar_collapsed;
        let sidebar_border = cx.theme().sidebar_border;

        let groups: Vec<_> = ActiveView::LIFECYCLE_GROUPS
            .iter()
            .map(|(group, views)| {
                let menu = SidebarMenu::new().children(
                    views
                        .iter()
                        .copied()
                        .map(|view| self.build_nav_item(view, cx)),
                );
                SidebarGroup::new(group.label()).child(menu)
            })
            .collect();

        let utility_items: Vec<_> = ActiveView::UTILITY_VIEWS
            .iter()
            .copied()
            .map(|view| self.build_nav_item(view, cx))
            .collect();

        let utility_menu = SidebarMenu::new()
            .children(utility_items)
            .collapsed(collapsed)
            .render("utility-nav", window, cx)
            .into_any_element();

        Sidebar::new("activity-rail")
            .w(px(SIDEBAR_WIDTH))
            .collapsible(SidebarCollapsible::Icon)
            .collapsed(collapsed)
            .children(groups)
            .footer(
                div()
                    .w_full()
                    .pt_2()
                    .border_t_1()
                    .border_color(sidebar_border)
                    .child(utility_menu),
            )
    }

    fn render_active_view(&self, cx: &App) -> AnyElement {
        let state = self.app_state.read(cx);
        match state.active_view {
            ActiveView::TShirtEstimator => state.tshirt_estimator.clone().into_any_element(),
            ActiveView::CodeExplainer => state.code_explainer.clone().into_any_element(),
            ActiveView::SolutionDesigner => state.solution_designer.clone().into_any_element(),
            ActiveView::DiagramBuilder => state.diagram_builder.clone().into_any_element(),
            ActiveView::StoryWriter => state.story_writer.clone().into_any_element(),
            ActiveView::TestCaseWriter => state.test_case_writer.clone().into_any_element(),
            ActiveView::StoryElaborator => state.story_elaborator.clone().into_any_element(),
            ActiveView::PrReviewer => state.pr_reviewer.clone().into_any_element(),
            ActiveView::UsageHistory => state.usage_history.clone().into_any_element(),
            ActiveView::Settings => state.settings_view.clone().into_any_element(),
        }
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);
        let auth_status = state.copilot_auth.clone();
        let active_task = state.active_task_summary();
        let app_state_for_auth = self.app_state.clone();

        let (dot_color, auth_label, is_checking) = match &auth_status {
            CopilotAuthStatus::Checking => (None, "Checking Auth...".to_string(), true),
            CopilotAuthStatus::Connected(info) => (
                Some(hsla(152.0 / 360.0, 0.76, 0.40, 1.0)),
                match info.login.as_deref() {
                    Some(login) => format!("Copilot Connected (@{login})"),
                    None => "Copilot Connected (Authenticated)".to_string(),
                },
                false,
            ),
            CopilotAuthStatus::Disconnected { .. } => (
                Some(hsla(38.0 / 360.0, 0.92, 0.50, 1.0)),
                "Copilot Disconnected".to_string(),
                false,
            ),
        };

        let auth_slot = h_flex()
            .id("copilot-auth-status")
            .items_center()
            .gap_1p5()
            .px_1p5()
            .py_0p5()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().secondary_hover))
            .on_click(move |_, window, cx| {
                if matches!(
                    app_state_for_auth.read(cx).copilot_auth,
                    CopilotAuthStatus::Checking
                ) {
                    return;
                }
                refresh_copilot_status(app_state_for_auth.clone(), true, false, window, cx);
            })
            .when(is_checking, |this| this.child(Spinner::new().xsmall()))
            .when_some(dot_color, |this, color| {
                this.child(div().size(px(8.)).rounded_full().bg(color))
            })
            .child(div().text_xs().child(auth_label));

        let version_label = format!("Version {}", env!("CARGO_PKG_VERSION"));
        let version_updated_from = state.version_updated_from.clone();
        let app_state_for_version = self.app_state.clone();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let green_bg = hsla(152.0 / 360.0, 0.76, 0.40, 0.14);

        let right_slot = h_flex()
            .items_center()
            .gap_2()
            .when_some(version_updated_from, |row, prev_ver| {
                row.child(
                    h_flex()
                        .id("version-update-notice")
                        .items_center()
                        .gap_1()
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(green_bg)
                        .text_xs()
                        .text_color(green)
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            app_state_for_version.update(cx, |state, cx| {
                                state.dismiss_version_notice();
                                cx.notify();
                            });
                        })
                        .child(format!("Updated from v{prev_ver}"))
                        .child(Icon::new(LucideIcon::X).xsmall().text_color(green)),
                )
            })
            .child(div().text_xs().child(version_label));

        StatusBar::new()
            .left(auth_slot)
            .when_some(active_task, |this, summary| {
                this.child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .child(Spinner::new().xsmall())
                        .child(div().text_xs().child(summary)),
                )
            })
            .right(right_slot)
    }
}

impl Render for WorkspaceShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (window_progress, toast_data, toast_seq) = {
            let state = self.app_state.read(cx);
            (
                state.active_window_progress(),
                state.active_usage_toast.clone(),
                state.usage_toast_seq,
            )
        };
        if toast_data.is_some() && toast_seq != self.toast_seen_seq {
            self.toast_seen_seq = toast_seq;
            self.toast_hovered = false;
            self.start_toast_timer(cx);
        }
        let app_state_for_toast = self.app_state.clone();

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_title_bar(cx))
            .when_some(window_progress, |shell, progress| {
                shell.child(
                    div()
                        .w_full()
                        .h(px(2.5))
                        .bg(theme.secondary)
                        .overflow_hidden()
                        .child(
                            div()
                                .w(relative(progress.clamp(0.05, 1.0)))
                                .h_full()
                                .bg(theme.primary),
                        ),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_activity_rail(window, cx))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(self.render_active_view(cx))
                            .when_some(toast_data, |pane, toast| {
                                let state_for_dismiss = app_state_for_toast.clone();
                                pane.child(
                                    div()
                                        .id("usage-toast-hover-area")
                                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                            this.toast_hovered = *hovered;
                                            if !*hovered {
                                                this.start_toast_timer(cx);
                                            }
                                        }))
                                        .absolute()
                                        .bottom_4()
                                        .right_4()
                                        .child(render_usage_stats_toast(
                                            &toast,
                                            move |_, cx| {
                                                state_for_dismiss.update(cx, |s, cx| {
                                                    s.dismiss_usage_toast();
                                                    cx.notify();
                                                });
                                            },
                                            cx,
                                        )),
                                )
                            }),
                    ),
            )
            .child(self.render_footer(cx))
    }
}
