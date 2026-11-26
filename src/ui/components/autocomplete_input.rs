use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString, Styled as _, Subscription,
    Window, div, hsla, prelude::FluentBuilder as _, px,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectIdMode {
    /// Only all-digit strings (e.g. Confluence page IDs or Azure DevOps work item IDs).
    NumericOnly,
    /// All-digit strings or `repo/123` / `owner/repo/123` (GitHub issue identifiers).
    NumericOrRepoIssue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteOption {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AutocompleteEvent {
    SearchRequested { query: String, request_id: u64 },
    Selected(AutocompleteOption),
    DirectValueChanged(Option<String>),
    Cleared,
}

/// Parses a user-typed query to see if it already represents a direct ticket or page ID.
pub fn parse_direct_id(raw: &str, mode: DirectIdMode) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let is_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());

    match mode {
        DirectIdMode::NumericOnly => {
            if is_digits(trimmed) {
                Some(trimmed.to_string())
            } else {
                None
            }
        }
        DirectIdMode::NumericOrRepoIssue => {
            if is_digits(trimmed) {
                return Some(trimmed.to_string());
            }
            let parts: Vec<&str> = trimmed.split('/').collect();
            let valid_slug = |s: &str| !s.is_empty() && !s.chars().any(char::is_whitespace);
            match parts.as_slice() {
                [repo, num] if valid_slug(repo) && is_digits(num) => Some(trimmed.to_string()),
                [owner, repo, num] if valid_slug(owner) && valid_slug(repo) && is_digits(num) => {
                    Some(trimmed.to_string())
                }
                _ => None,
            }
        }
    }
}

/// Reusable debounced (400ms) autocomplete search combobox backed by GPUI's `InputState`.
pub struct AutocompleteInput {
    id_prefix: SharedString,
    input_state: Entity<InputState>,
    direct_id_mode: DirectIdMode,
    results: Vec<AutocompleteOption>,
    is_searching: bool,
    search_error: Option<String>,
    is_open: bool,
    disabled: bool,
    selected_display: Option<String>,
    debounce_gen: u64,
    active_request_id: u64,
    empty_label: SharedString,
    searching_label: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AutocompleteEvent> for AutocompleteInput {}

impl AutocompleteInput {
    pub fn new(
        id_prefix: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        direct_id_mode: DirectIdMode,
        empty_label: impl Into<SharedString>,
        searching_label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder_str = placeholder.into();
        let input_state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder_str));

        let input_sub = cx.subscribe_in(&input_state, window, |this, _, event, window, cx| {
            match event {
                InputEvent::Change => this.handle_input_change(cx),
                InputEvent::Focus => {
                    let current = this.input_state.read(cx).value().trim().to_string();
                    if !current.is_empty()
                        && this.selected_display.as_deref() != Some(current.as_str())
                    {
                        this.is_open = true;
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => {
                    let current = this.input_state.read(cx).value().trim().to_string();
                    if this.is_open
                        && !this.results.is_empty()
                        && parse_direct_id(&current, this.direct_id_mode).is_none()
                    {
                        let first = this.results[0].clone();
                        this.select_option(first, window, cx);
                    } else if this.is_open {
                        this.is_open = false;
                        cx.notify();
                    }
                }
                InputEvent::Blur => {}
            }
        });

        Self {
            id_prefix: id_prefix.into(),
            input_state,
            direct_id_mode,
            results: Vec::new(),
            is_searching: false,
            search_error: None,
            is_open: false,
            disabled: false,
            selected_display: None,
            debounce_gen: 0,
            active_request_id: 0,
            empty_label: empty_label.into(),
            searching_label: searching_label.into(),
            _subscriptions: vec![input_sub],
        }
    }

    pub fn set_direct_id_mode(&mut self, mode: DirectIdMode, cx: &mut Context<Self>) {
        if self.direct_id_mode != mode {
            self.direct_id_mode = mode;
            cx.notify();
        }
    }

    pub fn has_value(&self, cx: &App) -> bool {
        !self.input_state.read(cx).value().trim().is_empty()
            || self.selected_display.is_some()
            || self.is_open
            || self.is_searching
            || !self.results.is_empty()
            || self.search_error.is_some()
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debounce_gen = self.debounce_gen.wrapping_add(1);
        self.active_request_id = self.active_request_id.wrapping_add(1);
        self.selected_display = None;
        self.results.clear();
        self.search_error = None;
        self.is_searching = false;
        self.is_open = false;
        self.disabled = false;
        self.input_state.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        cx.notify();
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            if disabled {
                self.is_open = false;
                self.is_searching = false;
                self.debounce_gen = self.debounce_gen.wrapping_add(1);
            }
            cx.notify();
        }
    }

    pub fn set_results(
        &mut self,
        request_id: u64,
        res: Result<Vec<AutocompleteOption>, String>,
        cx: &mut Context<Self>,
    ) {
        if request_id != self.active_request_id {
            return;
        }
        self.is_searching = false;
        match res {
            Ok(options) => {
                self.results = options;
                self.search_error = None;
            }
            Err(err) => {
                self.results.clear();
                self.search_error = Some(err);
            }
        }
        cx.notify();
    }

    pub fn select_option(
        &mut self,
        option: AutocompleteOption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let display = format!("#{} - {}", option.id, option.title);
        self.selected_display = Some(display.clone());
        self.is_open = false;
        self.is_searching = false;
        self.search_error = None;
        self.debounce_gen = self.debounce_gen.wrapping_add(1);
        self.active_request_id = self.active_request_id.wrapping_add(1);
        self.input_state.update(cx, |input, cx| {
            input.set_value(display, window, cx);
        });
        cx.emit(AutocompleteEvent::Selected(option));
        cx.notify();
    }

    fn handle_input_change(&mut self, cx: &mut Context<Self>) {
        let trimmed = self.input_state.read(cx).value().trim().to_string();
        if self.selected_display.as_deref() == Some(trimmed.as_str()) && self.selected_display.is_some()
        {
            return;
        }

        self.selected_display = None;
        cx.emit(AutocompleteEvent::DirectValueChanged(parse_direct_id(
            &trimmed,
            self.direct_id_mode,
        )));

        if trimmed.is_empty() {
            self.debounce_gen = self.debounce_gen.wrapping_add(1);
            self.active_request_id = self.active_request_id.wrapping_add(1);
            self.results.clear();
            self.search_error = None;
            self.is_searching = false;
            self.is_open = false;
            cx.emit(AutocompleteEvent::Cleared);
            cx.notify();
            return;
        }

        self.is_open = true;
        self.is_searching = true;
        self.search_error = None;
        self.debounce_gen = self.debounce_gen.wrapping_add(1);
        let generation = self.debounce_gen;
        let query = trimmed;

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.debounce_gen != generation || this.disabled {
                    return;
                }
                this.active_request_id = this.active_request_id.wrapping_add(1);
                let request_id = this.active_request_id;
                cx.emit(AutocompleteEvent::SearchRequested { query, request_id });
                cx.notify();
            });
        })
        .detach();

        cx.notify();
    }
}

impl Render for AutocompleteInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let green = hsla(152.0 / 360.0, 0.76, 0.40, 1.0);
        let red = hsla(0.0, 0.78, 0.55, 1.0);

        let prefix_icon = if self.is_searching {
            Spinner::new()
                .xsmall()
                .color(theme.primary)
                .into_any_element()
        } else {
            Icon::new(LucideIcon::Search)
                .xsmall()
                .text_color(theme.muted_foreground)
                .into_any_element()
        };

        let result_count = self.results.len();
        let close_btn_id: SharedString = format!("{}-close", self.id_prefix).into();
        let prefix = self.id_prefix.clone();

        v_flex()
            .w_full()
            .gap_1()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.is_open {
                    this.is_open = false;
                    cx.notify();
                }
            }))
            .child(
                Input::new(&self.input_state)
                    .disabled(self.disabled)
                    .cleanable(true)
                    .prefix(prefix_icon),
            )
            .when(self.is_open && !self.disabled, |col| {
                col.child(
                    v_flex()
                        .w_full()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .px_2p5()
                                .py_1p5()
                                .bg(theme.secondary)
                                .border_b_1()
                                .border_color(theme.border)
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(if self.is_searching {
                                            self.searching_label.clone()
                                        } else if self.search_error.is_some() {
                                            "Search error".into()
                                        } else if result_count == 1 {
                                            "1 match".into()
                                        } else {
                                            format!("{result_count} matches").into()
                                        }),
                                )
                                .child(
                                    Button::new(close_btn_id)
                                        .ghost()
                                        .xsmall()
                                        .icon(Icon::new(LucideIcon::X).xsmall())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.is_open = false;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(if self.is_searching {
                            h_flex()
                                .py_4()
                                .px_3()
                                .justify_center()
                                .items_center()
                                .gap_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(Spinner::new().xsmall())
                                .child(self.searching_label.clone())
                                .into_any_element()
                        } else if let Some(err) = &self.search_error {
                            h_flex()
                                .p_3()
                                .gap_2()
                                .items_start()
                                .text_xs()
                                .text_color(red)
                                .child(Icon::new(LucideIcon::CircleAlert).xsmall())
                                .child(div().flex_1().child(err.clone()))
                                .into_any_element()
                        } else if self.results.is_empty() {
                            div()
                                .py_4()
                                .px_3()
                                .text_center()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(self.empty_label.clone())
                                .into_any_element()
                        } else {
                            v_flex()
                                .w_full()
                                .max_h(px(220.))
                                .overflow_y_scrollbar()
                                .children(self.results.iter().enumerate().map(|(idx, opt)| {
                                    let option_for_click = opt.clone();
                                    let row_id: SharedString =
                                        format!("{}-opt-{idx}", prefix).into();
                                    let subtitle = opt.subtitle.clone();
                                    v_flex()
                                        .id(row_id)
                                        .w_full()
                                        .px_3()
                                        .py_2()
                                        .gap_0p5()
                                        .cursor_pointer()
                                        .when(idx > 0, |row| {
                                            row.border_t_1().border_color(theme.border)
                                        })
                                        .hover(|s| s.bg(theme.secondary_hover))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.select_option(
                                                    option_for_click.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        )
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(green)
                                                        .child(format!("#{}", opt.id)),
                                                )
                                                .when_some(subtitle, |row, sub| {
                                                    row.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(theme.muted_foreground)
                                                            .truncate()
                                                            .child(sub),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme.foreground)
                                                .child(opt.title.clone()),
                                        )
                                }))
                                .into_any_element()
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_id_numeric_only() {
        assert_eq!(
            parse_direct_id("  12345 ", DirectIdMode::NumericOnly),
            Some("12345".to_string())
        );
        assert_eq!(parse_direct_id("", DirectIdMode::NumericOnly), None);
        assert_eq!(parse_direct_id("12a", DirectIdMode::NumericOnly), None);
        assert_eq!(
            parse_direct_id("repo/42", DirectIdMode::NumericOnly),
            None
        );
        assert_eq!(
            parse_direct_id("#12345 - Title", DirectIdMode::NumericOnly),
            None
        );
    }

    #[test]
    fn parse_direct_id_numeric_or_repo_issue() {
        assert_eq!(
            parse_direct_id("42", DirectIdMode::NumericOrRepoIssue),
            Some("42".to_string())
        );
        assert_eq!(
            parse_direct_id("stitch-rs/42", DirectIdMode::NumericOrRepoIssue),
            Some("stitch-rs/42".to_string())
        );
        assert_eq!(
            parse_direct_id("acme/stitch-rs/42", DirectIdMode::NumericOrRepoIssue),
            Some("acme/stitch-rs/42".to_string())
        );
        assert_eq!(
            parse_direct_id("stitch-rs/abc", DirectIdMode::NumericOrRepoIssue),
            None
        );
        assert_eq!(
            parse_direct_id("auth bug", DirectIdMode::NumericOrRepoIssue),
            None
        );
        assert_eq!(
            parse_direct_id("a/b/c/42", DirectIdMode::NumericOrRepoIssue),
            None
        );
    }
}
