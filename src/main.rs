#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_state;
mod branding;
mod features;
mod infrastructure;
mod types;
mod ui;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    AppContext as _, Bounds, KeyBinding, WindowBounds, WindowOptions, actions, px, size,
};

use crate::app_state::AppState;
use crate::branding::AppAssets;
use crate::ui::WorkspaceShell;

actions!(stitch, [Quit]);

fn main() {
    gpui_kit::application()
        .with_assets(AppAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            branding::set_app_dock_icon();

            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            let bounds = Bounds::centered(None, size(px(1200.), px(890.)), cx);
            let mut titlebar = TitleBar::title_bar_options();
            titlebar.title = Some("Stitch".into());
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.), px(640.))),
                titlebar: Some(titlebar),
                app_id: Some("Stitch".to_string()),
                ..TitleBar::window_options()
            };

            gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Stitch");
                let app_state = AppState::new(window, cx);
                cx.new(|cx| WorkspaceShell::new(app_state, window, cx))
            })
            .expect("failed to open Stitch main window");

            cx.activate(true);
        });
}
