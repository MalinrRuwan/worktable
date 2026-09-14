//! Process-wide UI preferences that non-view code must read synchronously.
//!
//! The window-close handler runs inside an AppKit callback, so it cannot await
//! the async config store. The view keeps this mirror in sync when it loads
//! preferences and when the user flips a toggle.

use std::sync::atomic::{AtomicBool, Ordering};

/// Config key for the "keep running in the background" preference.
pub const BACKGROUND_ON_CLOSE_KEY: &str = "background_on_close";

/// Default: closing the window keeps Worktable in the menu bar (the app is a
/// capture tool first), removes it from the Dock, and leaves capture running.
static BACKGROUND_ON_CLOSE: AtomicBool = AtomicBool::new(true);

/// Whether closing the main window should keep the app alive in the menu bar.
pub fn background_on_close() -> bool {
    BACKGROUND_ON_CLOSE.load(Ordering::Relaxed)
}

/// Mirror the preference for code that cannot await the config store.
pub fn set_background_on_close(value: bool) {
    BACKGROUND_ON_CLOSE.store(value, Ordering::Relaxed);
}
