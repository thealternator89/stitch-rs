use gpui_kit::{Context, IntoElement, Render, Window};

use crate::app_state::ActiveView;
use crate::features::placeholder::render_tool_placeholder;

pub struct StoryWriterView;

impl StoryWriterView {
    pub fn new() -> Self {
        Self
    }
}

impl Render for StoryWriterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        render_tool_placeholder(
            ActiveView::StoryWriter,
            "Scheduled for Phases 4–5",
            "Confluence page autocomplete, parent Feature search, additional context, and model selector will live here.",
            "Streamed user story cards with inline editing and one-click ticket creation will appear here.",
            cx,
        )
    }
}
