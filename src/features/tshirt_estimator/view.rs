use gpui_kit::{Context, IntoElement, Render, Window};

use crate::app_state::ActiveView;
use crate::features::placeholder::render_tool_placeholder;

pub struct TShirtEstimatorView;

impl TShirtEstimatorView {
    pub fn new() -> Self {
        Self
    }
}

impl Render for TShirtEstimatorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        render_tool_placeholder(
            ActiveView::TShirtEstimator,
            "Scheduled for Phase 3",
            "Proposed change description, local repository path picker, Git branch, and Copilot model selector will live here.",
            "Live tool-status console and XS–XL T-shirt estimate reasoning will appear here.",
            cx,
        )
    }
}
