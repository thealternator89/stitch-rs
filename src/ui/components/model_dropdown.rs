use gpui_kit::component::select::{
    SearchableVec, Select, SelectEvent, SelectItem, SelectState,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, IndexPath, Sizable as _, h_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};

use crate::app_state::AppState;
use crate::types::CopilotModelInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ModelDropdownMode {
    /// Used in Settings > GitHub Copilot ("Auto (Recommended)").
    SettingsDefault,
    /// Used inside individual tool views ("Default (Auto)").
    ToolOverride,
}

#[derive(Debug, Clone)]
pub enum ModelDropdownEvent {
    Changed(String),
}

#[derive(Clone)]
struct ModelOption {
    id: String,
    title: SharedString,
    subtitle: Option<SharedString>,
}

impl SelectItem for ModelOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_2()
            .child(div().truncate().child(self.title.clone()))
            .when_some(self.subtitle.clone(), |this, sub| {
                this.child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(sub),
                )
            })
    }
}

/// Shared searchable Copilot model selector backed by `AppState::copilot_models`.
pub struct ModelDropdown {
    app_state: Entity<AppState>,
    mode: ModelDropdownMode,
    select_state: Entity<SelectState<SearchableVec<ModelOption>>>,
    selected_model: String,
    last_known_models: Vec<CopilotModelInfo>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ModelDropdownEvent> for ModelDropdown {}

impl ModelDropdown {
    pub fn new(
        app_state: Entity<AppState>,
        initial_model: String,
        initial_models: Vec<CopilotModelInfo>,
        mode: ModelDropdownMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let normalized_initial = if initial_model.trim().is_empty() {
            "auto".to_string()
        } else {
            initial_model.trim().to_string()
        };

        let items = Self::build_options(mode, &normalized_initial, &initial_models);
        let selected_row = items
            .iter()
            .position(|opt| opt.id == normalized_initial)
            .unwrap_or(0);

        let select_state = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(items),
                Some(IndexPath::default().row(selected_row)),
                window,
                cx,
            )
            .searchable(true)
        });

        let confirm_sub = cx.subscribe(&select_state, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(model_id)) = event {
                let model_id = model_id.clone();
                if this.selected_model != model_id {
                    this.selected_model = model_id.clone();
                    cx.emit(ModelDropdownEvent::Changed(model_id));
                    cx.notify();
                }
            }
        });

        let state_sub = cx.observe_in(&app_state, window, |this, app_state, window, cx| {
            let models = app_state.read(cx).copilot_models.clone();
            if models != this.last_known_models {
                this.last_known_models = models.clone();
                let options = Self::build_options(this.mode, &this.selected_model, &models);
                let selected = this.selected_model.clone();
                this.select_state.update(cx, |state, cx| {
                    state.set_items(SearchableVec::new(options), window, cx);
                    state.set_selected_value(&selected, window, cx);
                });
                cx.notify();
            }
        });

        Self {
            app_state,
            mode,
            select_state,
            selected_model: normalized_initial,
            last_known_models: initial_models,
            _subscriptions: vec![confirm_sub, state_sub],
        }
    }

    #[allow(dead_code)]
    pub fn set_selected_model(
        &mut self,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let normalized = if model.trim().is_empty() {
            "auto".to_string()
        } else {
            model.trim().to_string()
        };
        if self.selected_model == normalized {
            return;
        }
        self.selected_model = normalized.clone();
        let options = Self::build_options(self.mode, &normalized, &self.last_known_models);
        self.select_state.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(options), window, cx);
            state.set_selected_value(&normalized, window, cx);
        });
        cx.notify();
    }

    fn build_options(
        mode: ModelDropdownMode,
        selected_model: &str,
        models: &[CopilotModelInfo],
    ) -> Vec<ModelOption> {
        let auto_label = match mode {
            ModelDropdownMode::SettingsDefault => "Auto (Recommended)",
            ModelDropdownMode::ToolOverride => "Default (Auto)",
        };

        let mut options = Vec::with_capacity(models.len() + 2);
        options.push(ModelOption {
            id: "auto".to_string(),
            title: auto_label.into(),
            subtitle: Some("Default".into()),
        });

        let mut found_selected = selected_model == "auto";
        for model in models {
            if model.id == "auto" {
                continue;
            }
            if model.id == selected_model {
                found_selected = true;
            }
            options.push(ModelOption {
                id: model.id.clone(),
                title: model.name.clone().into(),
                subtitle: model
                    .billing_multiplier
                    .map(|_| model.multiplier_label().into()),
            });
        }

        // Preserve a previously saved custom model selection while models are still loading or offline.
        if !found_selected && !selected_model.is_empty() {
            options.push(ModelOption {
                id: selected_model.to_string(),
                title: format!("{selected_model} (Saved)").into(),
                subtitle: None,
            });
        }

        options
    }
}

impl Render for ModelDropdown {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_loading = self.app_state.read(cx).models_loading && self.last_known_models.is_empty();

        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                div().flex_1().min_w_0().child(
                    Select::new(&self.select_state)
                        .placeholder("Select Copilot model...")
                        .search_placeholder("Search models..."),
                ),
            )
            .when(is_loading, |this| {
                this.child(Spinner::new().xsmall().color(cx.theme().muted_foreground))
            })
    }
}
