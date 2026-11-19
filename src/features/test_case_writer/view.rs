use gpui_kit::{Context, IntoElement, Render, Window};

use crate::app_state::ActiveView;
use crate::features::placeholder::render_tool_placeholder;

pub struct TestCaseWriterView;

impl TestCaseWriterView {
    pub fn new() -> Self {
        Self
    }
}

impl Render for TestCaseWriterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        render_tool_placeholder(
            ActiveView::TestCaseWriter,
            "Scheduled for Phase 6",
            "Ticket autocomplete search, additional context, fetched ticket summary, and model selector will live here.",
            "Interactive test scenario table with reordering, soft-delete/undo, and Markdown export will appear here.",
            cx,
        )
    }
}
