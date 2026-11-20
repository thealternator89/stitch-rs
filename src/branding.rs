use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use gpui_kit::{AssetSource, Image, ImageFormat, Result, SharedString, assets};

pub const ICON_ICNS_BYTES: &[u8] = include_bytes!("../assets/icon.icns");
pub const ICON_ICO_BYTES: &[u8] = include_bytes!("../assets/icon.ico");

#[allow(dead_code)]
pub const ICON_ASSET_PATH: &str = "icon.icns";

/// Returns a cached [`Image`] handle for the Stitch application icon (`assets/icon.icns`).
pub fn icon_image() -> Arc<Image> {
    static ICON: OnceLock<Arc<Image>> = OnceLock::new();
    ICON.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Png, ICON_ICNS_BYTES.to_vec())))
        .clone()
}

/// Composite asset source combining Stitch's embedded branding assets with
/// [`gpui_kit::assets::AllAssets`] (Lucide icon catalog).
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let normalized = path.trim_start_matches("./");
        match normalized {
            "icon.icns" | "assets/icon.icns" | "icon.png" | "assets/icon.png" => {
                Ok(Some(Cow::Borrowed(ICON_ICNS_BYTES)))
            }
            "icon.ico" | "assets/icon.ico" => Ok(Some(Cow::Borrowed(ICON_ICO_BYTES))),
            _ => assets::AllAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut entries = assets::AllAssets.list(path)?;
        for custom in [
            "icon.icns",
            "assets/icon.icns",
            "icon.ico",
            "assets/icon.ico",
        ] {
            if custom.starts_with(path) {
                entries.push(SharedString::from(custom));
            }
        }
        Ok(entries)
    }
}

/// Sets the native application/Dock icon at runtime when supported by the OS.
pub fn set_app_dock_icon() {
    #[cfg(target_os = "macos")]
    {
        use objc2::{AnyThread as _, MainThreadMarker};
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::NSData;

        if let Some(mtm) = MainThreadMarker::new() {
            let data = NSData::with_bytes(ICON_ICNS_BYTES);
            if let Some(ns_image) = NSImage::initWithData(NSImage::alloc(), &data) {
                unsafe {
                    NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&ns_image));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_embedded_branding_assets_and_cached_images() {
        let icon = AppAssets
            .load(ICON_ASSET_PATH)
            .expect("load icon")
            .expect("icon bytes present");
        assert!(!icon.is_empty());
        assert!(icon.starts_with(b"\x89PNG\r\n\x1a\n"));

        let ico = AppAssets
            .load("assets/icon.ico")
            .expect("load ico")
            .expect("ico bytes present");
        assert!(!ico.is_empty());

        let listed = AppAssets.list("icon").expect("list assets");
        assert!(listed.contains(&SharedString::from("icon.icns")));
        assert!(listed.contains(&SharedString::from("icon.ico")));

        assert!(!icon_image().bytes().is_empty());
    }
}
