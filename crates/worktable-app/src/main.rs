//! Worktable app entry point.
//!
//! Bootstraps the Tokio runtime + `WorktableService`, opens the main GPUI
//! window, installs the macOS status item, and wires keybindings + status-item
//! commands to the main view.

pub(crate) mod actions;
pub(crate) mod assistant;
pub(crate) mod format;
pub(crate) mod github;
pub(crate) mod markdown;
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
    App, AppContext as _, Application, AsyncApp, Bounds, Context, KeyBinding, SharedString,
    WindowBounds, WindowOptions, px, size,
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
        .with_assets(gpui_component_assets::Assets);
    application.on_reopen(move |cx| {
        open_window(cx, &visible_for_reopen, &service_for_reopen);
    });

    application.run(move |cx: &mut App| {
        gpui_component::init(cx);
        init_theme(cx);

        // Keep the quit hook alive for the lifetime of the app.
        let tokio_for_quit = tokio.clone();
        let service_for_quit = service.clone();
        let quit_subscription = cx.on_app_quit(move |cx| {
            let _ = cx;
            let service = service_for_quit.clone();
            let tokio = tokio_for_quit.clone();
            async move {
                let _ = tokio.block_on(service.shutdown());
            }
        });
        std::mem::forget(quit_subscription);

        // Keyboard shortcuts.
        cx.bind_keys(bindings());
        cx.on_action(|_: &crate::actions::Quit, cx: &mut App| {
            cx.quit();
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
            if status_item::install(sender).is_none() {
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
                    let _ = cx.update(move |cx| handle_command(command, cx, &visible, &service));
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
        if let Some(theme) = ThemeRegistry::global(cx).themes().get(&theme_name).cloned() {
            let mode = Theme::global(cx).mode;
            Theme::global_mut(cx).apply_config(&theme);
            // `apply_config` updates the component theme. Calling `change`
            // also refreshes GPUI Base's semantic tokens and scrollbars.
            Theme::change(mode, None, cx);
        }
    }) {
        eprintln!("Worktable: failed to watch themes directory: {error}");
    }
}

fn resolve_themes_dir() -> PathBuf {
    if let Ok(path) = std::env::var("WORKTABLE_THEMES_DIR") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return path;
        }
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join("../Resources/themes"));
            candidates.push(directory.join("../../themes"));
        }
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
        KeyBinding::new("cmd-n", NewNote, None),
        KeyBinding::new("cmd-l", NewLink, None),
        KeyBinding::new("cmd-f", FocusSearch, None),
        KeyBinding::new("cmd-1", ShowEntries, None),
        KeyBinding::new("cmd-2", ShowAssistant, None),
        KeyBinding::new("cmd-,", ShowSettings, None),
        KeyBinding::new("cmd-shift-s", ToggleSidebar, None),
        KeyBinding::new("cmd-t", ToggleTheme, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("backspace", DeleteEntry, Some("worktable-list")),
        KeyBinding::new("enter", OpenEntry, Some("worktable-list")),
        KeyBinding::new("cmd-c", CopyEntry, Some("worktable-list")),
        KeyBinding::new("cmd-shift-c", CopyLink, Some("worktable-list")),
        KeyBinding::new("escape", CancelComposer, None),
        KeyBinding::new("cmd-enter", SubmitComposer, None),
        KeyBinding::new("cmd-shift-p", ClearSearch, Some("worktable-list")),
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
        AppCommand::NewNote => {
            let _ = dispatch_main_view(cx, |this, window, cx| this.open_composer_note(window, cx));
            cx.activate(true);
        }
        AppCommand::NewLink => {
            let _ = dispatch_main_view(cx, |this, window, cx| this.open_composer_link(window, cx));
            cx.activate(true);
        }
        AppCommand::CaptureText(text) => {
            if let Some(view) = cx.try_global::<MainView>().map(|main| main.0.clone()) {
                let _ = view.update(cx, |this, cx| this.add_captured_text(text, cx));
            }
        }
        AppCommand::CaptureImage { path, mime_type } => {
            if let Some(view) = cx.try_global::<MainView>().map(|main| main.0.clone()) {
                let _ = view.update(cx, |this, cx| this.add_captured_image(path, mime_type, cx));
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
        visible.store(true, Ordering::SeqCst);
        cx.activate(true);
    }
}

fn open_window(cx: &mut App, visible: &Arc<AtomicBool>, service: &Arc<WorktableService>) {
    if cx.windows().is_empty() {
        if let Err(error) = open_main_window(cx, service.clone(), visible.clone()) {
            eprintln!("Worktable: failed to reopen window: {error:#}");
            return;
        }
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
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1080.), px(720.)),
                cx,
            ))),
            window_min_size: Some(size(px(900.), px(600.))),
            ..WindowOptions::default()
        },
        move |window, cx| {
            let visible_for_close = visible.clone();
            window.on_window_should_close(cx, move |_window, cx| {
                visible_for_close.store(false, Ordering::SeqCst);
                // Keep the process, menu-bar item, and global capture monitor
                // alive. Dock activation or Open Window can show it again.
                cx.hide();
                false
            });

            let view = cx.new(|cx| WorktableView::new(service.clone(), window, cx));
            cx.set_global(MainView(view.clone()));
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    Ok(())
}

/// Run a closure against the main `WorktableView` (if the window exists).
fn dispatch_main_view<R>(
    cx: &mut App,
    f: impl FnOnce(&mut WorktableView, &mut gpui::Window, &mut Context<WorktableView>) -> R,
) -> Option<R> {
    let view = cx.try_global::<MainView>()?.0.clone();
    let window = cx.active_window()?;
    let window = window.downcast::<Root>()?;
    window
        .update(cx, |_root, window, cx| {
            view.update(cx, |this, cx| f(this, window, cx))
        })
        .ok()
}
