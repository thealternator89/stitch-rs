use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Entity, FontWeight, ParentElement as _, Styled as _, Window, div, hsla,
    prelude::FluentBuilder as _, px,
};

use crate::app_state::{AppState, refresh_copilot_status};
use crate::features::settings::SettingsTab;
use crate::types::CopilotAuthStatus;

/// Displays a dismissible modal when startup detects unauthenticated GitHub Copilot.
pub fn open_copilot_auth_modal(app_state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _window, cx| {
        let state = app_state.read(cx);
        let auth_status = state.copilot_auth.clone();
        let is_checking = matches!(auth_status, CopilotAuthStatus::Checking);
        let theme = cx.theme();
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let mono_font = theme.mono_font_family.clone();

        let status_detail = match &auth_status {
            CopilotAuthStatus::Disconnected { info, error } => error
                .clone()
                .or_else(|| info.as_ref().and_then(|i| i.status_message.clone())),
            _ => None,
        };

        let app_state_for_close = app_state.clone();
        let app_state_for_settings = app_state.clone();
        let app_state_for_cancel = app_state.clone();
        let app_state_for_recheck = app_state.clone();

        dialog
            .w(px(520.))
            .overlay_closable(true)
            .close_button(true)
            .on_close(move |_, _, cx| {
                app_state_for_close.update(cx, |state, _| {
                    state.auth_modal_open = false;
                });
            })
            .title(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(LucideIcon::TriangleAlert)
                            .with_size(Size::Medium)
                            .text_color(amber),
                    )
                    .child("GitHub Copilot Authentication Required"),
            )
            .child(
                v_flex()
                    .gap_3()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("Stitch requires an authenticated GitHub Copilot session to inspect codebases and generate AI outputs."),
                    )
                    .when_some(status_detail, |this, detail| {
                        this.child(
                            div()
                                .p_2p5()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.secondary)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                    })
                    .child(
                        v_flex()
                            .p_3()
                            .gap_2()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Option 1: Sign in via CLI"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Run one of the following commands in your terminal, then click Check Again:"),
                            )
                            .child(
                                v_flex()
                                    .p_2()
                                    .gap_1()
                                    .rounded(theme.radius)
                                    .bg(theme.background)
                                    .border_1()
                                    .border_color(theme.border)
                                    .font_family(mono_font.clone())
                                    .text_xs()
                                    .child("copilot auth signin")
                                    .child("gh auth login"),
                            )
                            .child(
                                div()
                                    .pt_1()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Option 2: Configure a Copilot Token"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Open Settings > GitHub Copilot to save a personal token securely in your OS Keychain."),
                            ),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        Button::new("auth-modal-open-settings")
                            .ghost()
                            .icon(Icon::new(LucideIcon::Settings).small())
                            .label("Open Settings")
                            .on_click(move |_, window, cx| {
                                app_state_for_settings.update(cx, |state, cx| {
                                    state.auth_modal_open = false;
                                    state.navigate_to_settings_tab(SettingsTab::Copilot, cx);
                                    cx.notify();
                                });
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("auth-modal-close")
                                    .ghost()
                                    .label("Close")
                                    .on_click(move |_, window, cx| {
                                        app_state_for_cancel.update(cx, |state, cx| {
                                            state.auth_modal_open = false;
                                            cx.notify();
                                        });
                                        window.close_dialog(cx);
                                    }),
                            )
                            .child(
                                Button::new("auth-modal-recheck")
                                    .primary()
                                    .disabled(is_checking)
                                    .loading(is_checking)
                                    .icon(Icon::new(LucideIcon::RefreshCw).small())
                                    .label(if is_checking {
                                        "Checking..."
                                    } else {
                                        "Check Again"
                                    })
                                    .on_click(move |_, window, cx| {
                                        refresh_copilot_status(
                                            app_state_for_recheck.clone(),
                                            true,
                                            false,
                                            window,
                                            cx,
                                        );
                                    }),
                            ),
                    ),
            )
    });
}
