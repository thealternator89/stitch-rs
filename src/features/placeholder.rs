use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{App, IntoElement, ParentElement as _, Styled as _, div, px};

use crate::app_state::ActiveView;

/// Renders a consistent, themed placeholder layout for tools not yet implemented.
pub fn render_tool_placeholder(
    view: ActiveView,
    phase_label: &'static str,
    left_pane_hint: &'static str,
    right_pane_hint: &'static str,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();

    v_flex()
        .size_full()
        .p_6()
        .gap_5()
        .bg(theme.background)
        .text_color(theme.foreground)
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .pb_4()
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
                                .size(px(40.))
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .text_color(theme.accent_foreground)
                                .child(Icon::new(view.icon()).large()),
                        )
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(view.label()),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(view.subtitle()),
                                ),
                        ),
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
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(phase_label),
                ),
        )
        .child(
            h_flex()
                .flex_1()
                .gap_4()
                .min_h_0()
                .child(
                    v_flex()
                        .w(px(300.))
                        .h_full()
                        .p_4()
                        .gap_2()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.sidebar)
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child("CONFIGURATION & CONTEXT"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(left_pane_hint),
                        ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .h_full()
                        .p_6()
                        .gap_2()
                        .items_center()
                        .justify_center()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(
                            Icon::new(view.icon())
                                .large()
                                .text_color(theme.muted_foreground.opacity(0.6)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .child(format!("{} Workspace", view.label())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(right_pane_hint),
                        ),
                ),
        )
}
