use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::{
    App, Entity, FontWeight, ParentElement as _, Styled as _, Window, div, hsla,
    prelude::FluentBuilder as _, px, relative,
};

use crate::app_state::{AppState, check_for_updates, download_update, install_ready_update};
use crate::infrastructure::constants::APP_VERSION;
use crate::infrastructure::updater::{UpdateReleaseInfo, UpdateStatus};

fn format_bytes_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn release_info_from_status(status: &UpdateStatus) -> Option<&UpdateReleaseInfo> {
    match status {
        UpdateStatus::Available { info, .. }
        | UpdateStatus::Downloading { info, .. }
        | UpdateStatus::ReadyToInstall { info, .. }
        | UpdateStatus::Installing { info } => Some(info),
        UpdateStatus::Error { info, .. } => info.as_ref(),
        _ => None,
    }
}

/// Opens the minimal Update Details dialog showing version comparison, GitHub release notes link,
/// download progress, active-task warning, and contextual install/download actions.
pub fn open_update_modal(app_state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _window, cx| {
        let state = app_state.read(cx);
        let status = state.update_status.clone();
        let active_task_labels: Vec<String> = state
            .active_tasks
            .keys()
            .map(|v| v.label().to_string())
            .collect();
        let theme = cx.theme();
        let amber = hsla(38.0 / 360.0, 0.92, 0.50, 1.0);
        let amber_bg = hsla(38.0 / 360.0, 0.92, 0.50, 0.12);
        let red = hsla(0.0, 0.78, 0.55, 1.0);
        let red_bg = hsla(0.0, 0.78, 0.55, 0.10);
        let mono_font = theme.mono_font_family.clone();

        let is_installing = matches!(status, UpdateStatus::Installing { .. });
        let info_opt = release_info_from_status(&status).cloned();

        let mut body = v_flex().gap_3().text_sm().text_color(theme.foreground);

        if let Some(info) = info_opt.as_ref() {
            let release_url = info.release_url.clone();
            body = body.child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .font_family(mono_font.clone())
                                    .text_xs()
                                    .child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("Current: v{APP_VERSION}")),
                                    )
                                    .child(div().text_color(theme.muted_foreground).child("→"))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.foreground)
                                            .child(format!("Latest: v{}", info.version)),
                                    ),
                            )
                            .child(
                                Button::new("update-modal-release-notes")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(LucideIcon::ExternalLink).xsmall())
                                    .label("View Release Notes on GitHub")
                                    .on_click(move |_, _, cx| {
                                        cx.open_url(&release_url);
                                    }),
                            ),
                    ),
            );
        }

        match &status {
            UpdateStatus::Available {
                can_auto_install, ..
            } => {
                let msg = if *can_auto_install {
                    "A new version of Stitch is available to download and install."
                } else {
                    "A new version of Stitch is available on GitHub Releases."
                };
                body = body.child(div().text_xs().text_color(theme.muted_foreground).child(msg));
            }
            UpdateStatus::Downloading {
                downloaded_bytes,
                total_bytes,
                ..
            } => {
                let fraction = match total_bytes {
                    Some(total) if *total > 0 => (*downloaded_bytes as f32 / *total as f32).clamp(0.02, 1.0),
                    _ => 0.35,
                };
                let progress_text = match total_bytes {
                    Some(total) if *total > 0 => format!(
                        "{} / {} ({:.0}%)",
                        format_bytes_mb(*downloaded_bytes),
                        format_bytes_mb(*total),
                        (*downloaded_bytes as f64 / *total as f64) * 100.0
                    ),
                    _ => format!("{} downloaded...", format_bytes_mb(*downloaded_bytes)),
                };
                body = body.child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Downloading update in the background. You can close this dialog and keep working."),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(6.))
                                .rounded_full()
                                .bg(theme.secondary)
                                .overflow_hidden()
                                .child(
                                    div()
                                        .w(relative(fraction))
                                        .h_full()
                                        .bg(theme.primary),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_family(mono_font)
                                .text_color(theme.muted_foreground)
                                .child(progress_text),
                        ),
                );
            }
            UpdateStatus::ReadyToInstall { .. } => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("The installer has been downloaded and verified. Restart Stitch to apply the update."),
                );
                if !active_task_labels.is_empty() {
                    body = body.child(
                        h_flex()
                            .p_2p5()
                            .gap_2()
                            .items_start()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(amber)
                            .bg(amber_bg)
                            .child(
                                Icon::new(LucideIcon::TriangleAlert)
                                    .small()
                                    .text_color(amber),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.foreground)
                                    .child(format!(
                                        "Restarting now will cancel active tasks: {}.",
                                        active_task_labels.join(", ")
                                    )),
                            ),
                    );
                }
            }
            UpdateStatus::Installing { .. } => {
                body = body.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Preparing update installer..."),
                        ),
                );
            }
            UpdateStatus::Error { message, .. } => {
                body = body.child(
                    div()
                        .p_2p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(red)
                        .bg(red_bg)
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(message.clone()),
                );
            }
            UpdateStatus::UpToDate { .. } => {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Stitch v{APP_VERSION} is up to date.")),
                );
            }
            UpdateStatus::Checking => {
                body = body.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Checking for updates..."),
                        ),
                );
            }
            UpdateStatus::Idle | UpdateStatus::Disabled { .. } => {}
        }

        let footer_left = if let Some(info) = info_opt.as_ref() {
            if matches!(
                status,
                UpdateStatus::Available { .. } | UpdateStatus::ReadyToInstall { .. }
            ) {
                let ver = info.version.clone();
                let app_state_skip = app_state.clone();
                Some(
                    Button::new("update-modal-skip")
                        .ghost()
                        .label("Skip this version")
                        .on_click(move |_, window, cx| {
                            app_state_skip.update(cx, |state, cx| {
                                state.skip_update_version(ver.clone());
                                cx.notify();
                            });
                            window.close_dialog(cx);
                        }),
                )
            } else {
                None
            }
        } else {
            None
        };

        let mut footer_right = h_flex().gap_2();

        match &status {
            UpdateStatus::ReadyToInstall { .. } => {
                let app_state_install = app_state.clone();
                let install_label = if active_task_labels.is_empty() {
                    "Restart & Install"
                } else {
                    "Force Restart & Install"
                };
                footer_right = footer_right
                    .child(
                        Button::new("update-modal-later")
                            .ghost()
                            .label("Later")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("update-modal-install")
                            .primary()
                            .icon(Icon::new(LucideIcon::RefreshCw).small())
                            .label(install_label)
                            .on_click(move |_, window, cx| {
                                install_ready_update(app_state_install.clone(), window, cx);
                            }),
                    );
            }
            UpdateStatus::Available {
                info,
                can_auto_install,
            } => {
                footer_right = footer_right.child(
                    Button::new("update-modal-later")
                        .ghost()
                        .label("Later")
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                );
                if *can_auto_install {
                    let info_dl = info.clone();
                    let app_state_dl = app_state.clone();
                    footer_right = footer_right.child(
                        Button::new("update-modal-download")
                            .primary()
                            .icon(Icon::new(LucideIcon::Download).small())
                            .label("Download Update")
                            .on_click(move |_, window, cx| {
                                download_update(app_state_dl.clone(), info_dl.clone(), window, cx);
                            }),
                    );
                } else {
                    let url = info.release_url.clone();
                    footer_right = footer_right.child(
                        Button::new("update-modal-open-release")
                            .primary()
                            .icon(Icon::new(LucideIcon::ExternalLink).small())
                            .label("Open Release Page")
                            .on_click(move |_, window, cx| {
                                cx.open_url(&url);
                                window.close_dialog(cx);
                            }),
                    );
                }
            }
            UpdateStatus::Error { info, .. } => {
                let app_state_retry = app_state.clone();
                let info_retry = info.clone();
                let can_retry_download = info_retry
                    .as_ref()
                    .is_some_and(|i| i.asset.is_some() && state.updater_service.can_auto_install());
                footer_right = footer_right
                    .child(
                        Button::new("update-modal-close")
                            .ghost()
                            .label("Close")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                    )
                    .when_some(info.as_ref().map(|i| i.release_url.clone()), |row, url| {
                        row.child(
                            Button::new("update-modal-error-release")
                                .ghost()
                                .icon(Icon::new(LucideIcon::ExternalLink).small())
                                .label("Open Release Page")
                                .on_click(move |_, _, cx| {
                                    cx.open_url(&url);
                                }),
                        )
                    })
                    .child(
                        Button::new("update-modal-retry")
                            .primary()
                            .icon(Icon::new(LucideIcon::RefreshCw).small())
                            .label("Retry")
                            .on_click(move |_, window, cx| {
                                if can_retry_download {
                                    if let Some(ref info) = info_retry {
                                        download_update(
                                            app_state_retry.clone(),
                                            info.clone(),
                                            window,
                                            cx,
                                        );
                                    }
                                } else {
                                    check_for_updates(app_state_retry.clone(), true, window, cx);
                                }
                            }),
                    );
            }
            UpdateStatus::Installing { .. } => {}
            _ => {
                footer_right = footer_right.child(
                    Button::new("update-modal-close")
                        .primary()
                        .label("Close")
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                );
            }
        }

        dialog
            .w(px(500.))
            .overlay_closable(!is_installing)
            .close_button(!is_installing)
            .title(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(LucideIcon::Download)
                            .with_size(Size::Medium)
                            .text_color(theme.primary),
                    )
                    .child("Update Stitch"),
            )
            .child(body)
            .footer(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(div().when_some(footer_left, |slot, btn| slot.child(btn)))
                    .child(footer_right),
            )
    });
}
