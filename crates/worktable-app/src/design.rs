//! Worktable design tokens.
//!
//! One owner for the values the guides say application code must not invent
//! locally:
//!
//! - **radii** come from the active theme's `RadiusTokens`, so a theme change
//!   moves every corner together (`theme.radius` and `theme.radius_lg`);
//! - **spacing, type and control sizes** use GPUI's rem scale helpers
//!   (`gap_2`, `p_4`, `h_9`, `size_4`, `text_sm`) directly at the call site;
//! - **measured geometry** that must be resolved to pixels (virtual-list row
//!   metrics, the reading column, popup offsets) is declared here in rems and
//!   resolved with [`to_pixels`] against `window.rem_size()`, so it tracks
//!   interface zoom instead of freezing at the default font size.
//!
//! Radius roles map onto the theme's three tiers:
//!
//! | Role      | Token               | Typical use                        |
//! |-----------|---------------------|------------------------------------|
//! | `sm`      | `radius / 2`        | chips, badges, inline code         |
//! | `md`      | `theme.radius`      | rows, cards, ordinary controls     |
//! | `lg`      | `theme.radius_lg`   | menus, popovers, floating surfaces |

use gpui::{App, Pixels, Rems, Window, rems};
use gpui_component::{Theme, ThemeRegistry};

/// Install the bundled palette before the first frame. Disk watching can
/// replace it later, but a missing theme directory must not leave blue defaults.
pub fn load_theme(cx: &mut App) -> anyhow::Result<()> {
    ThemeRegistry::global_mut(cx).load_themes_from_str(include_str!("../../../themes/ayu.json"))?;
    apply_registered_theme(cx);
    Ok(())
}

/// Register both variants without changing the user's current appearance.
pub fn apply_registered_theme(cx: &mut App) {
    // Since 0.7, apply_config also switches modes. Capture this before loading
    // either variant, not after the dark variant has made itself active.
    let mode = Theme::global(cx).mode;
    for name in ["Ayu Light", "Ayu Dark"] {
        if let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() {
            Theme::global_mut(cx).apply_config(&config);
        }
    }
    Theme::change(mode, None, cx);
    apply_control_style(cx);
}

/// Product-wide control treatment, reapplied after a theme file is loaded.
pub fn apply_control_style(cx: &mut App) {
    Theme::update(cx, |theme| {
        // GPUI Kit keeps the 1px focus edge when the outside halo is disabled.
        theme.focus_ring = false;
        // macOS Tahoe control radii are a documented platform boundary.
        theme.radius = gpui::px(10.);
        theme.radius_lg = gpui::px(14.);
    });
}

/// Reading-column width the app is designed around; content never gets
/// narrower than this when the viewport allows it.
pub const CONTENT_MAX_WIDTH: Rems = rems(45.);

/// Reading-column cap on very wide windows. Between the two caps the column
/// tracks the viewport, so fullscreen windows use their space instead of
/// showing a phone-width sliver in the middle.
pub const CONTENT_MAX_WIDTH_WIDE: Rems = rems(62.);

/// Minimum height of one entry card in the virtualized entries list: fits the
/// meta row plus a single body line.
pub const ENTRY_CARD_MIN_HEIGHT: Rems = rems(6.75);

/// Maximum height of one entry card. Longer bodies clamp their preview inside
/// this bound, so a card never grows past it.
pub const ENTRY_CARD_MAX_HEIGHT: Rems = rems(10.5);

/// Vertical gap between entry cards, so repeated rows read as separate cards
/// rather than one continuous surface.
pub const ENTRY_ROW_GAP: Rems = rems(0.5);

/// Height of a section heading row in the virtualized entries list.
pub const SECTION_HEADER_HEIGHT: Rems = rems(1.875);

/// Width of the modal provider configuration dialog: wide enough for an API
/// key field with a hint, narrow enough to read as a dialog on small windows.
pub const PROVIDER_DIALOG_WIDTH: Rems = rems(26.25);

/// Width of the GitHub stars dialog: fits full repository names and the
/// fetch/import controls on one row.
pub const GITHUB_DIALOG_WIDTH: Rems = rems(30.0);

/// Width of the first-run onboarding card: room for the shortcut rows without
/// stretching to dialog width.
pub const ONBOARDING_CARD_WIDTH: Rems = rems(34.0);

/// Maximum height of the expanded thinking block; longer reasoning scrolls
/// inside it (with `TextView::scrollable`).
pub const THINKING_MAX_HEIGHT: Rems = rems(10.0);

/// The chat picker rises from the bottom edge, capped to the window at render.
pub const CHATS_SHEET_HEIGHT: Rems = rems(26.0);

/// Two lines per saved-chat row: title and last activity.
pub const CHAT_ROW_HEIGHT: Rems = rems(4.0);

/// Entries ⇄ Agent page offset — transitions.dev's `--page-slide-distance`
/// (8px). The outgoing page exits toward it, the incoming page enters from it.
pub const PAGE_SLIDE_DISTANCE: Rems = rems(0.5);

/// Resolve a rem metric for layout and measurement code that needs pixels
/// (virtual-list row sizes, pane arithmetic, animation geometry).
///
/// Measurement is invalidated naturally because callers recompute from
/// `window.rem_size()` on every render, so zooming relayouts the list.
pub fn to_pixels(value: Rems, window: &Window) -> Pixels {
    value.to_pixels(window.rem_size())
}

/// The content column's width for the current window: full width on small
/// windows, then growing to the comfortable cap and stopping at the roomy cap
/// so fullscreen windows keep proportional side margins.
pub fn content_column_width(window: &Window) -> Pixels {
    let viewport = window.viewport_size().width;
    let comfortable = to_pixels(CONTENT_MAX_WIDTH, window);
    let roomy = to_pixels(CONTENT_MAX_WIDTH_WIDE, window);
    let target = (viewport * 0.72).clamp(comfortable.min(roomy), roomy);
    viewport.min(target)
}

/// Physical window-chrome inset for the transparent macOS titlebar: leaves
/// room for the traffic lights. A platform boundary, not a design value.
pub const TITLEBAR_INSET: Pixels = gpui::px(30.);

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::ThemeMode;

    #[gpui::test]
    fn bundled_theme_keeps_the_mode_and_uses_warm_selection(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            load_theme(cx).expect("bundled theme");
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                Theme::change(mode, None, cx);
                apply_registered_theme(cx);
                let theme = Theme::global(cx);
                assert_eq!(theme.mode, mode, "loading both variants must preserve mode");
                assert_eq!(theme.list_active.h, theme.accent.h);
                assert_eq!(theme.selection.h, theme.primary.h);
                assert_eq!(theme.tokens.list_active.color, theme.list_active);
                assert_eq!(theme.tokens.selection.color, theme.selection);
                assert!(
                    !theme.focus_ring,
                    "focus uses a thin edge, not an outside halo"
                );
            }
        });
    }
}
