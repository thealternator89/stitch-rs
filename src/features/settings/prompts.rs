use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Window, div, hsla, prelude::FluentBuilder as _, px,
};

use crate::app_state::AppState;
use crate::features::settings::prompt_validator::{
    PromptValidationResult, ValidationVerdict, check_story_writer_prompt_complexity,
    check_test_case_writer_prompt_complexity, parse_validation_response,
};
use crate::features::story_writer::prompts::{
    DEFAULT_STORY_AC_PROMPT, DEFAULT_STORY_DESCRIPTION_PROMPT, DEFAULT_STORY_NOTES_PROMPT,
    DEFAULT_STORY_TITLE_PROMPT,
};
use crate::features::test_case_writer::prompts::{
    DEFAULT_TC_DESCRIPTION_PROMPT, DEFAULT_TC_EXPECTED_RESULT_PROMPT,
    DEFAULT_TC_PRECONDITIONS_PROMPT, DEFAULT_TC_STEPS_PROMPT, SYSTEM_TC_PRIORITY_PROMPT,
};
use crate::infrastructure::runtime::spawn_tokio;
use crate::ui::components::MarkdownView;
use crate::types::{
    AppSettings, CodeExplainerPrompts, SolutionDesignerPrompts, StoryElaboratorPrompts,
    StoryWriterPrompts, TestCaseWriterPrompts,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptsSubTab {
    #[default]
    StoryWriter,
    TestCaseWriter,
    StoryElaborator,
    CodeExplainer,
    SolutionDesigner,
}

impl PromptsSubTab {
    const ALL: &'static [Self] = &[
        Self::StoryWriter,
        Self::TestCaseWriter,
        Self::StoryElaborator,
        Self::CodeExplainer,
        Self::SolutionDesigner,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::StoryWriter => "prompts-subtab-story-writer",
            Self::TestCaseWriter => "prompts-subtab-test-case-writer",
            Self::StoryElaborator => "prompts-subtab-story-elaborator",
            Self::CodeExplainer => "prompts-subtab-code-explainer",
            Self::SolutionDesigner => "prompts-subtab-solution-designer",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::StoryWriter => "Story Writer",
            Self::TestCaseWriter => "Test Case Writer",
            Self::StoryElaborator => "Story Elaborator",
            Self::CodeExplainer => "Code Explainer",
            Self::SolutionDesigner => "Solution Designer",
        }
    }

    fn icon(self) -> LucideIcon {
        match self {
            Self::StoryWriter => LucideIcon::FileText,
            Self::TestCaseWriter => LucideIcon::ClipboardCheck,
            Self::StoryElaborator => LucideIcon::MessageSquareText,
            Self::CodeExplainer => LucideIcon::BookOpen,
            Self::SolutionDesigner => LucideIcon::Layers,
        }
    }
}

#[derive(Debug, Clone)]
enum PromptFeedback {
    Saved(String),
    Error(String),
}

/// Settings > Prompts tab: custom prompt overrides and Prompt Complexity Validator.
pub struct PromptsSettingsView {
    app_state: Entity<AppState>,
    sub_tab: PromptsSubTab,
    story_general_input: Entity<TextareaState>,
    story_title_input: Entity<TextareaState>,
    story_description_input: Entity<TextareaState>,
    story_ac_input: Entity<TextareaState>,
    story_notes_input: Entity<TextareaState>,
    is_checking_story: bool,
    last_checked_story_key: Option<String>,
    cached_story_result: Option<Result<PromptValidationResult, String>>,
    tc_general_input: Entity<TextareaState>,
    tc_description_input: Entity<TextareaState>,
    tc_preconditions_input: Entity<TextareaState>,
    tc_steps_input: Entity<TextareaState>,
    tc_expected_result_input: Entity<TextareaState>,
    is_checking_tc: bool,
    last_checked_tc_key: Option<String>,
    cached_tc_result: Option<Result<PromptValidationResult, String>>,
    elaborator_general_input: Entity<TextareaState>,
    explainer_general_input: Entity<TextareaState>,
    solution_designer_general_input: Entity<TextareaState>,
    feedback: Option<PromptFeedback>,
    _subscriptions: Vec<Subscription>,
}

impl PromptsSettingsView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sw = &initial_settings.prompts.story_writer;
        let tc = &initial_settings.prompts.test_case_writer;
        let se = &initial_settings.prompts.story_elaborator;
        let ce = &initial_settings.prompts.code_explainer;
        let sd = &initial_settings.prompts.solution_designer;

        let story_general_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Optional general instructions inserted between the default header context and story details (e.g., 'Target a Rust & TypeScript architecture. Focus on clear boundary cases.')")
                .default_value(sw.general.clone())
        });
        let story_title_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_STORY_TITLE_PROMPT)
                .default_value(sw.title.clone())
        });
        let story_description_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_STORY_DESCRIPTION_PROMPT)
                .default_value(sw.description.clone())
        });
        let story_ac_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_STORY_AC_PROMPT)
                .default_value(sw.acceptance_criteria.clone())
        });
        let story_notes_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_STORY_NOTES_PROMPT)
                .default_value(sw.notes.clone())
        });

        let tc_general_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Optional general instructions inserted between the default header context and ticket details (e.g., 'Emphasize accessibility testing and responsiveness.')")
                .default_value(tc.general.clone())
        });
        let tc_description_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_TC_DESCRIPTION_PROMPT)
                .default_value(tc.description.clone())
        });
        let tc_preconditions_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_TC_PRECONDITIONS_PROMPT)
                .default_value(tc.pre_conditions.clone())
        });
        let tc_steps_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_TC_STEPS_PROMPT)
                .default_value(tc.steps.clone())
        });
        let tc_expected_result_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(DEFAULT_TC_EXPECTED_RESULT_PROMPT)
                .default_value(tc.expected_result.clone())
        });

        let elaborator_general_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Optional general instructions inserted into the system instructions for Story Elaborator (e.g., 'Target a clean architecture, write plans in a very structured way with clear file trees.')")
                .default_value(se.general.clone())
        });

        let explainer_general_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Optional general instructions inserted into the system instructions for Code Explainer (e.g., 'Write for new joiners. Always include a glossary and worked calculation examples.')")
                .default_value(ce.general.clone())
        });

        let solution_designer_general_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Optional general instructions inserted into Solution Designer prompts (e.g., 'Prefer event-driven boundaries, always include C4-style Mermaid diagrams and explicit non-goals.')")
                .default_value(sd.general.clone())
        });

        let mut subs = Vec::new();
        for input in [
            &story_general_input,
            &story_title_input,
            &story_description_input,
            &story_ac_input,
            &story_notes_input,
        ] {
            let sub = cx.subscribe(input, |this, _, event, cx| {
                match event {
                    InputEvent::Blur => {
                        if this.is_story_dirty(cx) {
                            this.save_story_prompts(false, cx);
                        }
                    }
                    InputEvent::Change => {
                        this.feedback = None;
                        cx.notify();
                    }
                    _ => {}
                }
            });
            subs.push(sub);
        }

        for input in [
            &tc_general_input,
            &tc_description_input,
            &tc_preconditions_input,
            &tc_steps_input,
            &tc_expected_result_input,
        ] {
            let sub = cx.subscribe(input, |this, _, event, cx| {
                match event {
                    InputEvent::Blur => {
                        if this.is_tc_dirty(cx) {
                            this.save_tc_prompts(false, cx);
                        }
                    }
                    InputEvent::Change => {
                        this.feedback = None;
                        cx.notify();
                    }
                    _ => {}
                }
            });
            subs.push(sub);
        }

        let elaborator_sub = cx.subscribe(&elaborator_general_input, |this, _, event, cx| {
            match event {
                InputEvent::Blur => {
                    if this.is_elaborator_dirty(cx) {
                        this.save_elaborator_prompts(false, cx);
                    }
                }
                InputEvent::Change => {
                    this.feedback = None;
                    cx.notify();
                }
                _ => {}
            }
        });
        subs.push(elaborator_sub);

        let explainer_sub = cx.subscribe(&explainer_general_input, |this, _, event, cx| {
            match event {
                InputEvent::Blur => {
                    if this.is_explainer_dirty(cx) {
                        this.save_explainer_prompts(false, cx);
                    }
                }
                InputEvent::Change => {
                    this.feedback = None;
                    cx.notify();
                }
                _ => {}
            }
        });
        subs.push(explainer_sub);

        let sd_sub = cx.subscribe(&solution_designer_general_input, |this, _, event, cx| {
            match event {
                InputEvent::Blur => {
                    if this.is_solution_designer_dirty(cx) {
                        this.save_solution_designer_prompts(false, cx);
                    }
                }
                InputEvent::Change => {
                    this.feedback = None;
                    cx.notify();
                }
                _ => {}
            }
        });
        subs.push(sd_sub);

        let state_sub = cx.observe(&app_state, |_, _, cx| cx.notify());
        subs.push(state_sub);

        Self {
            app_state,
            sub_tab: PromptsSubTab::default(),
            story_general_input,
            story_title_input,
            story_description_input,
            story_ac_input,
            story_notes_input,
            is_checking_story: false,
            last_checked_story_key: None,
            cached_story_result: None,
            tc_general_input,
            tc_description_input,
            tc_preconditions_input,
            tc_steps_input,
            tc_expected_result_input,
            is_checking_tc: false,
            last_checked_tc_key: None,
            cached_tc_result: None,
            elaborator_general_input,
            explainer_general_input,
            solution_designer_general_input,
            feedback: None,
            _subscriptions: subs,
        }
    }

    fn apply_save_outcome(
        &mut self,
        res: anyhow::Result<()>,
        success_msg: Option<&str>,
        err_prefix: &str,
        cx: &mut Context<Self>,
    ) {
        match res {
            Ok(()) => {
                if let Some(msg) = success_msg {
                    self.feedback = Some(PromptFeedback::Saved(msg.to_string()));
                }
            }
            Err(err) => {
                self.feedback = Some(PromptFeedback::Error(format!("{err_prefix}: {err}")));
            }
        }
        cx.notify();
    }

    fn current_story_prompts(&self, cx: &App) -> StoryWriterPrompts {
        StoryWriterPrompts {
            general: self.story_general_input.read(cx).value().trim().to_string(),
            title: self.story_title_input.read(cx).value().trim().to_string(),
            description: self
                .story_description_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
            acceptance_criteria: self.story_ac_input.read(cx).value().trim().to_string(),
            notes: self.story_notes_input.read(cx).value().trim().to_string(),
        }
    }

    fn is_story_dirty(&self, cx: &App) -> bool {
        let current = self.current_story_prompts(cx);
        let saved = &self.app_state.read(cx).settings.prompts.story_writer;
        &current != saved
    }

    fn save_story_prompts(&mut self, show_saved_banner: bool, cx: &mut Context<Self>) {
        let next = self.current_story_prompts(cx);
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.story_writer = next;
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        self.apply_save_outcome(
            res,
            show_saved_banner.then_some("Story Writer custom prompts saved."),
            "Failed to save custom prompts",
            cx,
        );
    }

    fn reset_story_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [
            &self.story_general_input,
            &self.story_title_input,
            &self.story_description_input,
            &self.story_ac_input,
            &self.story_notes_input,
        ] {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        self.last_checked_story_key = None;
        self.cached_story_result = None;

        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.story_writer = StoryWriterPrompts::default();
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        self.apply_save_outcome(
            res,
            Some("Story Writer prompts reset to defaults."),
            "Failed to reset custom prompts",
            cx,
        );
    }

    fn current_tc_prompts(&self, cx: &App) -> TestCaseWriterPrompts {
        TestCaseWriterPrompts {
            general: self.tc_general_input.read(cx).value().trim().to_string(),
            description: self
                .tc_description_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
            pre_conditions: self
                .tc_preconditions_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
            steps: self.tc_steps_input.read(cx).value().trim().to_string(),
            expected_result: self
                .tc_expected_result_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
        }
    }

    fn is_tc_dirty(&self, cx: &App) -> bool {
        let current = self.current_tc_prompts(cx);
        let saved = &self.app_state.read(cx).settings.prompts.test_case_writer;
        &current != saved
    }

    fn save_tc_prompts(&mut self, show_saved_banner: bool, cx: &mut Context<Self>) {
        let next = self.current_tc_prompts(cx);
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.test_case_writer = next;
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        self.apply_save_outcome(
            res,
            show_saved_banner.then_some("Test Case Writer custom prompts saved."),
            "Failed to save custom prompts",
            cx,
        );
    }

    fn reset_tc_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [
            &self.tc_general_input,
            &self.tc_description_input,
            &self.tc_preconditions_input,
            &self.tc_steps_input,
            &self.tc_expected_result_input,
        ] {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
        self.last_checked_tc_key = None;
        self.cached_tc_result = None;

        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.test_case_writer = TestCaseWriterPrompts::default();
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        self.apply_save_outcome(
            res,
            Some("Test Case Writer prompts reset to defaults."),
            "Failed to reset custom prompts",
            cx,
        );
    }

    fn check_story_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_checking_story {
            return;
        }

        if self.is_story_dirty(cx) {
            self.save_story_prompts(false, cx);
        }

        let prompts = self.current_story_prompts(cx);
        let has_custom = [
            &prompts.general,
            &prompts.title,
            &prompts.description,
            &prompts.acceptance_criteria,
            &prompts.notes,
        ]
        .iter()
        .any(|s| !s.trim().is_empty());

        if !has_custom {
            let immediate = parse_validation_response(
                "PASS: No customized prompt statements detected. Please customize at least one field before checking.",
            );
            let modal = cx.new(|_| PromptValidationModalView::with_outcome("Story Writer", Ok(immediate)));
            open_validation_dialog(modal, window, cx);
            return;
        }

        let current_key = serde_json::to_string(&prompts).unwrap_or_default();
        if self.last_checked_story_key.as_deref() == Some(current_key.as_str()) {
            if let Some(cached) = self.cached_story_result.clone() {
                let modal = cx.new(|_| PromptValidationModalView::with_outcome("Story Writer", cached));
                open_validation_dialog(modal, window, cx);
                return;
            }
        }

        let (copilot, settings) = {
            let state = self.app_state.read(cx);
            (state.copilot_service.clone(), state.settings.clone())
        };

        self.is_checking_story = true;
        cx.notify();

        let modal = cx.new(|_| PromptValidationModalView::loading("Story Writer"));
        open_validation_dialog(modal.clone(), window, cx);

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    check_story_writer_prompt_complexity(copilot, &prompts, &settings).await
                })
                .await;

                let outcome = match res {
                    Ok(Ok(parsed)) => Ok(parsed),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                };

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.is_checking_story = false;
                        this.last_checked_story_key = Some(current_key);
                        this.cached_story_result = Some(outcome.clone());
                        cx.notify();
                    });
                    modal.update(cx, |m, cx| {
                        m.set_outcome(outcome, cx);
                    });
                });
            })
            .detach();
    }

    fn check_tc_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_checking_tc {
            return;
        }

        if self.is_tc_dirty(cx) {
            self.save_tc_prompts(false, cx);
        }

        let prompts = self.current_tc_prompts(cx);
        let has_custom = [
            &prompts.general,
            &prompts.description,
            &prompts.pre_conditions,
            &prompts.steps,
            &prompts.expected_result,
        ]
        .iter()
        .any(|s| !s.trim().is_empty());

        if !has_custom {
            let immediate = parse_validation_response(
                "PASS: No customized prompt statements detected. Please customize at least one field before checking.",
            );
            let modal =
                cx.new(|_| PromptValidationModalView::with_outcome("Test Case Writer", Ok(immediate)));
            open_validation_dialog(modal, window, cx);
            return;
        }

        let current_key = serde_json::to_string(&prompts).unwrap_or_default();
        if self.last_checked_tc_key.as_deref() == Some(current_key.as_str()) {
            if let Some(cached) = self.cached_tc_result.clone() {
                let modal =
                    cx.new(|_| PromptValidationModalView::with_outcome("Test Case Writer", cached));
                open_validation_dialog(modal, window, cx);
                return;
            }
        }

        let (copilot, settings) = {
            let state = self.app_state.read(cx);
            (state.copilot_service.clone(), state.settings.clone())
        };

        self.is_checking_tc = true;
        cx.notify();

        let modal = cx.new(|_| PromptValidationModalView::loading("Test Case Writer"));
        open_validation_dialog(modal.clone(), window, cx);

        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let res = spawn_tokio(async move {
                    check_test_case_writer_prompt_complexity(copilot, &prompts, &settings).await
                })
                .await;

                let outcome = match res {
                    Ok(Ok(parsed)) => Ok(parsed),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(err) => Err(err.to_string()),
                };

                let _ = cx.update(|_, cx| {
                    view.update(cx, |this, cx| {
                        this.is_checking_tc = false;
                        this.last_checked_tc_key = Some(current_key);
                        this.cached_tc_result = Some(outcome.clone());
                        cx.notify();
                    });
                    modal.update(cx, |m, cx| {
                        m.set_outcome(outcome, cx);
                    });
                });
            })
            .detach();
    }

    fn render_prompt_toolbar(
        &self,
        id_prefix: &'static str,
        description: &'static str,
        is_dirty: bool,
        is_checking: bool,
        on_check: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        on_reset: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        on_save: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        theme: &gpui_kit::component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .items_start()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(description),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .flex_wrap()
                    .w_full()
                    .justify_end()
                    .child(
                        Button::new(format!("{id_prefix}-check"))
                            .small()
                            .disabled(is_checking)
                            .loading(is_checking)
                            .icon(Icon::new(LucideIcon::ShieldCheck).small())
                            .label(if is_checking {
                                "Checking..."
                            } else {
                                "Check Prompt"
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_check(this, window, cx);
                            })),
                    )
                    .child(
                        Button::new(format!("{id_prefix}-reset"))
                            .ghost()
                            .small()
                            .disabled(is_checking)
                            .icon(Icon::new(LucideIcon::RotateCcw).small())
                            .label("Reset Defaults")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_reset(this, window, cx);
                            })),
                    )
                    .child({
                        let btn = Button::new(format!("{id_prefix}-save"))
                            .small()
                            .disabled(!is_dirty || is_checking)
                            .icon(Icon::new(LucideIcon::Save).small())
                            .label("Save Changes")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_save(this, window, cx);
                            }));
                        if is_dirty { btn.primary() } else { btn.ghost() }
                    }),
            )
    }

    fn render_feedback_row(
        &self,
        feedback: Option<PromptFeedback>,
        theme: &gpui_kit::component::Theme,
    ) -> Option<impl IntoElement> {
        let fb = feedback?;
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let (msg, color, icon) = match fb {
            PromptFeedback::Saved(m) => (m, green, LucideIcon::Check),
            PromptFeedback::Error(m) => (m, red, LucideIcon::CircleAlert),
        };
        Some(
            h_flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2p5()
                .rounded(theme.radius)
                .border_1()
                .border_color(color.opacity(0.4))
                .bg(color.opacity(0.08))
                .text_xs()
                .text_color(color)
                .child(Icon::new(icon).small())
                .child(div().flex_1().child(msg)),
        )
    }

    fn render_story_writer_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_dirty = self.is_story_dirty(cx);

        v_flex()
            .gap_4()
            .child(self.render_prompt_toolbar(
                "prompts-story",
                "Customize the functional guidelines and JSON field instructions sent to Copilot when drafting user stories. Leave a field empty to use its default prompt.",
                is_dirty,
                self.is_checking_story,
                |this, window, cx| this.check_story_prompt(window, cx),
                |this, window, cx| this.reset_story_defaults(window, cx),
                |this, _, cx| this.save_story_prompts(true, cx),
                &theme,
                cx,
            ))
            .when_some(self.render_feedback_row(self.feedback.clone(), &theme), |col, banner| {
                col.child(banner)
            })
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(render_prompt_field(
                        "General Instructions",
                        "Inserted right after the main goal description to influence overall story generation behavior.",
                        &self.story_general_input,
                        92.0,
                        &theme,
                    ))
                    .child(div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("FIELD CUSTOMIZATION"),
                    )
                    .child(render_prompt_field(
                        "Title",
                        "Instruct how to generate each user story title.",
                        &self.story_title_input,
                        72.0,
                        &theme,
                    ))
                    .child(render_prompt_field(
                        "Description",
                        "Define format constraints or detail levels for story descriptions.",
                        &self.story_description_input,
                        92.0,
                        &theme,
                    ))
                    .child(render_prompt_field(
                        "Acceptance Criteria",
                        "Specify formatting rules for the acceptance criteria checklist.",
                        &self.story_ac_input,
                        84.0,
                        &theme,
                    ))
                    .child(render_prompt_field(
                        "Notes",
                        "Instruct what extra warnings, assumptions, or notes to include.",
                        &self.story_notes_input,
                        72.0,
                        &theme,
                    )),
            )
    }

    fn render_test_case_writer_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_dirty = self.is_tc_dirty(cx);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);

        v_flex()
            .gap_4()
            .child(self.render_prompt_toolbar(
                "prompts-tc",
                "Customize the guidelines and JSON field instructions sent to Copilot when generating test cases. Leave a field empty to use its default prompt.",
                is_dirty,
                self.is_checking_tc,
                |this, window, cx| this.check_tc_prompt(window, cx),
                |this, window, cx| this.reset_tc_defaults(window, cx),
                |this, _, cx| this.save_tc_prompts(true, cx),
                &theme,
                cx,
            ))
            .when_some(self.render_feedback_row(self.feedback.clone(), &theme), |col, banner| {
                col.child(banner)
            })
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(render_prompt_field(
                        "General Instructions",
                        "Inserted right after the main goal description to influence overall test generation behavior.",
                        &self.tc_general_input,
                        92.0,
                        &theme,
                    ))
                    .child(div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("FIELD CUSTOMIZATION"),
                    )
                    .child(render_prompt_field(
                        "Description",
                        "Define scenario structure requirements.",
                        &self.tc_description_input,
                        72.0,
                        &theme,
                    ))
                    .child(render_prompt_field(
                        "Preconditions",
                        "Instruct how to detail environment states or prerequisites.",
                        &self.tc_preconditions_input,
                        72.0,
                        &theme,
                    ))
                    .child(render_prompt_field(
                        "Steps",
                        "Customize the format of steps (e.g. Gherkin Given/When/Then, numbered list).",
                        &self.tc_steps_input,
                        84.0,
                        &theme,
                    ))
                    .child(render_prompt_field(
                        "Expected Result",
                        "Specify detail requirements for validation expectations.",
                        &self.tc_expected_result_input,
                        72.0,
                        &theme,
                    ))
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.foreground)
                                            .child("Priority"),
                                    )
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .gap_1()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(amber.opacity(0.4))
                                            .bg(amber.opacity(0.1))
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(amber)
                                            .child(Icon::new(LucideIcon::Lock).xsmall())
                                            .child("System Required"),
                                    ),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.secondary.opacity(0.5))
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(SYSTEM_TC_PRIORITY_PROMPT),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("This field is fixed to match downstream categorization logic and cannot be customized."),
                            ),
                    ),
            )
    }

    fn current_elaborator_prompts(&self, cx: &App) -> StoryElaboratorPrompts {
        StoryElaboratorPrompts {
            general: self
                .elaborator_general_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
        }
    }

    fn is_elaborator_dirty(&self, cx: &App) -> bool {
        let current = self.current_elaborator_prompts(cx);
        let saved = &self.app_state.read(cx).settings.prompts.story_elaborator;
        &current != saved
    }

    fn save_elaborator_prompts(&mut self, show_saved_banner: bool, cx: &mut Context<Self>) {
        let next = self.current_elaborator_prompts(cx);
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.story_elaborator = next;
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        self.apply_save_outcome(
            res,
            show_saved_banner.then_some("Story Elaborator custom prompt saved."),
            "Failed to save custom prompt",
            cx,
        );
    }

    fn reset_elaborator_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.elaborator_general_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });

        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.story_elaborator = StoryElaboratorPrompts::default();
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });

        self.apply_save_outcome(
            res,
            Some("Story Elaborator prompt reset to default."),
            "Failed to reset custom prompt",
            cx,
        );
    }

    fn render_story_elaborator_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_dirty = self.is_elaborator_dirty(cx);

        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Customize the guidelines and instructions sent to Copilot when elaborating user stories."),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_wrap()
                    .w_full()
                    .justify_end()
                            .child(
                                Button::new("prompts-elaborator-reset")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RotateCcw).small())
                                    .label("Reset Defaults")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_elaborator_defaults(window, cx);
                                    })),
                            )
                            .child({
                                let btn = Button::new("prompts-elaborator-save")
                                    .small()
                                    .disabled(!is_dirty)
                                    .icon(Icon::new(LucideIcon::Save).small())
                                    .label("Save Changes")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_elaborator_prompts(true, cx);
                                    }));
                                if is_dirty { btn.primary() } else { btn.ghost() }
                            }),
                    ),
            )
            .when_some(self.render_feedback_row(self.feedback.clone(), &theme), |col, banner| {
                col.child(banner)
            })
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(render_prompt_field(
                        "General Instructions",
                        "Appended to the Story Elaborator system instructions to steer questioning style and plan structure.",
                        &self.elaborator_general_input,
                        120.0,
                        &theme,
                    )),
            )
    }

    fn current_explainer_prompts(&self, cx: &App) -> CodeExplainerPrompts {
        CodeExplainerPrompts {
            general: self
                .explainer_general_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
        }
    }

    fn is_explainer_dirty(&self, cx: &App) -> bool {
        let current = self.current_explainer_prompts(cx);
        let saved = &self.app_state.read(cx).settings.prompts.code_explainer;
        &current != saved
    }

    fn save_explainer_prompts(&mut self, show_saved_banner: bool, cx: &mut Context<Self>) {
        let next = self.current_explainer_prompts(cx);
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.code_explainer = next;
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_save_outcome(
            res,
            show_saved_banner.then_some("Code Explainer custom prompt saved."),
            "Failed to save custom prompt",
            cx,
        );
    }

    fn reset_explainer_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.explainer_general_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.code_explainer = CodeExplainerPrompts::default();
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_save_outcome(
            res,
            Some("Code Explainer prompt reset to default."),
            "Failed to reset custom prompt",
            cx,
        );
    }

    fn render_code_explainer_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_dirty = self.is_explainer_dirty(cx);

        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Customize the guidelines and instructions sent to Copilot when documenting a part of a project."),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_wrap()
                            .w_full()
                            .justify_end()
                            .child(
                                Button::new("prompts-explainer-reset")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RotateCcw).small())
                                    .label("Reset Defaults")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_explainer_defaults(window, cx);
                                    })),
                            )
                            .child({
                                let btn = Button::new("prompts-explainer-save")
                                    .small()
                                    .disabled(!is_dirty)
                                    .icon(Icon::new(LucideIcon::Save).small())
                                    .label("Save Changes")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_explainer_prompts(true, cx);
                                    }));
                                if is_dirty { btn.primary() } else { btn.ghost() }
                            }),
                    ),
            )
            .when_some(self.render_feedback_row(self.feedback.clone(), &theme), |col, banner| {
                col.child(banner)
            })
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(render_prompt_field(
                        "General Instructions",
                        "Appended to the Code Explainer system instructions to steer investigation focus, audience, and documentation style.",
                        &self.explainer_general_input,
                        120.0,
                        &theme,
                    )),
            )
    }

    fn current_solution_designer_prompts(&self, cx: &App) -> SolutionDesignerPrompts {
        SolutionDesignerPrompts {
            general: self
                .solution_designer_general_input
                .read(cx)
                .value()
                .trim()
                .to_string(),
        }
    }

    fn is_solution_designer_dirty(&self, cx: &App) -> bool {
        let current = self.current_solution_designer_prompts(cx);
        let saved = &self.app_state.read(cx).settings.prompts.solution_designer;
        &current != saved
    }

    fn save_solution_designer_prompts(&mut self, show_saved_banner: bool, cx: &mut Context<Self>) {
        let next = self.current_solution_designer_prompts(cx);
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.solution_designer = next;
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_save_outcome(
            res,
            show_saved_banner.then_some("Solution Designer custom prompt saved."),
            "Failed to save custom prompt",
            cx,
        );
    }

    fn reset_solution_designer_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.solution_designer_general_input
            .update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.prompts.solution_designer = SolutionDesignerPrompts::default();
            let save_res = state.save_settings();
            cx.notify();
            save_res
        });
        self.apply_save_outcome(
            res,
            Some("Solution Designer prompt reset to default."),
            "Failed to reset custom prompt",
            cx,
        );
    }

    fn render_solution_designer_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_dirty = self.is_solution_designer_dirty(cx);

        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Customize the guidelines and instructions sent to Copilot during Solution Designer interviews and document generation."),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_wrap()
                            .w_full()
                            .justify_end()
                            .child(
                                Button::new("prompts-sd-reset")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::RotateCcw).small())
                                    .label("Reset Defaults")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_solution_designer_defaults(window, cx);
                                    })),
                            )
                            .child({
                                let btn = Button::new("prompts-sd-save")
                                    .small()
                                    .disabled(!is_dirty)
                                    .icon(Icon::new(LucideIcon::Save).small())
                                    .label("Save Changes")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_solution_designer_prompts(true, cx);
                                    }));
                                if is_dirty { btn.primary() } else { btn.ghost() }
                            }),
                    ),
            )
            .when_some(self.render_feedback_row(self.feedback.clone(), &theme), |col, banner| {
                col.child(banner)
            })
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(render_prompt_field(
                        "General Instructions",
                        "Appended to Solution Designer system instructions to steer interview style, architectural preferences, and Markdown structure.",
                        &self.solution_designer_general_input,
                        120.0,
                        &theme,
                    )),
            )
    }
}

impl Render for PromptsSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let body = match self.sub_tab {
            PromptsSubTab::StoryWriter => self.render_story_writer_panel(cx).into_any_element(),
            PromptsSubTab::TestCaseWriter => {
                self.render_test_case_writer_panel(cx).into_any_element()
            }
            PromptsSubTab::StoryElaborator => {
                self.render_story_elaborator_panel(cx).into_any_element()
            }
            PromptsSubTab::CodeExplainer => {
                self.render_code_explainer_panel(cx).into_any_element()
            }
            PromptsSubTab::SolutionDesigner => {
                self.render_solution_designer_panel(cx).into_any_element()
            }
        };

        v_flex()
            .gap_5()
            .max_w(px(820.))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Prompts & Complexity Validator"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Override default AI instructions per tool and validate custom prompt statements against JSONL output rules."),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .p_1()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .children(PromptsSubTab::ALL.iter().copied().map(|tab| {
                        let active = self.sub_tab == tab;
                        let btn = Button::new(tab.id())
                            .small()
                            .icon(Icon::new(tab.icon()).small())
                            .label(tab.label())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.sub_tab != tab {
                                    this.sub_tab = tab;
                                    this.feedback = None;
                                    cx.notify();
                                }
                            }));
                        if active { btn.primary() } else { btn.ghost() }
                    })),
            )
            .child(body)
    }
}

struct PromptValidationModalView {
    tool_label: &'static str,
    is_loading: bool,
    outcome: Option<Result<PromptValidationResult, String>>,
}

impl PromptValidationModalView {
    fn loading(tool_label: &'static str) -> Self {
        Self {
            tool_label,
            is_loading: true,
            outcome: None,
        }
    }

    fn with_outcome(
        tool_label: &'static str,
        outcome: Result<PromptValidationResult, String>,
    ) -> Self {
        Self {
            tool_label,
            is_loading: false,
            outcome: Some(outcome),
        }
    }

    fn set_outcome(
        &mut self,
        outcome: Result<PromptValidationResult, String>,
        cx: &mut Context<Self>,
    ) {
        self.is_loading = false;
        self.outcome = Some(outcome);
        cx.notify();
    }
}

impl Render for PromptValidationModalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        if self.is_loading {
            return v_flex()
                .py_8()
                .gap_3()
                .items_center()
                .justify_center()
                .child(Spinner::new().large().color(theme.primary))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child("Analyzing prompt structure & complexity..."),
                )
                .into_any_element();
        }

        match &self.outcome {
            Some(Ok(result)) => {
                let (badge_color, icon, headline) = match result.verdict {
                    ValidationVerdict::Pass => {
                        (green, LucideIcon::Check, "Prompt Validation Passed")
                    }
                    ValidationVerdict::Fail => (
                        amber,
                        LucideIcon::TriangleAlert,
                        "Potential Formatting Risks Detected",
                    ),
                    ValidationVerdict::Neutral => {
                        (theme.primary, LucideIcon::Info, "Prompt Analysis Feedback")
                    }
                };

                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .p_3()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(badge_color.opacity(0.4))
                            .bg(badge_color.opacity(0.08))
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .size(px(28.))
                                            .rounded(theme.radius)
                                            .bg(badge_color.opacity(0.18))
                                            .text_color(badge_color)
                                            .child(Icon::new(icon).small()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.foreground)
                                            .child(headline),
                                    ),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(theme.radius)
                                    .bg(theme.secondary)
                                    .border_1()
                                    .border_color(theme.border)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(self.tool_label),
                            ),
                    )
                    .child(
                        div()
                            .max_h(px(400.))
                            .overflow_y_scrollbar()
                            .p_4()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                MarkdownView::new(
                                    "prompt-validation-md",
                                    result.markdown_body.clone(),
                                )
                                .selectable(true),
                            ),
                    )
                    .into_any_element()
            }
            Some(Err(err)) => h_flex()
                .p_4()
                .gap_2p5()
                .items_start()
                .rounded(theme.radius)
                .border_1()
                .border_color(red.opacity(0.4))
                .bg(red.opacity(0.08))
                .text_sm()
                .text_color(red)
                .child(Icon::new(LucideIcon::CircleAlert).with_size(Size::Medium))
                .child(div().flex_1().child(err.clone()))
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

fn open_validation_dialog(
    modal: Entity<PromptValidationModalView>,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _window, _cx| {
        dialog
            .w(px(680.))
            .overlay_closable(true)
            .close_button(true)
            .title(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::new(LucideIcon::ShieldCheck).with_size(Size::Medium))
                    .child("Prompt Analysis Feedback"),
            )
            .child(modal.clone())
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .child(
                        Button::new("prompt-validation-close")
                            .primary()
                            .label("Close")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

fn render_prompt_field(
    label: &'static str,
    hint: &'static str,
    state: &Entity<TextareaState>,
    height_px: f32,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    v_flex()
        .gap_1p5()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(label),
        )
        .child(Textarea::new(state).h(px(height_px)))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(hint),
        )
}
