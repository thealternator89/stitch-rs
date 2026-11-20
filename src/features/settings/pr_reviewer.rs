use std::collections::HashSet;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, Size, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Window, div, hsla, prelude::FluentBuilder as _, px,
};

use crate::app_state::AppState;
use crate::types::{
    AppSettings, ReviewPersona, max_parallelism_limit, resolve_effective_parallelism,
    system_cpu_count,
};

pub const MAX_PERSONAS: usize = 5;
pub const MAX_PERSONA_NAME_LEN: usize = 20;
pub const MAX_PERSONA_GUIDELINES_LEN: usize = 160;

#[derive(Debug, Clone)]
enum PrSettingsFeedback {
    Saved(String),
    Error(String),
}

struct PersonaRow {
    uid: u64,
    name_input: Entity<InputState>,
    guidelines_input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

/// Validates and normalizes up to 5 PR Reviewer personas.
pub fn validate_personas(personas: &[ReviewPersona]) -> Result<Vec<ReviewPersona>, String> {
    if personas.len() > MAX_PERSONAS {
        return Err(format!("You can configure up to {MAX_PERSONAS} personas."));
    }

    let mut seen_names = HashSet::new();
    let mut sanitized = Vec::with_capacity(personas.len());

    for (idx, persona) in personas.iter().enumerate() {
        let name = persona.name.trim().to_string();
        if name.is_empty() {
            return Err(format!("Persona #{} has an empty name.", idx + 1));
        }
        if name.eq_ignore_ascii_case("none") {
            return Err("\"None\" is a reserved name for personas.".to_string());
        }
        if name.chars().count() > MAX_PERSONA_NAME_LEN {
            return Err(format!(
                "Persona \"{name}\" name exceeds {MAX_PERSONA_NAME_LEN} characters."
            ));
        }
        if !seen_names.insert(name.to_ascii_lowercase()) {
            return Err(format!(
                "Persona name \"{name}\" is a duplicate. Names must be unique."
            ));
        }

        let guidelines = persona
            .guidelines
            .replace(['\r', '\n'], " ")
            .trim()
            .to_string();
        if guidelines.is_empty() {
            return Err(format!("Persona \"{name}\" has empty guidelines."));
        }
        if guidelines.chars().count() > MAX_PERSONA_GUIDELINES_LEN {
            return Err(format!(
                "Persona \"{name}\" guidelines exceed {MAX_PERSONA_GUIDELINES_LEN} characters."
            ));
        }

        sanitized.push(ReviewPersona { name, guidelines });
    }

    Ok(sanitized)
}

/// Settings > PR Reviewer tab: Personas editor (up to 5) and Parallelism slider.
pub struct PrReviewerSettingsView {
    app_state: Entity<AppState>,
    rows: Vec<PersonaRow>,
    next_uid: u64,
    cpu_count: usize,
    max_parallelism: usize,
    parallelism_slider: Entity<SliderState>,
    feedback: Option<PrSettingsFeedback>,
    _subscriptions: Vec<Subscription>,
}

impl PrReviewerSettingsView {
    pub fn new(
        app_state: Entity<AppState>,
        initial_settings: &AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cpu_count = system_cpu_count();
        let max_limit = max_parallelism_limit(cpu_count);
        let initial_parallelism =
            resolve_effective_parallelism(None, initial_settings.max_parallelism, cpu_count);

        let parallelism_slider = cx.new(|_| {
            SliderState::new()
                .min(1.0)
                .max(max_limit.max(2) as f32)
                .step(1.0)
                .default_value(initial_parallelism as f32)
        });

        let slider_sub = cx.subscribe(&parallelism_slider, move |this, _, event, cx| {
            let max_allowed = max_parallelism_limit(this.cpu_count);
            match event {
                SliderEvent::Change(val) => {
                    let next = (val.start().round() as usize).clamp(1, max_allowed);
                    if this.max_parallelism != next {
                        this.max_parallelism = next;
                        cx.notify();
                    }
                }
                SliderEvent::Release(val) => {
                    let next = (val.start().round() as usize).clamp(1, max_allowed);
                    this.max_parallelism = next;
                    this.persist_parallelism(next, cx);
                }
            }
        });

        let mut next_uid = 1;
        let mut rows = Vec::new();
        for persona in initial_settings.pr_reviewer.personas.iter().take(MAX_PERSONAS) {
            rows.push(Self::build_row(
                next_uid,
                persona.name.clone(),
                persona.guidelines.clone(),
                window,
                cx,
            ));
            next_uid += 1;
        }

        Self {
            app_state,
            rows,
            next_uid,
            cpu_count,
            max_parallelism: initial_parallelism,
            parallelism_slider,
            feedback: None,
            _subscriptions: vec![slider_sub],
        }
    }

    fn build_row(
        uid: u64,
        name: String,
        guidelines: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PersonaRow {
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Persona Name")
                .default_value(name)
        });
        let guidelines_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. Focus on SQL injection, XSS, and authorization checks.")
                .default_value(guidelines)
        });

        let name_sub = cx.subscribe(&name_input, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.feedback = None;
                cx.notify();
            }
        });
        let guide_sub = cx.subscribe(&guidelines_input, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.feedback = None;
                cx.notify();
            }
        });

        PersonaRow {
            uid,
            name_input,
            guidelines_input,
            _subs: vec![name_sub, guide_sub],
        }
    }

    fn persist_parallelism(&mut self, value: usize, cx: &mut Context<Self>) {
        let res = self.app_state.update(cx, |state, cx| {
            state.settings.max_parallelism = Some(value);
            let out = state.save_settings();
            cx.notify();
            out
        });
        match res {
            Ok(()) => {
                self.feedback = Some(PrSettingsFeedback::Saved(format!(
                    "Default parallelism saved ({value} worker{}).",
                    if value == 1 { "" } else { "s" }
                )));
            }
            Err(err) => {
                self.feedback = Some(PrSettingsFeedback::Error(format!(
                    "Failed to save parallelism: {err}"
                )));
            }
        }
        cx.notify();
    }

    fn add_persona(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.len() >= MAX_PERSONAS {
            return;
        }
        let uid = self.next_uid;
        self.next_uid += 1;
        self.rows
            .push(Self::build_row(uid, String::new(), String::new(), window, cx));
        self.feedback = None;
        cx.notify();
    }

    fn delete_persona(&mut self, uid: u64, cx: &mut Context<Self>) {
        self.rows.retain(|r| r.uid != uid);
        self.feedback = None;
        cx.notify();
    }

    fn collect_personas(&self, cx: &App) -> Vec<ReviewPersona> {
        self.rows
            .iter()
            .map(|row| ReviewPersona {
                name: row.name_input.read(cx).value().to_string(),
                guidelines: row
                    .guidelines_input
                    .read(cx)
                    .value()
                    .replace(['\r', '\n'], " "),
            })
            .collect()
    }

    fn save_personas(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.collect_personas(cx);
        let validated = match validate_personas(&raw) {
            Ok(v) => v,
            Err(err) => {
                self.feedback = Some(PrSettingsFeedback::Error(err));
                cx.notify();
                return;
            }
        };

        for (row, clean) in self.rows.iter().zip(validated.iter()) {
            row.name_input.update(cx, |input, cx| {
                if input.value() != clean.name {
                    input.set_value(clean.name.clone(), window, cx);
                }
            });
            row.guidelines_input.update(cx, |input, cx| {
                if input.value() != clean.guidelines {
                    input.set_value(clean.guidelines.clone(), window, cx);
                }
            });
        }

        let res = self.app_state.update(cx, |state, cx| {
            state.settings.pr_reviewer.personas = validated;
            state.settings.max_parallelism = Some(self.max_parallelism);
            let out = state.save_settings();
            cx.notify();
            out
        });

        match res {
            Ok(()) => {
                self.feedback = Some(PrSettingsFeedback::Saved(
                    "PR Reviewer settings saved.".to_string(),
                ));
            }
            Err(err) => {
                self.feedback = Some(PrSettingsFeedback::Error(format!(
                    "Failed to save personas: {err}"
                )));
            }
        }
        cx.notify();
    }
}

impl Render for PrReviewerSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let blue = hsla(212.0 / 360.0, 0.88, 0.56, 1.0);

        let current_personas = self.collect_personas(cx);
        let is_personas_dirty = {
            let saved = &self.app_state.read(cx).settings.pr_reviewer.personas;
            &current_personas != saved
        };
        let row_uids: Vec<u64> = self.rows.iter().map(|r| r.uid).collect();
        let max_limit = max_parallelism_limit(self.cpu_count);

        v_flex()
            .gap_5()
            .max_w(px(760.))
            .when_some(self.feedback.clone(), |col, fb| {
                let (is_err, msg) = match fb {
                    PrSettingsFeedback::Saved(m) => (false, m),
                    PrSettingsFeedback::Error(m) => (true, m),
                };
                let accent = if is_err { red } else { green };
                let icon = if is_err {
                    LucideIcon::CircleAlert
                } else {
                    LucideIcon::CircleCheck
                };
                col.child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .items_center()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(accent.opacity(0.4))
                        .bg(accent.opacity(0.08))
                        .text_xs()
                        .text_color(accent)
                        .child(Icon::new(icon).small())
                        .child(div().flex_1().child(msg)),
                )
            })
            // Personas Card
            .child(
                v_flex()
                    .p_5()
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .pb_3()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(Icon::new(LucideIcon::Users).with_size(Size::Medium))
                                    .child("Personas"),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .when(is_personas_dirty, |row| {
                                        row.child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(theme.radius)
                                                .border_1()
                                                .border_color(theme.primary.opacity(0.4))
                                                .bg(theme.primary.opacity(0.1))
                                                .text_xs()
                                                .text_color(theme.primary)
                                                .child("Unsaved"),
                                        )
                                    })
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(theme.radius)
                                            .border_1()
                                            .border_color(theme.border)
                                            .bg(theme.secondary)
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("{} / {MAX_PERSONAS} Personas", self.rows.len())),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("You can use personas to customise how your agents talk and what they focus on."),
                    )
                    .child(if self.rows.is_empty() {
                        v_flex()
                            .items_center()
                            .justify_center()
                            .py_6()
                            .px_4()
                            .gap_2()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(
                                Icon::new(LucideIcon::User)
                                    .with_size(Size::Large)
                                    .text_color(theme.muted_foreground),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("No Personas Configured"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Add a persona to start customizing review perspectives."),
                            )
                            .child(
                                div().pt_1().child(
                                    Button::new("settings-add-first-persona")
                                        .primary()
                                        .small()
                                        .icon(Icon::new(LucideIcon::Plus).small())
                                        .label("Add Persona")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.add_persona(window, cx);
                                        })),
                                ),
                            )
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_3()
                            .children(self.rows.iter().enumerate().map(|(index, row)| {
                                let uid = row_uids[index];
                                let snapshot = &current_personas[index];
                                let trimmed_name = snapshot.name.trim();
                                let trimmed_guide = snapshot.guidelines.trim();
                                let is_dup = !trimmed_name.is_empty()
                                    && current_personas.iter().enumerate().any(|(other_idx, other)| {
                                        other_idx != index
                                            && other.name.trim().eq_ignore_ascii_case(trimmed_name)
                                    });
                                let is_none = trimmed_name.eq_ignore_ascii_case("none");
                                let name_len = snapshot.name.chars().count();
                                let guide_len = snapshot.guidelines.chars().count();

                                let name_error = if trimmed_name.is_empty() {
                                    Some("Name is required.")
                                } else if is_dup {
                                    Some("Persona name must be unique.")
                                } else if is_none {
                                    Some("\"None\" is a reserved name.")
                                } else if name_len > MAX_PERSONA_NAME_LEN {
                                    Some("Name exceeds 20 characters.")
                                } else {
                                    None
                                };
                                let guide_error = if trimmed_guide.is_empty() {
                                    Some("Guidelines are required.")
                                } else if guide_len > MAX_PERSONA_GUIDELINES_LEN {
                                    Some("Guidelines exceed 160 characters.")
                                } else {
                                    None
                                };

                                v_flex()
                                    .p_3p5()
                                    .gap_3()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(if name_error.is_some() || guide_error.is_some() {
                                        red.opacity(0.45)
                                    } else {
                                        theme.border
                                    })
                                    .bg(theme.background)
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(div().flex_1().child(Input::new(&row.name_input)))
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_family(theme.mono_font_family.clone())
                                                            .text_color(if name_len > MAX_PERSONA_NAME_LEN {
                                                                red
                                                            } else {
                                                                theme.muted_foreground
                                                            })
                                                            .child(format!("{name_len}/{MAX_PERSONA_NAME_LEN}")),
                                                    )
                                                    .child(
                                                        Button::new(("delete-persona", uid))
                                                            .ghost()
                                                            .small()
                                                            .icon(Icon::new(LucideIcon::Trash).small())
                                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                                this.delete_persona(uid, cx);
                                                            })),
                                                    ),
                                            )
                                            .when_some(name_error, |col, err| {
                                                col.child(div().text_xs().text_color(red).child(err))
                                            }),
                                    )
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_weight(FontWeight::MEDIUM)
                                                            .text_color(theme.muted_foreground)
                                                            .child("Guidelines (Single-line focus)"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_family(theme.mono_font_family.clone())
                                                            .text_color(if guide_len > MAX_PERSONA_GUIDELINES_LEN {
                                                                red
                                                            } else {
                                                                theme.muted_foreground
                                                            })
                                                            .child(format!("{guide_len}/{MAX_PERSONA_GUIDELINES_LEN}")),
                                                    ),
                                            )
                                            .child(Input::new(&row.guidelines_input))
                                            .when_some(guide_error, |col, err| {
                                                col.child(div().text_xs().text_color(red).child(err))
                                            }),
                                    )
                            }))
                            .into_any_element()
                    })
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .pt_1()
                            .child(if self.rows.is_empty() {
                                div().into_any_element()
                            } else if self.rows.len() < MAX_PERSONAS {
                                Button::new("settings-add-persona")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(LucideIcon::Plus).small())
                                    .label("Add Persona")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_persona(window, cx);
                                    }))
                                    .into_any_element()
                            } else {
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(LucideIcon::Info).xsmall())
                                    .child(format!("Maximum limit of {MAX_PERSONAS} personas reached."))
                                    .into_any_element()
                            })
                            .child(
                                Button::new("settings-save-personas")
                                    .primary()
                                    .small()
                                    .icon(Icon::new(LucideIcon::Check).small())
                                    .label("Save Personas")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_personas(window, cx);
                                    })),
                            ),
                    ),
            )
            // Concurrency Settings Card
            .child(
                v_flex()
                    .p_5()
                    .gap_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .pb_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(LucideIcon::Cpu).with_size(Size::Medium))
                            .child("Concurrency Settings"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Default Review Agent Parallelism"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Configure how many parallel GitHub Copilot agents can run review phases concurrently."),
                    )
                    .child(if self.cpu_count < 4 {
                        h_flex()
                            .p_3()
                            .gap_2()
                            .items_center()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(blue.opacity(0.35))
                            .bg(blue.opacity(0.08))
                            .text_xs()
                            .text_color(theme.foreground)
                            .child(Icon::new(LucideIcon::Info).small().text_color(blue))
                            .child(format!(
                                "Parallelism is locked to 1 because this machine has {} CPU cores (fewer than 4 required for parallel agents).",
                                self.cpu_count
                            ))
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .w(px(300.))
                                            .child(Slider::new(&self.parallelism_slider).horizontal()),
                                    )
                                    .child(
                                        div()
                                            .px_2p5()
                                            .py_1()
                                            .rounded(theme.radius)
                                            .bg(theme.primary)
                                            .text_color(theme.primary_foreground)
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(format!(
                                                "{} Worker{}",
                                                self.max_parallelism,
                                                if self.max_parallelism == 1 { "" } else { "s" }
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!(
                                        "Allowing between 1 and {max_limit} parallel agents (Total CPU cores: {}).",
                                        self.cpu_count
                                    )),
                            )
                            .into_any_element()
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_personas_enforces_limits_uniqueness_reserved_name_and_sanitization() {
        let valid = vec![ReviewPersona {
            name: "  Security  ".to_string(),
            guidelines: "Check auth\nand SQL\rboundaries.".to_string(),
        }];
        let sanitized = validate_personas(&valid).expect("valid persona");
        assert_eq!(sanitized[0].name, "Security");
        assert_eq!(sanitized[0].guidelines, "Check auth and SQL boundaries.");

        let empty_name = vec![ReviewPersona {
            name: "   ".to_string(),
            guidelines: "Valid".to_string(),
        }];
        assert!(validate_personas(&empty_name).is_err());

        let reserved = vec![ReviewPersona {
            name: "none".to_string(),
            guidelines: "Valid".to_string(),
        }];
        assert!(validate_personas(&reserved).is_err());

        let dup = vec![
            ReviewPersona {
                name: "Sec".to_string(),
                guidelines: "G1".to_string(),
            },
            ReviewPersona {
                name: "sec".to_string(),
                guidelines: "G2".to_string(),
            },
        ];
        assert!(validate_personas(&dup).is_err());

        let long_name = vec![ReviewPersona {
            name: "a".repeat(21),
            guidelines: "Valid".to_string(),
        }];
        assert!(validate_personas(&long_name).is_err());

        let long_guide = vec![ReviewPersona {
            name: "Perf".to_string(),
            guidelines: "x".repeat(161),
        }];
        assert!(validate_personas(&long_guide).is_err());

        let too_many: Vec<ReviewPersona> = (0..6)
            .map(|i| ReviewPersona {
                name: format!("P{i}"),
                guidelines: "Focus".to_string(),
            })
            .collect();
        assert!(validate_personas(&too_many).is_err());
    }
}
