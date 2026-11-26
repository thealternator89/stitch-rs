use std::rc::Rc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{App, ParentElement as _, Styled as _, Window, div, hsla, px};

/// Describes whether a tool is clean, has only form edits, or has active work/results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetState {
    /// Tool is already in its initial default state.
    Clean,
    /// Form inputs, overrides, or transient errors/notices are modified, but no results or active tasks would be lost.
    DirtyInputsOnly,
    /// Active task, session/checkout, or generated results exist and require confirmation before clearing.
    NeedsConfirmation { is_running: bool },
}

impl ResetState {
    pub fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }

    #[allow(dead_code)]
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::NeedsConfirmation { .. })
    }
}

/// Normalizes a model identifier from settings, defaulting empty values to `"auto"`.
pub fn normalize_default_model(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "auto".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Builds the consistent top-right header Reset button shared across all tools.
pub fn render_tool_reset_button(
    id: &'static str,
    reset_state: ResetState,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .disabled(reset_state.is_clean())
        .icon(Icon::new(LucideIcon::RotateCcw).small())
        .label("Reset")
        .on_click(move |_, window, cx| {
            on_click(window, cx);
        })
}

type ResetConfirmCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// Opens a confirmation dialog before resetting a tool that has active work or generated results.
pub fn open_reset_confirm_modal(
    tool_name: &'static str,
    is_running: bool,
    detail: impl Into<String>,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_confirm: ResetConfirmCallback = Rc::new(on_confirm);
    let detail_text = detail.into();

    window.open_dialog(cx, move |dialog, _window, cx| {
        let theme = cx.theme();
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let confirm_cb = on_confirm.clone();
        let title_text = if is_running {
            format!("Cancel and Reset {tool_name}?")
        } else {
            format!("Reset {tool_name}?")
        };
        let confirm_label = if is_running {
            "Cancel & Reset"
        } else {
            "Reset Tool"
        };

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
                    .child(title_text),
            )
            .child(
                v_flex().gap_2().child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(detail_text.clone()),
                ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("reset-confirm-cancel")
                            .ghost()
                            .label("Keep Working")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("reset-confirm-proceed")
                            .danger()
                            .icon(Icon::new(LucideIcon::RotateCcw).small())
                            .label(confirm_label)
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                confirm_cb(window, cx);
                            }),
                    ),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_default_model_handles_empty_and_explicit_values() {
        assert_eq!(normalize_default_model(""), "auto");
        assert_eq!(normalize_default_model("   "), "auto");
        assert_eq!(normalize_default_model("auto"), "auto");
        assert_eq!(normalize_default_model(" gpt-4.1 "), "gpt-4.1");
    }

    #[test]
    fn reset_state_helpers_match_variants() {
        assert!(ResetState::Clean.is_clean());
        assert!(!ResetState::Clean.needs_confirmation());

        assert!(!ResetState::DirtyInputsOnly.is_clean());
        assert!(!ResetState::DirtyInputsOnly.needs_confirmation());

        let running = ResetState::NeedsConfirmation { is_running: true };
        assert!(!running.is_clean());
        assert!(running.needs_confirmation());

        let idle_results = ResetState::NeedsConfirmation { is_running: false };
        assert!(!idle_results.is_clean());
        assert!(idle_results.needs_confirmation());
    }
}
