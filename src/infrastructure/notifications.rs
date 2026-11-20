use gpui_kit::Window;

/// Dispatches native desktop notifications when long-running operations complete while the Stitch window is unfocused.
pub struct NotificationService;

impl NotificationService {
    /// Shows a native OS notification if `window` is not currently active/focused.
    pub fn show_if_unfocused(window: &Window, title: impl Into<String>, body: impl Into<String>) {
        if window.is_window_active() {
            return;
        }
        Self::show(title.into(), body.into());
    }

    /// Dispatches a native OS notification asynchronously on a background thread.
    pub fn show(title: String, body: String) {
        std::thread::spawn(move || {
            let _ = notify_rust::Notification::new()
                .appname("Stitch")
                .summary(&title)
                .body(&body)
                .show();
        });
    }
}
