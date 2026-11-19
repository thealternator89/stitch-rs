use std::rc::Rc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex};
use gpui_kit::{App, ParentElement as _, Styled as _, Window, div, hsla, px};

/// Opens a modal notifying the user that a Copilot streaming request hit its inactivity timeout.
pub fn open_timeout_modal(
    tool_name: &'static str,
    timeout_secs: u64,
    on_retry: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_retry: Rc<dyn Fn(&mut Window, &mut App)> = Rc::new(on_retry);

    window.open_dialog(cx, move |dialog, _window, cx| {
        let theme = cx.theme();
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let mono_font = theme.mono_font_family.clone();
        let retry_cb = on_retry.clone();

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
                    .child("Copilot Request Timed Out"),
            )
            .child(
                v_flex()
                    .gap_3()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{tool_name} did not receive output from Copilot for {timeout_secs} seconds while no tools were actively running, so the session was stopped."
                            )),
                    )
                    .child(
                        div()
                            .p_3()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(
                                h_flex()
                                    .flex_wrap()
                                    .gap_1()
                                    .child("You can retry now, or adjust the inactivity timeout via the")
                                    .child(
                                        div()
                                            .px_1()
                                            .rounded(px(4.))
                                            .bg(theme.background)
                                            .border_1()
                                            .border_color(theme.border)
                                            .font_family(mono_font)
                                            .text_color(theme.foreground)
                                            .child("STITCH_COPILOT_TIMEOUT"),
                                    )
                                    .child("environment variable (in milliseconds)."),
                            ),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("timeout-modal-close")
                            .ghost()
                            .label("Close")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("timeout-modal-retry")
                            .primary()
                            .icon(Icon::new(LucideIcon::RefreshCw).small())
                            .label("Retry")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                retry_cb(window, cx);
                            }),
                    ),
            )
    });
}
