use gpui_kit::{Context, IntoElement, Render, Window};

use crate::app_state::ActiveView;
use crate::features::placeholder::render_tool_placeholder;

pub struct PrReviewerView;

impl PrReviewerView {
    pub fn new() -> Self {
        Self
    }
}

impl Render for PrReviewerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        render_tool_placeholder(
            ActiveView::PrReviewer,
            "Scheduled for Phases 9–11",
            "PR discovery tabs, repository path verification, grouped review phases checklist, and parallelism controls will live here.",
            "Live phase progress, syntax-highlighted code context comments, AI Critic toggles, and PR posting controls will appear here.",
            cx,
        )
    }
}
