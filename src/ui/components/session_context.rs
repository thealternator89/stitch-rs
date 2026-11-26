use std::path::Path;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex};
use gpui_kit::{
    App, IntoElement, ParentElement as _, Styled as _, Window, div, hsla,
    prelude::FluentBuilder as _,
};

/// Returns the last path segment (directory name) for compact badge display,
/// falling back to the trimmed path for root or non-standard paths.
pub fn display_repo_name(raw_path: &str) -> String {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let trimmed_slashes = trimmed.trim_end_matches(['/', '\\']);
    let candidate = if trimmed_slashes.is_empty() {
        trimmed
    } else {
        trimmed_slashes
    };
    Path::new(candidate)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string())
}

/// Collapses newlines and repeated whitespace into single spaces and truncates
/// safely by character count with a trailing ellipsis.
pub fn summarize_single_line(raw: &str, max_chars: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if max_chars == 0 {
        return String::new();
    }
    let char_count = collapsed.chars().count();
    if char_count <= max_chars {
        return collapsed;
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut out: String = collapsed.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// Renders a compact metadata badge matching PR Reviewer's collapsed context header style.
pub fn render_context_pill(
    icon: LucideIcon,
    text: impl Into<String>,
    bordered: bool,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .items_center()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded(theme.radius)
        .when(bordered, |el| {
            el.border_1()
                .border_color(theme.border)
                .bg(theme.background)
        })
        .when(!bordered, |el| el.bg(theme.secondary))
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(Icon::new(icon).xsmall())
        .child(text.into())
}

/// Renders a dismissible inline error banner for active full-width workspaces
/// where the left setup panel is hidden.
pub fn render_active_error_banner(
    dismiss_id: &'static str,
    message: String,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let red = hsla(0.0, 0.78, 0.55, 1.0);

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
        .child(div().flex_1().min_w_0().child(message))
        .child(
            Button::new(dismiss_id)
                .ghost()
                .xsmall()
                .icon(Icon::new(LucideIcon::X).xsmall())
                .on_click(move |_, window, cx| {
                    on_dismiss(window, cx);
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_repo_name_extracts_basename_and_handles_edge_cases() {
        assert_eq!(display_repo_name("/Users/dev/code/stitch-rs"), "stitch-rs");
        assert_eq!(display_repo_name("/Users/dev/code/stitch-rs/"), "stitch-rs");
        assert_eq!(display_repo_name("  /Users/dev/my-app///  "), "my-app");
        assert_eq!(display_repo_name("/"), "/");
        assert_eq!(display_repo_name("   "), "");
    }

    #[test]
    fn summarize_single_line_collapses_whitespace_and_truncates_utf8_safely() {
        assert_eq!(
            summarize_single_line("  First line\n\n  second   line\tthird  ", 50),
            "First line second line third"
        );
        assert_eq!(summarize_single_line("exact", 5), "exact");
        assert_eq!(summarize_single_line("abcdef", 5), "abcd…");
        assert_eq!(summarize_single_line("héllo wörld", 6), "héllo…");
        assert_eq!(summarize_single_line("hello", 1), "…");
        assert_eq!(summarize_single_line("hello", 0), "");
    }
}
