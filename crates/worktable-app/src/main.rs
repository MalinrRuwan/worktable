//! Worktable app entry point.
//!
//! Bootstraps the Tokio runtime + `WorktableService`, opens the main GPUI
//! window, installs the macOS status item, and wires keybindings + status-item
//! commands to the main view.

pub(crate) mod actions;
pub(crate) mod assets;
pub(crate) mod assistant;
pub(crate) mod design;
mod entry_actions;
pub(crate) mod format;
pub(crate) mod github;
pub(crate) mod preferences;
pub(crate) mod service;
pub(crate) mod status_item;
pub(crate) mod worktable_view;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context as _;
use gpui::{
    App, AppContext as _, Application, AsyncApp, Bounds, KeyBinding, Menu, MenuItem, OsAction,
    SharedString, SystemMenuType, WindowBounds, WindowOptions, px, size,
};
use gpui_component::{Root, Theme, ThemeRegistry};
use tokio::runtime::Runtime;
use worktable_view::WorktableView;

use crate::service::{AppCommand, WorktableService};

/// Global handle to the main view so commands (status item) can reach it.
struct MainView(gpui::Entity<WorktableView>);
impl gpui::Global for MainView {}

fn main() -> anyhow::Result<()> {
    let tokio = Arc::new(Runtime::new().context("failed to create Worktable Tokio runtime")?);
    let service = Arc::new(WorktableService::new()?);

    let visible = Arc::new(AtomicBool::new(true));
    let service_for_reopen = service.clone();
    let visible_for_reopen = visible.clone();
    let application = Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(assets::AppAssets);
    application.on_reopen(move |cx| {
        open_window(cx, &visible_for_reopen, &service_for_reopen);
    });

    application.run(move |cx: &mut App| {
        gpui_component::init(cx);
        // The brand icon for the Dock, app switcher, and About panel.
        #[cfg(target_os = "macos")]
        status_item::set_app_icon();
        init_theme(cx);
        // The Tahoe radii must be applied once at startup too — the theme
        // watcher callback only fires when the theme files change on disk.
        apply_tahoe_radius(cx);

        // Keep the quit hook alive for the lifetime of the app.
        let tokio_for_quit = tokio.clone();
        let service_for_quit = service.clone();
        let quit_subscription = cx.on_app_quit(move |_cx| {
            let service = service_for_quit.clone();
            let tokio = tokio_for_quit.clone();
            async move {
                tokio.block_on(service.shutdown());
            }
        });
        std::mem::forget(quit_subscription);

        // Keyboard shortcuts and the native application menu bar.
        cx.bind_keys(bindings());
        cx.set_menus(app_menus());
        cx.on_action(|_: &crate::actions::Quit, cx: &mut App| {
            cx.quit();
        });
        cx.on_action(|_: &crate::actions::About, _: &mut App| {
            #[cfg(target_os = "macos")]
            status_item::show_about_panel();
        });
        cx.on_action(|_: &crate::actions::Hide, cx: &mut App| {
            cx.hide();
        });
        cx.on_action(|_: &crate::actions::HideOthers, _: &mut App| {
            #[cfg(target_os = "macos")]
            status_item::hide_other_applications();
        });
        cx.on_action(|_: &crate::actions::ShowAll, _: &mut App| {
            #[cfg(target_os = "macos")]
            status_item::show_all_applications();
        });
        cx.on_action(|_: &crate::actions::MinimizeWindow, cx: &mut App| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, _| window.minimize_window());
            }
        });
        cx.on_action(|_: &crate::actions::ZoomWindow, cx: &mut App| {
            if let Some(window) = cx.active_window() {
                let _ = window.update(cx, |_, window, _| window.zoom_window());
            }
        });

        // Window visibility state (driven by the status item and Dock).

        // Open the main window.
        if let Err(error) = open_main_window(cx, service.clone(), visible.clone()) {
            eprintln!("Worktable: failed to open window: {error:#}");
        }

        // Install the macOS menu-bar item.
        #[cfg(target_os = "macos")]
        {
            let sender = service.command_sender();
            if !status_item::install(sender) {
                eprintln!("Worktable: failed to install the macOS menu-bar item");
            }
        }
        // Route status-item commands into the app.
        let mut command_rx = service
            .take_command_receiver()
            .expect("command receiver should be available");
        let visible_for_commands = visible.clone();
        let service_for_commands = service.clone();
        cx.spawn(|cx: &mut AsyncApp| {
            let cx = cx.clone();
            async move {
                while let Some(command) = command_rx.recv().await {
                    let visible = visible_for_commands.clone();
                    let service = service_for_commands.clone();
                    cx.update(move |cx| handle_command(command, cx, &visible, &service));
                }
            }
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

/// Load the application's theme files and keep the active theme in sync with
/// changes made while the app is running.
fn init_theme(cx: &mut App) {
    let theme_name = SharedString::from("Ayu Light");
    let themes_dir = resolve_themes_dir();
    if let Err(error) = ThemeRegistry::watch_dir(themes_dir, cx, move |cx| {
        // Apply both variants of the watched theme: `apply_config` only stores
        // the config matching its own mode, so loading the light theme alone
        // leaves dark mode on the default (blue) dark palette.
        let light = ThemeRegistry::global(cx).themes().get(&theme_name).cloned();
        let dark = ThemeRegistry::global(cx)
            .themes()
            .get(&SharedString::from("Ayu Dark"))
            .cloned();
        if let Some(light) = light {
            Theme::global_mut(cx).apply_config(&light);
        }
        if let Some(dark) = dark {
            Theme::global_mut(cx).apply_config(&dark);
        }
        let mode = Theme::global(cx).mode;
        // `apply_config` updates the component theme. Calling `change` also
        // refreshes GPUI Base's semantic tokens and scrollbars.
        Theme::change(mode, None, cx);
        apply_tahoe_radius(cx);
    }) {
        eprintln!("Worktable: failed to watch themes directory: {error}");
    }
}

/// macOS Tahoe (Liquid Glass) radius scale for every component control:
/// buttons/inputs/switches at 10px, dialogs/popovers at 14px. Applied after
/// any theme config load, which resets radii to the theme's own defaults.
pub(crate) fn apply_tahoe_radius(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    theme.radius = px(10.);
    theme.radius_lg = px(14.);
}

fn resolve_themes_dir() -> PathBuf {
    if let Ok(path) = std::env::var("WORKTABLE_THEMES_DIR") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return path;
        }
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("../Resources/themes"));
        candidates.push(directory.join("../../themes"));
    }
    if let Ok(current_dir) = std::env::current_dir() {
        candidates.push(current_dir.join("themes"));
    }

    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .unwrap_or_else(|| Path::new("themes").to_path_buf())
}

/// All keyboard shortcuts for the app.
fn bindings() -> Vec<KeyBinding> {
    use crate::actions::*;
    vec![
        KeyBinding::new("cmd-f", FocusSearch, None),
        KeyBinding::new("cmd-1", ShowEntries, None),
        KeyBinding::new("cmd-2", ShowAssistant, None),
        KeyBinding::new("cmd-,", ShowSettings, None),
        KeyBinding::new("cmd-t", ToggleTheme, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("backspace", DeleteEntry, Some("worktable-list")),
        KeyBinding::new("enter", OpenEntry, Some("worktable-list")),
        // Arrows are global so the list moves without first focusing it; the
        // handlers still yield while a text field owns the keyboard.
        KeyBinding::new("up", SelectPrevious, None),
        KeyBinding::new("down", SelectNext, None),
        KeyBinding::new("cmd-c", CopyEntry, Some("worktable-list")),
        KeyBinding::new("cmd-shift-c", CopyLink, Some("worktable-list")),
        KeyBinding::new("escape", CancelComposer, None),
        KeyBinding::new("cmd-enter", SubmitComposer, None),
        KeyBinding::new("escape", CloseImageViewer, Some("ImageViewer")),
        KeyBinding::new("cmd-shift-p", ClearSearch, Some("worktable-list")),
    ]
}

/// The native macOS menu bar: the app menu, File, Edit, View, and Window.
/// Key equivalents come from the keymap bindings above, so the menu and the
/// shortcuts never drift apart.
fn app_menus() -> Vec<Menu> {
    use crate::actions::*;
    use gpui_component::input::{
        Copy as InputCopy, Cut as InputCut, Paste as InputPaste, Redo as InputRedo,
        SelectAll as InputSelectAll, Undo as InputUndo,
    };

    vec![
        Menu::new("Worktable").items(vec![
            MenuItem::action("About Worktable", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", ShowSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Worktable", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Worktable", Quit),
        ]),
        Menu::new("Edit").items(vec![
            MenuItem::os_action("Undo", InputUndo, OsAction::Undo),
            MenuItem::os_action("Redo", InputRedo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", InputCut, OsAction::Cut),
            MenuItem::os_action("Copy", InputCopy, OsAction::Copy),
            MenuItem::os_action("Paste", InputPaste, OsAction::Paste),
            MenuItem::os_action("Select All", InputSelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items(vec![
            MenuItem::action("Entries", ShowEntries),
            MenuItem::action("Assistant", ShowAssistant),
            MenuItem::separator(),
            MenuItem::action("Toggle Theme", ToggleTheme),
        ]),
        Menu::new("Window").items(vec![
            MenuItem::action("Minimize", MinimizeWindow),
            MenuItem::action("Zoom", ZoomWindow),
        ]),
    ]
}

/// Handle a command from the macOS status item.
fn handle_command(
    command: AppCommand,
    cx: &mut App,
    visible: &Arc<AtomicBool>,
    service: &Arc<WorktableService>,
) {
    match command {
        AppCommand::ToggleWindow => toggle_window(cx, visible, service),
        AppCommand::OpenWindow => open_window(cx, visible, service),
        AppCommand::CaptureText(text) => {
            // Blink the menu-bar glyph twice so the triple-Shift capture is
            // visibly acknowledged.
            #[cfg(target_os = "macos")]
            status_item::flash_capture_feedback(cx);
            if let Some(view) = cx.try_global::<MainView>().map(|main| main.0.clone()) {
                view.update(cx, |this, cx| this.add_captured_text(text, cx));
            }
        }
        AppCommand::CaptureImage { path, mime_type } => {
            #[cfg(target_os = "macos")]
            status_item::flash_capture_feedback(cx);
            if let Some(view) = cx.try_global::<MainView>().map(|main| main.0.clone()) {
                view.update(cx, |this, cx| this.add_captured_image(path, mime_type, cx));
            }
        }
        AppCommand::Quit => cx.quit(),
    }
}

/// Bring the app forward / hide it (driven by the status item).
fn toggle_window(cx: &mut App, visible: &Arc<AtomicBool>, service: &Arc<WorktableService>) {
    if cx.windows().is_empty() {
        open_window(cx, visible, service);
        return;
    }

    let currently_visible = visible.swap(false, Ordering::SeqCst);
    if currently_visible {
        cx.hide();
    } else {
        // Showing from the status menu also comes back to the Dock.
        #[cfg(target_os = "macos")]
        status_item::restore_dock_and_unhide();
        visible.store(true, Ordering::SeqCst);
        cx.activate(true);
    }
}

fn open_window(cx: &mut App, visible: &Arc<AtomicBool>, service: &Arc<WorktableService>) {
    // Reopening from the menu bar restores the Dock presence before the
    // window comes forward (unhide → activate → Regular policy).
    #[cfg(target_os = "macos")]
    status_item::restore_dock_and_unhide();
    if cx.windows().is_empty()
        && let Err(error) = open_main_window(cx, service.clone(), visible.clone())
    {
        eprintln!("Worktable: failed to reopen window: {error:#}");
        return;
    }
    visible.store(true, Ordering::SeqCst);
    cx.activate(true);
}

fn open_main_window(
    cx: &mut App,
    service: Arc<WorktableService>,
    visible: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    visible.store(true, Ordering::SeqCst);
    cx.open_window(
        WindowOptions {
            // A portrait 3:4 window: the reading column stays centered and
            // the minimum keeps the composer usable.
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(600.), px(800.)),
                cx,
            ))),
            window_min_size: Some(size(px(480.), px(640.))),
            // Transparent titlebar so the app background paints through it —
            // no visible seam between the title bar and the content.
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Worktable".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            ..WindowOptions::default()
        },
        move |window, cx| {
            let visible_for_close = visible.clone();
            window.on_window_should_close(cx, move |_window, cx| {
                visible_for_close.store(false, Ordering::SeqCst);
                if preferences::background_on_close() {
                    // Keep the process, menu-bar item, and global capture
                    // monitor alive, and step out of the Dock: Worktable
                    // becomes a menu-bar app until the window is reopened.
                    #[cfg(target_os = "macos")]
                    status_item::set_dock_visible(false);
                    cx.hide();
                    false
                } else {
                    // The preference is off: closing the window quits.
                    cx.quit();
                    false
                }
            });

            let view = cx.new(|cx| WorktableView::new(service.clone(), window, cx));
            cx.set_global(MainView(view.clone()));
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    Ok(())
}
