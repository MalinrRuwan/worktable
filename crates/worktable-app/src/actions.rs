//! Actions for the Worktable application.
//!
//! Every action here is wired into the native application menu bar, the keymap,
//! and (on macOS) the menu-bar status item, so a single definition keeps
//! keyboard shortcuts, menus, and the tray in sync.

use gpui::actions;

actions!(
    worktable,
    [
        /// Focus the search field (⌘F)
        FocusSearch,
        /// Jump to the entries section (⌘1)
        ShowEntries,
        /// Jump to the AI assistant section (⌘2)
        ShowAssistant,
        /// Open the AI provider settings panel (⌘,)
        ShowSettings,
        /// Delete the selected entry (⌫)
        DeleteEntry,
        /// Open the selected entry (⏎ / ⌘O)
        OpenEntry,
        /// Copy the selected entry's text (⌘C when an entry is selected)
        CopyEntry,
        /// Copy the selected entry's URL
        CopyLink,
        /// Dismiss the composer (⎋)
        CancelComposer,
        /// Submit the composer (⌘⏎)
        SubmitComposer,
        /// Toggle dark / light theme
        ToggleTheme,
        /// Toggle the main window visibility (menu-bar icon)
        ToggleWindow,
        /// Show the standard About panel
        About,
        /// Hide the application
        Hide,
        /// Hide every other application
        HideOthers,
        /// Reveal all hidden applications
        ShowAll,
        /// Minimize the main window
        MinimizeWindow,
        /// Zoom the main window
        ZoomWindow,
        /// Quit the application
        Quit,
        /// Clear the active search
        ClearSearch,
        /// Close the fullscreen image viewer (⎋)
        CloseImageViewer,
        /// Move the entry selection up
        SelectPrevious,
        /// Move the entry selection down
        SelectNext,
    ]
);
