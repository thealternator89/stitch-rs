use gpui_kit::{Context, IntoElement, Render, Window};

use crate::app_state::ActiveView;
use crate::features::placeholder::render_tool_placeholder;

pub struct UsageHistoryView;

impl UsageHistoryView {
    pub fn new() -> Self {
        Self
    }
}

impl Render for UsageHistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        render_tool_placeholder(
            ActiveView::UsageHistory,
            "Scheduled for Phase 12",
            "Search filter and tool selector for the 30-day local SQLite history database will live here.",
            "KPI summary cards, relative context size bars, and expandable per-phase LLM usage tables will appear here.",
            cx,
        )
    }
}
