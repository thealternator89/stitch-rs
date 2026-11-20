use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, TimeZone as _};
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Sizable as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, hsla, prelude::FluentBuilder as _, px, relative,
};

use crate::infrastructure::database::{DbSession, UsageDatabase};
use crate::infrastructure::runtime::spawn_tokio;
use crate::ui::components::{cache_percentage, format_multiplier, format_token_count};

const ALL_TOOLS_VALUE: &str = "__all__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryKpiSummary {
    pub total_runs: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub cache_savings_pct: u64,
}

/// Computes the 4 KPI summary metrics across a slice of sessions.
pub fn compute_kpi_summary(sessions: &[DbSession]) -> HistoryKpiSummary {
    let total_runs = sessions.len();
    let total_input_tokens: u64 = sessions.iter().map(DbSession::total_input_tokens).sum();
    let total_output_tokens: u64 = sessions.iter().map(DbSession::total_output_tokens).sum();
    let total_cached_tokens: u64 = sessions.iter().map(DbSession::total_cache_read_tokens).sum();
    let cache_savings_pct = cache_percentage(total_cached_tokens, total_input_tokens);
    HistoryKpiSummary {
        total_runs,
        total_input_tokens,
        total_output_tokens,
        total_cached_tokens,
        cache_savings_pct,
    }
}

/// Filters sessions by tool name (`ALL_TOOLS_VALUE` or exact tool name) and case-insensitive search query
/// across `context_reference`, `ai_output`, `pushed`, and `tool_name`.
pub fn filter_history_sessions(
    sessions: &[DbSession],
    tool_filter: &str,
    search_query: &str,
) -> Vec<DbSession> {
    let query = search_query.trim().to_ascii_lowercase();
    let filter_by_tool = !tool_filter.is_empty() && tool_filter != ALL_TOOLS_VALUE;

    sessions
        .iter()
        .filter(|session| {
            if filter_by_tool && !session.tool_name.eq_ignore_ascii_case(tool_filter) {
                return false;
            }
            if query.is_empty() {
                return true;
            }
            let ctx_match = session
                .context_reference
                .as_deref()
                .is_some_and(|s| s.to_ascii_lowercase().contains(&query));
            let out_match = session
                .ai_output
                .as_deref()
                .is_some_and(|s| s.to_ascii_lowercase().contains(&query));
            let push_match = session
                .pushed
                .as_deref()
                .is_some_and(|s| s.to_ascii_lowercase().contains(&query));
            let tool_match = session.tool_name.to_ascii_lowercase().contains(&query);
            ctx_match || out_match || push_match || tool_match
        })
        .cloned()
        .collect()
}

/// Computes the visual bar proportions for the **Context Size Relative Visualizer**:
/// - `bar_width_fraction`: scaled relative to `max_session_tokens` across the filtered list, with a minimum 6% width (`0.06`) when `session_tokens > 0`.
/// - `input_fraction` & `output_fraction`: proportional split between green (`input_tokens`) and cyan (`output_tokens`).
pub fn compute_visualizer_fractions(
    input_tokens: u64,
    output_tokens: u64,
    max_session_tokens: u64,
) -> (f32, f32, f32) {
    let total = input_tokens + output_tokens;
    if total == 0 || max_session_tokens == 0 {
        return (0.0, 0.0, 0.0);
    }
    let raw_width = (total as f64 / max_session_tokens as f64) as f32;
    let bar_width = raw_width.clamp(0.06, 1.0);
    let in_frac = (input_tokens as f64 / total as f64) as f32;
    let out_frac = (1.0 - in_frac).max(0.0);
    (bar_width, in_frac, out_frac)
}

fn format_timestamp_ms(timestamp_ms: i64) -> String {
    match Local.timestamp_millis_opt(timestamp_ms).single() {
        Some(dt) => dt.format("%b %d, %Y %H:%M").to_string(),
        None => "-".to_string(),
    }
}

fn icon_for_tool(tool_name: &str) -> LucideIcon {
    let lower = tool_name.to_ascii_lowercase();
    if lower.contains("explainer") {
        LucideIcon::BookOpen
    } else if lower.contains("pr") || lower.contains("review") {
        LucideIcon::GitPullRequest
    } else if lower.contains("story writer") {
        LucideIcon::FileText
    } else if lower.contains("test case") {
        LucideIcon::ClipboardCheck
    } else if lower.contains("elaborat") {
        LucideIcon::MessageSquareText
    } else if lower.contains("t-shirt") || lower.contains("estimat") {
        LucideIcon::Shirt
    } else {
        LucideIcon::ChartBar
    }
}

#[derive(Debug, Clone)]
struct ToolFilterOption {
    value: String,
    label: SharedString,
}

impl SelectItem for ToolFilterOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

pub struct UsageHistoryView {
    usage_db: Arc<UsageDatabase>,
    sessions: Vec<DbSession>,
    is_loading: bool,
    error_message: Option<String>,
    search_input: Entity<InputState>,
    search_query: String,
    tool_select: Entity<SelectState<Vec<ToolFilterOption>>>,
    selected_tool: String,
    expanded_sessions: HashSet<i64>,
    confirming_clear: bool,
    clear_confirm_gen: u64,
    _subscriptions: Vec<Subscription>,
}

impl UsageHistoryView {
    pub fn new(
        usage_db: Arc<UsageDatabase>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search by ticket, PR, Confluence ID, output, or pushed status...")
        });

        let initial_items = build_tool_select_items(&[]);
        let tool_select = cx.new(|cx| {
            SelectState::new(initial_items, Some(IndexPath::new(0)), window, cx)
        });

        let search_sub = cx.subscribe(&search_input, |this, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.search_query = input.read(cx).value().to_string();
                cx.notify();
            }
        });

        let select_sub = cx.subscribe(
            &tool_select,
            |this, _, event: &SelectEvent<Vec<ToolFilterOption>>, cx| {
                let SelectEvent::Confirm(maybe_val) = event;
                let chosen = maybe_val
                    .clone()
                    .unwrap_or_else(|| ALL_TOOLS_VALUE.to_string());
                this.selected_tool = chosen;
                cx.notify();
            },
        );

        let mut this = Self {
            usage_db,
            sessions: Vec::new(),
            is_loading: false,
            error_message: None,
            search_input,
            search_query: String::new(),
            tool_select,
            selected_tool: ALL_TOOLS_VALUE.to_string(),
            expanded_sessions: HashSet::new(),
            confirming_clear: false,
            clear_confirm_gen: 0,
            _subscriptions: vec![search_sub, select_sub],
        };

        this.reload_history(window, cx);
        this
    }

    /// Reloads session history from SQLite asynchronously.
    pub fn reload_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let db = Arc::clone(&self.usage_db);
        self.is_loading = true;
        self.error_message = None;
        cx.notify();

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move { db.get_history() }).await;
                let _ = cx.update(|window, cx| {
                    view.update(cx, |this, cx| {
                        this.is_loading = false;
                        match res {
                            Ok(Ok(rows)) => {
                                this.apply_loaded_sessions(rows, Some(window), cx);
                            }
                            Ok(Err(err)) => {
                                this.error_message = Some(err.to_string());
                            }
                            Err(err) => {
                                this.error_message = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    /// Reloads session history synchronously (called when a tool logs a session or records a push).
    pub fn refresh_from_db(&mut self, cx: &mut Context<Self>) {
        match self.usage_db.get_history() {
            Ok(rows) => {
                self.sessions = rows;
                self.error_message = None;
            }
            Err(err) => {
                self.error_message = Some(err.to_string());
            }
        }
        cx.notify();
    }

    fn apply_loaded_sessions(
        &mut self,
        rows: Vec<DbSession>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.sessions = rows;
        self.error_message = None;
        if let Some(window) = window {
            let items = build_tool_select_items(&self.sessions);
            let current = self.selected_tool.clone();
            self.tool_select.update(cx, |select, cx| {
                select.set_items(items, window, cx);
                select.set_selected_value(&current, window, cx);
            });
        }
    }

    fn handle_clear_click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirming_clear {
            self.confirming_clear = true;
            self.clear_confirm_gen = self.clear_confirm_gen.wrapping_add(1);
            let confirm_gen = self.clear_confirm_gen;
            cx.notify();

            let view = cx.entity();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let _ = view.update(cx, |this, cx| {
                    if this.confirming_clear && this.clear_confirm_gen == confirm_gen {
                        this.confirming_clear = false;
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }

        // Second click confirmed!
        self.confirming_clear = false;
        self.clear_confirm_gen = self.clear_confirm_gen.wrapping_add(1);
        let db = Arc::clone(&self.usage_db);
        let view = cx.entity();

        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move { db.clear_history() }).await;
                let _ = cx.update(|window, cx| {
                    view.update(cx, |this, cx| {
                        match res {
                            Ok(Ok(())) => {
                                this.sessions.clear();
                                this.expanded_sessions.clear();
                                this.selected_tool = ALL_TOOLS_VALUE.to_string();
                                let items = build_tool_select_items(&[]);
                                this.tool_select.update(cx, |select, cx| {
                                    select.set_items(items, window, cx);
                                    select.set_selected_index(Some(IndexPath::new(0)), window, cx);
                                });
                            }
                            Ok(Err(err)) => {
                                this.error_message = Some(err.to_string());
                            }
                            Err(err) => {
                                this.error_message = Some(err.to_string());
                            }
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    fn toggle_session_expanded(&mut self, session_id: i64, cx: &mut Context<Self>) {
        if !self.expanded_sessions.remove(&session_id) {
            self.expanded_sessions.insert(session_id);
        }
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let has_sessions = !self.sessions.is_empty();
        let confirming = self.confirming_clear;

        h_flex()
            .w_full()
            .px_6()
            .py_4()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
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
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .text_color(theme.foreground)
                            .child(Icon::new(LucideIcon::ChartBar)),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child("Usage History & Telemetry"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        "30-day local SQLite token audit log, cache hit savings, and per-phase LLM breakdown.",
                                    ),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("refresh-history-btn")
                            .outline()
                            .small()
                            .icon(Icon::new(LucideIcon::RefreshCw).xsmall())
                            .label("Refresh")
                            .disabled(self.is_loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reload_history(window, cx);
                            })),
                    )
                    .child({
                        let btn = Button::new("clear-all-history-btn")
                            .small()
                            .icon(Icon::new(LucideIcon::Trash).xsmall())
                            .disabled(!has_sessions)
                            .label(if confirming {
                                "Click again to confirm!"
                            } else {
                                "Clear All History"
                            });
                        let btn = if confirming {
                            btn.danger()
                        } else {
                            btn.outline()
                        };
                        btn.on_click(cx.listener(|this, _, window, cx| {
                            this.handle_clear_click(window, cx);
                        }))
                    }),
            )
    }

    fn render_kpi_cards(&self, kpi: &HistoryKpiSummary, cx: &App) -> impl IntoElement {
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let cyan = hsla(192.0 / 360.0, 0.85, 0.45, 1.0);

        h_flex()
            .w_full()
            .gap_3()
            .child(render_kpi_card(
                "Total Runs",
                format_token_count(kpi.total_runs as u64),
                "Recorded tool sessions (last 30 days)",
                LucideIcon::Layers,
                None,
                cx,
            ))
            .child(render_kpi_card(
                "Input Tokens",
                format_token_count(kpi.total_input_tokens),
                "Prompt & codebase context tokens",
                LucideIcon::ArrowUpRight,
                Some(green),
                cx,
            ))
            .child(render_kpi_card(
                "Output Tokens",
                format_token_count(kpi.total_output_tokens),
                "Generated response & JSONL tokens",
                LucideIcon::Sparkles,
                Some(cyan),
                cx,
            ))
            .child(render_kpi_card(
                "Cache Savings",
                format!("{}%", kpi.cache_savings_pct),
                &format!(
                    "{} cached prompt tokens",
                    format_token_count(kpi.total_cached_tokens)
                ),
                LucideIcon::Zap,
                Some(green),
                cx,
            ))
    }

    fn render_filter_bar(
        &self,
        filtered_count: usize,
        total_count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let has_active_filter =
            !self.search_query.trim().is_empty() || self.selected_tool != ALL_TOOLS_VALUE;

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                h_flex()
                    .flex_1()
                    .items_center()
                    .gap_2()
                    .child(
                        div().w(px(360.)).child(
                            Input::new(&self.search_input)
                                .small()
                                .prefix(
                                    Icon::new(LucideIcon::Search)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                )
                                .cleanable(true),
                        ),
                    )
                    .child(
                        div()
                            .w(px(210.))
                            .child(Select::new(&self.tool_select).small()),
                    )
                    .when(has_active_filter, |row| {
                        row.child(
                            Button::new("reset-history-filters-btn")
                                .ghost()
                                .small()
                                .icon(Icon::new(LucideIcon::X).xsmall())
                                .label("Reset Filters")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.search_query.clear();
                                    this.selected_tool = ALL_TOOLS_VALUE.to_string();
                                    this.search_input.update(cx, |input, cx| {
                                        input.set_value(String::new(), window, cx);
                                    });
                                    this.tool_select.update(cx, |select, cx| {
                                        select.set_selected_index(
                                            Some(IndexPath::new(0)),
                                            window,
                                            cx,
                                        );
                                    });
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if filtered_count == total_count {
                        format!("{total_count} session(s)")
                    } else {
                        format!("Showing {filtered_count} of {total_count} session(s)")
                    }),
            )
    }

    fn render_session_row(
        &self,
        session: &DbSession,
        max_session_tokens: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let green_bg = hsla(152.0 / 360.0, 0.76, 0.40, 0.14);
        let cyan = hsla(192.0 / 360.0, 0.85, 0.45, 1.0);
        let mono_font = theme.mono_font_family.clone();

        let session_id = session.id;
        let is_expanded = self.expanded_sessions.contains(&session_id);
        let chevron = if is_expanded {
            LucideIcon::ChevronDown
        } else {
            LucideIcon::ChevronRight
        };
        let tool_icon = icon_for_tool(&session.tool_name);

        let in_tokens = session.total_input_tokens();
        let out_tokens = session.total_output_tokens();
        let cached_tokens = session.total_cache_read_tokens();
        let total_tokens = session.total_tokens();
        let cache_pct = cache_percentage(cached_tokens, in_tokens);

        let (bar_width, in_frac, out_frac) =
            compute_visualizer_fractions(in_tokens, out_tokens, max_session_tokens);

        let ctx_ref = session
            .context_reference
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("-")
            .to_string();
        let produced = session
            .ai_output
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("-")
            .to_string();
        let pushed = session
            .pushed
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        let formatted_time = format_timestamp_ms(session.timestamp);

        v_flex()
            .w_full()
            .border_b_1()
            .border_color(theme.border)
            // Main clickable accordion header row
            .child(
                h_flex()
                    .id(("history-session-row", session_id as usize))
                    .w_full()
                    .px_4()
                    .py_3()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.secondary_hover))
                    .when(is_expanded, |s| s.bg(theme.sidebar))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_session_expanded(session_id, cx);
                    }))
                    // Expand chevron + Tool Icon & Name
                    .child(
                        h_flex()
                            .w(px(195.))
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                Icon::new(chevron)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(
                                Icon::new(tool_icon)
                                    .small()
                                    .text_color(theme.primary),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(session.tool_name.clone()),
                            ),
                    )
                    // Context Reference badge
                    .child(
                        div()
                            .w(px(140.))
                            .min_w_0()
                            .child(if ctx_ref == "-" {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("-")
                                    .into_any_element()
                            } else {
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(4.))
                                    .bg(theme.secondary)
                                    .text_xs()
                                    .font_family(mono_font.clone())
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(ctx_ref)
                                    .into_any_element()
                            }),
                    )
                    // Timestamp
                    .child(
                        div()
                            .w(px(130.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(formatted_time),
                    )
                    // Context Size Relative Visualizer bar (Green input + Cyan output)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(180.))
                            .gap_1()
                            .pr_2()
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .justify_between()
                                    .text_xs()
                                    .child(
                                        div()
                                            .font_family(mono_font.clone())
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.foreground)
                                            .child(format!(
                                                "{} tokens",
                                                format_token_count(total_tokens)
                                            )),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("{cache_pct}% cached")),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .h(px(7.))
                                    .rounded_full()
                                    .bg(theme.secondary)
                                    .overflow_hidden()
                                    .when(bar_width > 0.0, |track| {
                                        track.child(
                                            h_flex()
                                                .w(relative(bar_width))
                                                .h_full()
                                                .rounded_full()
                                                .overflow_hidden()
                                                .child(
                                                    div()
                                                        .w(relative(in_frac))
                                                        .h_full()
                                                        .bg(green),
                                                )
                                                .child(
                                                    div()
                                                        .w(relative(out_frac))
                                                        .h_full()
                                                        .bg(cyan),
                                                ),
                                        )
                                    }),
                            ),
                    )
                    // Produced column (`ai_output`)
                    .child(
                        div()
                            .w(px(135.))
                            .text_xs()
                            .text_color(theme.foreground)
                            .truncate()
                            .child(produced),
                    )
                    // Accepted column (`pushed` with green check icon or `-`)
                    .child(
                        div().w(px(135.)).child(match pushed {
                            Some(pushed_text) => h_flex()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .py_0p5()
                                .rounded(px(4.))
                                .bg(green_bg)
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(green)
                                .child(Icon::new(LucideIcon::CircleCheck).xsmall().text_color(green))
                                .child(div().truncate().child(pushed_text))
                                .into_any_element(),
                            None => div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("-")
                                .into_any_element(),
                        }),
                    ),
            )
            // Expanded LLM Usage Breakdown Table
            .when(is_expanded, |container| {
                container.child(self.render_expanded_llm_table(session, cx))
            })
    }

    fn render_expanded_llm_table(&self, session: &DbSession, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let green_bg = hsla(152.0 / 360.0, 0.76, 0.40, 0.14);
        let mono_font = theme.mono_font_family.clone();

        let usages = &session.llm_usages;
        let show_total = usages.len() > 1;
        let total_in = session.total_input_tokens();
        let total_out = session.total_output_tokens();
        let total_cached = session.total_cache_read_tokens();
        let total_cache_pct = cache_percentage(total_cached, total_in);

        div()
            .w_full()
            .px_8()
            .py_3()
            .bg(theme.sidebar)
            .border_t_1()
            .border_color(theme.border)
            .child(if usages.is_empty() {
                div()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No granular LLM usage rows recorded for this session.")
                    .into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .overflow_hidden()
                    // Table Header
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
                            .child(div().w(px(160.)).child("Model"))
                            .child(
                                div()
                                    .w(px(72.))
                                    .flex()
                                    .justify_end()
                                    .child("Multiplier"),
                            )
                            .child(
                                div()
                                    .w(px(105.))
                                    .flex()
                                    .justify_end()
                                    .child("Input Tokens"),
                            )
                            .child(
                                div()
                                    .w(px(105.))
                                    .flex()
                                    .justify_end()
                                    .child("Output Tokens"),
                            )
                            .child(
                                div()
                                    .w(px(105.))
                                    .flex()
                                    .justify_end()
                                    .child("Cached Tokens"),
                            )
                            .child(
                                div()
                                    .w(px(80.))
                                    .flex()
                                    .justify_end()
                                    .child("% Cached"),
                            ),
                    )
                    // Table Rows
                    .children(usages.iter().enumerate().map(|(idx, row)| {
                        let row_pct =
                            cache_percentage(row.cache_read_tokens, row.input_tokens);
                        let row_mono = mono_font.clone();
                        h_flex()
                            .w_full()
                            .px_3()
                            .py_2()
                            .items_center()
                            .when(idx > 0, |el| el.border_t_1().border_color(theme.border))
                            .text_xs()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .pr_2()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .child(row.label.clone()),
                            )
                            .child(
                                div()
                                    .w(px(160.))
                                    .pr_2()
                                    .truncate()
                                    .font_family(row_mono.clone())
                                    .text_color(theme.muted_foreground)
                                    .child(row.model.clone()),
                            )
                            .child(
                                div()
                                    .w(px(72.))
                                    .flex()
                                    .justify_end()
                                    .font_family(row_mono.clone())
                                    .text_color(theme.muted_foreground)
                                    .child(format_multiplier(row.multiplier)),
                            )
                            .child(
                                div()
                                    .w(px(105.))
                                    .flex()
                                    .justify_end()
                                    .font_family(row_mono.clone())
                                    .text_color(theme.foreground)
                                    .child(format_token_count(row.input_tokens)),
                            )
                            .child(
                                div()
                                    .w(px(105.))
                                    .flex()
                                    .justify_end()
                                    .font_family(row_mono.clone())
                                    .text_color(theme.foreground)
                                    .child(format_token_count(row.output_tokens)),
                            )
                            .child(
                                div()
                                    .w(px(105.))
                                    .flex()
                                    .justify_end()
                                    .font_family(row_mono)
                                    .text_color(theme.muted_foreground)
                                    .child(format_token_count(row.cache_read_tokens)),
                            )
                            .child(
                                div()
                                    .w(px(80.))
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
                    }))
                    // Bold Total Summary Footer Row when llm_usages.len() > 1
                    .when(show_total, |table| {
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
                                        .child("Total"),
                                )
                                .child(div().w(px(160.)).child(""))
                                .child(div().w(px(72.)).child(""))
                                .child(
                                    div()
                                        .w(px(105.))
                                        .flex()
                                        .justify_end()
                                        .font_family(total_mono.clone())
                                        .text_color(theme.foreground)
                                        .child(format_token_count(total_in)),
                                )
                                .child(
                                    div()
                                        .w(px(105.))
                                        .flex()
                                        .justify_end()
                                        .font_family(total_mono.clone())
                                        .text_color(theme.foreground)
                                        .child(format_token_count(total_out)),
                                )
                                .child(
                                    div()
                                        .w(px(105.))
                                        .flex()
                                        .justify_end()
                                        .font_family(total_mono)
                                        .text_color(theme.foreground)
                                        .child(format_token_count(total_cached)),
                                )
                                .child(
                                    div()
                                        .w(px(80.))
                                        .flex()
                                        .justify_end()
                                        .text_color(green)
                                        .child(format!("{total_cache_pct}%")),
                                ),
                        )
                    })
                    .into_any_element()
            })
    }
}

impl Render for UsageHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let filtered =
            filter_history_sessions(&self.sessions, &self.selected_tool, &self.search_query);
        let kpi = compute_kpi_summary(&filtered);
        let max_session_tokens = filtered
            .iter()
            .map(DbSession::total_tokens)
            .max()
            .unwrap_or(0);
        let total_count = self.sessions.len();
        let filtered_count = filtered.len();

        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let cyan = hsla(192.0 / 360.0, 0.85, 0.45, 1.0);

        let header = self.render_header(cx).into_any_element();
        let kpi_cards = self.render_kpi_cards(&kpi, cx).into_any_element();
        let filter_bar = self
            .render_filter_bar(filtered_count, total_count, cx)
            .into_any_element();
        let session_rows: Vec<AnyElement> = filtered
            .iter()
            .map(|session| {
                self.render_session_row(session, max_session_tokens, cx)
                    .into_any_element()
            })
            .collect();

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(header)
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .p_6()
                    .gap_4()
                    .when_some(self.error_message.clone(), |el, err| {
                        el.child(
                            div()
                                .w_full()
                                .p_3()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.danger)
                                .text_xs()
                                .text_color(theme.danger)
                                .child(format!("Database error: {err}")),
                        )
                    })
                    .child(kpi_cards)
                    .child(filter_bar)
                    // Accordion Table Card
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .overflow_hidden()
                            // Column headers
                            .child(
                                h_flex()
                                    .w_full()
                                    .px_4()
                                    .py_2p5()
                                    .items_center()
                                    .gap_3()
                                    .bg(theme.sidebar)
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(div().w(px(195.)).child("Tool"))
                                    .child(div().w(px(140.)).child("Context"))
                                    .child(div().w(px(130.)).child("Timestamp"))
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .min_w(px(180.))
                                            .items_center()
                                            .justify_between()
                                            .pr_2()
                                            .child("Context Size Visualizer")
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .font_weight(FontWeight::NORMAL)
                                                    .child(
                                                        h_flex()
                                                            .items_center()
                                                            .gap_1()
                                                            .child(
                                                                div()
                                                                    .size(px(6.))
                                                                    .rounded_full()
                                                                    .bg(green),
                                                            )
                                                            .child("In"),
                                                    )
                                                    .child(
                                                        h_flex()
                                                            .items_center()
                                                            .gap_1()
                                                            .child(
                                                                div()
                                                                    .size(px(6.))
                                                                    .rounded_full()
                                                                    .bg(cyan),
                                                            )
                                                            .child("Out"),
                                                    ),
                                            ),
                                    )
                                    .child(div().w(px(135.)).child("Produced"))
                                    .child(div().w(px(135.)).child("Accepted")),
                            )
                            // Scrollable rows or empty state
                            .child(if session_rows.is_empty() {
                                v_flex()
                                    .flex_1()
                                    .w_full()
                                    .items_center()
                                    .justify_center()
                                    .gap_2()
                                    .p_8()
                                    .child(
                                        Icon::new(LucideIcon::ChartBar)
                                            .large()
                                            .text_color(theme.muted_foreground),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.foreground)
                                            .child(if self.sessions.is_empty() {
                                                "No usage history recorded yet"
                                            } else {
                                                "No sessions match your filter"
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(if self.sessions.is_empty() {
                                                "Run any tool (T-Shirt Estimator, Story Writer, Test Case Writer, Story Elaborator, or PR Reviewer) to populate token telemetry."
                                            } else {
                                                "Try clearing the search query or selecting 'All Tools'."
                                            }),
                                    )
                                    .into_any_element()
                            } else {
                                v_flex()
                                    .id("usage-history-session-list")
                                    .flex_1()
                                    .min_h_0()
                                    .w_full()
                                    .overflow_y_scrollbar()
                                    .children(session_rows)
                                    .into_any_element()
                            }),
                    ),
            )
    }
}

fn render_kpi_card(
    title: &'static str,
    value: String,
    subtitle: &str,
    icon: LucideIcon,
    accent: Option<gpui_kit::Hsla>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let accent_color = accent.unwrap_or(theme.primary);

    v_flex()
        .flex_1()
        .p_4()
        .gap_1p5()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(title),
                )
                .child(Icon::new(icon).small().text_color(accent_color)),
        )
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::BOLD)
                .text_color(accent.unwrap_or(theme.foreground))
                .child(value),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(subtitle.to_string()),
        )
}

fn build_tool_select_items(sessions: &[DbSession]) -> Vec<ToolFilterOption> {
    let mut names = vec![
        "T-Shirt Size Estimator".to_string(),
        "Story Writer".to_string(),
        "Test Case Writer".to_string(),
        "Story Elaborator".to_string(),
        "Code Explainer".to_string(),
        "PR Reviewer".to_string(),
    ];
    for s in sessions {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&s.tool_name)) {
            names.push(s.tool_name.clone());
        }
    }

    let mut items = Vec::with_capacity(names.len() + 1);
    items.push(ToolFilterOption {
        value: ALL_TOOLS_VALUE.to_string(),
        label: SharedString::from("All Tools"),
    });
    for name in names {
        items.push(ToolFilterOption {
            value: name.clone(),
            label: SharedString::from(name),
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::database::DbLlmUsage;

    fn sample_sessions() -> Vec<DbSession> {
        vec![
            DbSession {
                id: 1,
                tool_name: "PR Reviewer".to_string(),
                context_reference: Some("PR - 42".to_string()),
                timestamp: 1_700_000_500_000,
                ai_output: Some("5 comments".to_string()),
                pushed: Some("2 PR comment(s)".to_string()),
                llm_usages: vec![
                    DbLlmUsage {
                        id: 1,
                        session_id: 1,
                        label: "Phase: Security".to_string(),
                        model: "gpt-5".to_string(),
                        input_tokens: 6_000,
                        output_tokens: 2_000,
                        cache_read_tokens: 3_000,
                        cost: 0.08,
                        multiplier: Some(1.0),
                    },
                    DbLlmUsage {
                        id: 2,
                        session_id: 1,
                        label: "Critic Phase".to_string(),
                        model: "claude-sonnet-4.5".to_string(),
                        input_tokens: 4_000,
                        output_tokens: 1_000,
                        cache_read_tokens: 1_000,
                        cost: 0.05,
                        multiplier: Some(1.5),
                    },
                ],
            },
            DbSession {
                id: 2,
                tool_name: "Story Writer".to_string(),
                context_reference: Some("Confluence - 9912".to_string()),
                timestamp: 1_700_000_000_000,
                ai_output: Some("3 stories".to_string()),
                pushed: Some("1 story".to_string()),
                llm_usages: vec![DbLlmUsage {
                    id: 3,
                    session_id: 2,
                    label: "Story Generation".to_string(),
                    model: "gpt-4.1".to_string(),
                    input_tokens: 10_000,
                    output_tokens: 2_000,
                    cache_read_tokens: 6_000,
                    cost: 0.10,
                    multiplier: Some(1.0),
                }],
            },
        ]
    }

    #[test]
    fn compute_kpi_summary_aggregates_runs_tokens_and_cache_savings() {
        let sessions = sample_sessions();
        let kpi = compute_kpi_summary(&sessions);
        assert_eq!(kpi.total_runs, 2);
        assert_eq!(kpi.total_input_tokens, 20_000);
        assert_eq!(kpi.total_output_tokens, 5_000);
        assert_eq!(kpi.total_cached_tokens, 10_000);
        assert_eq!(kpi.cache_savings_pct, 50);
    }

    #[test]
    fn filter_history_sessions_filters_by_tool_and_query() {
        let sessions = sample_sessions();

        let all = filter_history_sessions(&sessions, ALL_TOOLS_VALUE, "");
        assert_eq!(all.len(), 2);

        let by_tool = filter_history_sessions(&sessions, "PR Reviewer", "");
        assert_eq!(by_tool.len(), 1);
        assert_eq!(by_tool[0].id, 1);

        let by_context = filter_history_sessions(&sessions, ALL_TOOLS_VALUE, "9912");
        assert_eq!(by_context.len(), 1);
        assert_eq!(by_context[0].id, 2);

        let by_pushed = filter_history_sessions(&sessions, ALL_TOOLS_VALUE, "pr comment");
        assert_eq!(by_pushed.len(), 1);
        assert_eq!(by_pushed[0].id, 1);
    }

    #[test]
    fn compute_visualizer_fractions_enforces_minimum_width_and_split() {
        assert_eq!(compute_visualizer_fractions(0, 0, 10_000), (0.0, 0.0, 0.0));

        //Tiny non-zero session clamps to 6% min visual width
        let (width, in_f, out_f) = compute_visualizer_fractions(75, 25, 100_000);
        assert!((width - 0.06).abs() < 1e-5);
        assert!((in_f - 0.75).abs() < 1e-5);
        assert!((out_f - 0.25).abs() < 1e-5);

        // Max session has 100% width
        let (width, in_f, out_f) = compute_visualizer_fractions(8_000, 2_000, 10_000);
        assert!((width - 1.0).abs() < 1e-5);
        assert!((in_f - 0.8).abs() < 1e-5);
        assert!((out_f - 0.2).abs() < 1e-5);
    }
}
