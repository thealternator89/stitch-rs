use gpui_kit::{Context, IntoElement, Render, Window};

use crate::app_state::ActiveView;
use crate::features::placeholder::render_tool_placeholder;

pub struct StoryElaboratorView;

impl StoryElaboratorView {
    pub fn new() -> Self {
        Self
    }
}

impl Render for StoryElaboratorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        render_tool_placeholder(
            ActiveView::StoryElaborator,
            "Scheduled for Phases 7–8",
            "Ticket search, optional local repository picker, Git branch, and model selector will live here.",
            "Multi-turn Q&A chat feed, suggested answer chips, and refined Markdown implementation plan will appear here.",
            cx,
        )
    }
}
