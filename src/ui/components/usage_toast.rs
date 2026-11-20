use std::rc::Rc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, Window,
    div, hsla, prelude::FluentBuilder as _, px,
};

use crate::types::{CopilotUsage, PhaseUsage};

/// Payload displayed by [`render_usage_stats_toast`] and [`open_usage_details_modal`].
#[derive(Debug, Clone)]
pub struct UsageToastData {
    pub tool_name: String,
    pub context_reference: Option<String>,
    pub default_label: String,
    pub default_model: String,
    pub default_multiplier: Option<f64>,
    pub usage: CopilotUsage,
}

impl UsageToastData {
    pub fn total_tokens(&self) -> u64 {
        self.usage.input_tokens + self.usage.output_tokens
    }

    pub fn cache_hit_percent(&self) -> u64 {
        cache_percentage(self.usage.cache_read_tokens, self.usage.input_tokens)
    }

    pub fn rows(&self) -> Vec<PhaseUsage> {
        if let Some(phases) = self.usage.phases.as_ref().filter(|p| !p.is_empty()) {
            phases.clone()
        } else {
            let model = self
                .usage
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(&self.default_model);
            let model = if model.trim().is_empty() {
                "auto".to_string()
            } else {
                model.trim().to_string()
            };
            vec![PhaseUsage {
                phase_title: self.default_label.clone(),
                model,
                input_tokens: self.usage.input_tokens,
                output_tokens: self.usage.output_tokens,
                cache_read_tokens: self.usage.cache_read_tokens,
                cost: self.usage.cost,
                multiplier: self.default_multiplier.unwrap_or(1.0),
            }]
        }
    }
}

/// Formats an integer with comma thousands separators (`1234567` -> `"1,234,567"`).
pub fn format_token_count(value: u64) -> String {
    let raw = value.to_string();
    let len = raw.len();
    let mut out = String::with_capacity(len + len / 3);
    for (idx, ch) in raw.chars().enumerate() {
        if idx > 0 && (len - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Computes `round((cached_tokens / input_tokens) * 100)`. Returns `0` when `input_tokens == 0`.
pub fn cache_percentage(cached_tokens: u64, input_tokens: u64) -> u64 {
    if input_tokens == 0 {
        return 0;
    }
    let pct = (cached_tokens as f64 / input_tokens as f64) * 100.0;
    pct.round().clamp(0.0, 100.0) as u64
}

/// Formats a billing multiplier cleanly (e.g. `1.0` -> `"1x"`, `1.5` -> `"1.5x"`, `0.33` -> `"0.33x"`).
pub fn format_multiplier(multiplier: Option<f64>) -> String {
    let Some(m) = multiplier else {
        return "-".to_string();
    };
    if (m - m.round()).abs() < 1e-6 {
        format!("{}x", m.round() as i64)
    } else {
        let formatted = format!("{m:.2}");
        let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
        format!("{trimmed}x")
    }
}

/// Renders the floating bottom-right **`UsageStatsToast`** shown after a tool generation run finishes.
pub fn render_usage_stats_toast(
    data: &UsageToastData,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
    let green_bg = hsla(152.0 / 360.0, 0.76, 0.40, 0.14);
    let cyan = hsla(192.0 / 360.0, 0.85, 0.45, 1.0);

    let total_tokens = format_token_count(data.total_tokens());
    let input_tokens = format_token_count(data.usage.input_tokens);
    let output_tokens = format_token_count(data.usage.output_tokens);
    let cache_pct = data.cache_hit_percent();

    let header_title = match data.context_reference.as_deref().filter(|s| !s.is_empty()) {
        Some(ctx_ref) => format!("{} • {ctx_ref}", data.tool_name),
        None => data.tool_name.clone(),
    };

    let data_for_modal = data.clone();
    let on_dismiss = Rc::new(on_dismiss);
    let dismiss_for_btn = on_dismiss.clone();

    v_flex()
        .id("global-usage-stats-toast")
        .w(px(360.))
        .p_3()
        .gap_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .shadow_lg()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .min_w_0()
                        .child(
                            Icon::new(LucideIcon::ChartBar)
                                .xsmall()
                                .text_color(theme.primary),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .truncate()
                                .child(header_title),
                        ),
                )
                .child(
                    Button::new("dismiss-usage-toast")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(LucideIcon::X).xsmall())
                        .on_click(move |_, window, cx| {
                            dismiss_for_btn(window, cx);
                        }),
                ),
        )
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .items_baseline()
                                .gap_1p5()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(theme.foreground)
                                        .child(format!("{total_tokens} tokens")),
                                )
                                .child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(px(4.))
                                        .bg(green_bg)
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(green)
                                        .child(format!("{cache_pct}% cached")),
                                ),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .child(div().size(px(6.)).rounded_full().bg(green))
                                        .child(format!("In: {input_tokens}")),
                                )
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .child(div().size(px(6.)).rounded_full().bg(cyan))
                                        .child(format!("Out: {output_tokens}")),
                                ),
                        ),
                )
                .child(
                    Button::new("view-usage-details-btn")
                        .outline()
                        .xsmall()
                        .icon(Icon::new(LucideIcon::Eye).xsmall())
                        .label("View Details")
                        .on_click(move |_, window, cx| {
                            open_usage_details_modal(data_for_modal.clone(), window, cx);
                        }),
                ),
        )
}

/// Opens the **`UsageDetailsModal`** displaying per-phase/turn input, output, cached tokens,
/// cache hit percentages, and model billing multipliers.
pub fn open_usage_details_modal(data: UsageToastData, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _window, cx| {
        let theme = cx.theme();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let green_bg = hsla(152.0 / 360.0, 0.76, 0.40, 0.14);
        let cyan = hsla(192.0 / 360.0, 0.85, 0.45, 1.0);
        let mono_font = theme.mono_font_family.clone();

        let rows = data.rows();
        let show_total_row = rows.len() > 1;
        let total_in = data.usage.input_tokens;
        let total_out = data.usage.output_tokens;
        let total_cached = data.usage.cache_read_tokens;
        let total_tokens = data.total_tokens();
        let total_cache_pct = data.cache_hit_percent();

        let subtitle = match data.context_reference.as_deref().filter(|s| !s.is_empty()) {
            Some(ctx_ref) => format!("{} • {ctx_ref}", data.tool_name),
            None => data.tool_name.clone(),
        };

        dialog
            .w(px(740.))
            .overlay_closable(true)
            .close_button(true)
            .title(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(LucideIcon::ChartBar)
                            .with_size(Size::Medium)
                            .text_color(theme.primary),
                    )
                    .child("LLM Token Usage Breakdown"),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_3()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(subtitle),
                    )
                    // 4 summary KPI tiles
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(render_modal_kpi_tile(
                                "Total Tokens",
                                format_token_count(total_tokens),
                                None,
                                cx,
                            ))
                            .child(render_modal_kpi_tile(
                                "Input Tokens",
                                format_token_count(total_in),
                                Some(green),
                                cx,
                            ))
                            .child(render_modal_kpi_tile(
                                "Output Tokens",
                                format_token_count(total_out),
                                Some(cyan),
                                cx,
                            ))
                            .child(render_modal_kpi_tile(
                                "Cache Savings",
                                format!(
                                    "{} ({total_cache_pct}%)",
                                    format_token_count(total_cached)
                                ),
                                Some(green),
                                cx,
                            )),
                    )
                    // Table container
                    .child(
                        v_flex()
                            .w_full()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .overflow_hidden()
                            // Header row
                            .child(
                                h_flex()
                                    .w_full()
                                    .px_3()
                                    .py_2()
                                    .bg(theme.sidebar)
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(div().flex_1().min_w_0().child("Phase / Label"))
                                    .child(div().w(px(140.)).child("Model"))
                                    .child(
                                        div()
                                            .w(px(64.))
                                            .flex()
                                            .justify_end()
                                            .child("Multiplier"),
                                    )
                                    .child(
                                        div()
                                            .w(px(86.))
                                            .flex()
                                            .justify_end()
                                            .child("Input"),
                                    )
                                    .child(
                                        div()
                                            .w(px(86.))
                                            .flex()
                                            .justify_end()
                                            .child("Output"),
                                    )
                                    .child(
                                        div()
                                            .w(px(86.))
                                            .flex()
                                            .justify_end()
                                            .child("Cached"),
                                    )
                                    .child(
                                        div()
                                            .w(px(72.))
                                            .flex()
                                            .justify_end()
                                            .child("% Cached"),
                                    ),
                            )
                            // Data rows
                            .child(
                                v_flex()
                                    .id("usage-details-modal-rows")
                                    .max_h(px(320.))
                                    .overflow_y_scrollbar()
                                    .children(rows.iter().enumerate().map(|(idx, row)| {
                                        let row_pct = cache_percentage(
                                            row.cache_read_tokens,
                                            row.input_tokens,
                                        );
                                        let mult_label = format_multiplier(Some(row.multiplier));
                                        let row_mono = mono_font.clone();
                                        h_flex()
                                            .w_full()
                                            .px_3()
                                            .py_2()
                                            .items_center()
                                            .when(idx > 0, |el| {
                                                el.border_t_1().border_color(theme.border)
                                            })
                                            .text_xs()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .pr_2()
                                                    .truncate()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(theme.foreground)
                                                    .child(row.phase_title.clone()),
                                            )
                                            .child(
                                                div()
                                                    .w(px(140.))
                                                    .pr_2()
                                                    .truncate()
                                                    .font_family(row_mono.clone())
                                                    .text_color(theme.muted_foreground)
                                                    .child(row.model.clone()),
                                            )
                                            .child(
                                                div()
                                                    .w(px(64.))
                                                    .flex()
                                                    .justify_end()
                                                    .font_family(row_mono.clone())
                                                    .text_color(theme.muted_foreground)
                                                    .child(mult_label),
                                            )
                                            .child(
                                                div()
                                                    .w(px(86.))
                                                    .flex()
                                                    .justify_end()
                                                    .font_family(row_mono.clone())
                                                    .text_color(theme.foreground)
                                                    .child(format_token_count(row.input_tokens)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(86.))
                                                    .flex()
                                                    .justify_end()
                                                    .font_family(row_mono.clone())
                                                    .text_color(theme.foreground)
                                                    .child(format_token_count(row.output_tokens)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(86.))
                                                    .flex()
                                                    .justify_end()
                                                    .font_family(row_mono)
                                                    .text_color(theme.muted_foreground)
                                                    .child(format_token_count(
                                                        row.cache_read_tokens,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .w(px(72.))
                                                    .flex()
                                                    .justify_end()
                                                    .child(
                                                        div()
                                                            .px_1p5()
                                                            .py_0p5()
                                                            .rounded(px(4.))
                                                            .bg(green_bg)
                                                            .text_color(green)
                                                            .font_weight(FontWeight::MEDIUM)
                                                            .child(format!("{row_pct}%")),
                                                    ),
                                            )
                                    })),
                            )
                            // Bold Total footer row when rows.len() > 1
                            .when(show_total_row, |table| {
                                let total_mono = mono_font.clone();
                                table.child(
                                    h_flex()
                                        .w_full()
                                        .px_3()
                                        .py_2()
                                        .items_center()
                                        .bg(theme.sidebar)
                                        .border_t_1()
                                        .border_color(theme.border)
                                        .text_xs()
                                        .font_weight(FontWeight::BOLD)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_color(theme.foreground)
                                                .child(format!("Total ({} phases)", rows.len())),
                                        )
                                        .child(div().w(px(140.)).child(""))
                                        .child(div().w(px(64.)).child(""))
                                        .child(
                                            div()
                                                .w(px(86.))
                                                .flex()
                                                .justify_end()
                                                .font_family(total_mono.clone())
                                                .text_color(theme.foreground)
                                                .child(format_token_count(total_in)),
                                        )
                                        .child(
                                            div()
                                                .w(px(86.))
                                                .flex()
                                                .justify_end()
                                                .font_family(total_mono.clone())
                                                .text_color(theme.foreground)
                                                .child(format_token_count(total_out)),
                                        )
                                        .child(
                                            div()
                                                .w(px(86.))
                                                .flex()
                                                .justify_end()
                                                .font_family(total_mono)
                                                .text_color(theme.foreground)
                                                .child(format_token_count(total_cached)),
                                        )
                                        .child(
                                            div()
                                                .w(px(72.))
                                                .flex()
                                                .justify_end()
                                                .text_color(green)
                                                .child(format!("{total_cache_pct}%")),
                                        ),
                                )
                            }),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_end()
                    .child(
                        Button::new("usage-details-modal-close")
                            .primary()
                            .label("Close")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

fn render_modal_kpi_tile(
    label: &'static str,
    value: String,
    accent: Option<gpui_kit::Hsla>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .p_2p5()
        .gap_1()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(accent.unwrap_or(theme.foreground))
                .child(value),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_helpers_format_commas_percentages_and_multipliers() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1,000");
        assert_eq!(format_token_count(1_234_567), "1,234,567");

        assert_eq!(cache_percentage(0, 0), 0);
        assert_eq!(cache_percentage(250, 1000), 25);
        assert_eq!(cache_percentage(666, 1000), 67);

        assert_eq!(format_multiplier(None), "-");
        assert_eq!(format_multiplier(Some(1.0)), "1x");
        assert_eq!(format_multiplier(Some(1.5)), "1.5x");
        assert_eq!(format_multiplier(Some(0.33)), "0.33x");
    }
}
