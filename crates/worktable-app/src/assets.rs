//! Application asset source: the Worktable brand logo layered over the
//! component library's icons and fonts.
//!
//! `gpui_component_assets::Assets` stays authoritative for component assets;
//! this wrapper only answers for the brand paths so `img(LOGO_PATH)` works
//! anywhere in the app (and in the visual runner, which uses the same source).

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

/// Asset path of the in-app brand logo (`png`, 256×256).
pub const LOGO_PATH: &str = "worktable-logo.png";

/// Asset path of the Lucide `sparkles` glyph used by the Entries|Agent toggle.
pub const SPARKLES_PATH: &str = "worktable-sparkles.svg";

/// Lucide-style conversation glyph for the saved chat picker.
pub const CHATS_PATH: &str = "worktable-chats.svg";

/// The app's asset source.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if path == LOGO_PATH {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/logo.png"))));
        }
        if path == SPARKLES_PATH {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/sparkles.svg"
            ))));
        }
        if path == CHATS_PATH {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/chats.svg"))));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        gpui_component_assets::Assets.list(path)
    }
}
