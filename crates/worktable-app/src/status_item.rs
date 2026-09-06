//! macOS menu-bar (status item) integration.
//!
//! GPUI 0.2.x does not expose a public API for the macOS status bar, so we drive
//! `NSStatusItem` / `NSMenu` directly with `objc2`. The item shows a template
//! image that adapts to dark/light menu bars and, when clicked, presents a menu
//! whose items dispatch into the app over a simple command channel.

#![cfg(target_os = "macos")]

use std::{
    path::PathBuf,
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::service::{AppCommand, CommandSender};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSImage, NSImageScaling, NSMenu,
    NSMenuItem, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF,
    NSStatusBar,
};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
use objc2_foundation::{NSData, NSSize, NSString};

/// Global shift-held state updated by the FlagsChanged monitor.
/// Used by `worktable_view` to detect Shift+right-click.
static SHIFT_HELD: AtomicBool = AtomicBool::new(false);

/// A small Objective-C object that forwards menu clicks to the command sender.
struct MenuTargetIvars {
    sender: CommandSender,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "GPUIWorktableMenuTarget"]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(menuItemClicked:))]
        fn menu_item_clicked(&self, sender: &NSMenuItem) {
            let result = sender.tag();
            let command = match result {
                0 => AppCommand::OpenWindow,
                1 => AppCommand::ToggleWindow,
                2 => AppCommand::NewNote,
                3 => AppCommand::NewLink,
                4 => AppCommand::Quit,
                _ => return,
            };
            self.ivars().sender.send(command);
        }
    }
);

impl MenuTarget {
    fn new(sender: CommandSender) -> Retained<Self> {
        let this = Self::alloc().set_ivars(MenuTargetIvars { sender });
        unsafe { msg_send![super(this), init] }
    }
}

/// Create the status item in the macOS menu bar.
pub fn install(sender: CommandSender) -> Option<()> {
    let mtm = MainThreadMarker::new()?;

    let status_bar = NSStatusBar::systemStatusBar();
    // 22 is the standard menu bar icon slot size.
    let item = status_bar.statusItemWithLength(22.0);

    if let Some(button) = item.button(mtm) {
        if let Some(image) = make_template_image() {
            button.setImage(Some(&image));
            button.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        } else {
            // Keep a visible fallback if SF Symbols are unavailable in the
            // current runtime or application bundle.
            button.setTitle(&NSString::from_str("W"));
        }
        let tooltip = NSString::from_str("Worktable");
        button.setToolTip(Some(&tooltip));
    }

    let menu = build_menu(sender.clone(), mtm);
    item.setMenu(Some(&menu));
    item.setVisible(true);

    // Keep the native item alive for the lifetime of the process. The status
    // bar normally retains it too, but explicitly retaining it avoids the
    // icon disappearing when the local handle is released.
    std::mem::forget(item);
    install_shift_capture_monitor(sender);

    Some(())
}

struct ShiftTracker {
    count: u8,
    last_press: Option<Instant>,
    shift_active: bool,
}

impl ShiftTracker {
    fn register_press(&mut self) -> bool {
        const TRIPLE_PRESS_WINDOW: Duration = Duration::from_millis(700);
        let now = Instant::now();
        if self
            .last_press
            .map(|last| now.duration_since(last) > TRIPLE_PRESS_WINDOW)
            .unwrap_or(true)
        {
            self.count = 0;
        }

        self.count = self.count.saturating_add(1);
        self.last_press = Some(now);
        if self.count >= 3 {
            self.count = 0;
            self.last_press = None;
            true
        } else {
            false
        }
    }
}

/// Watch global Shift transitions and capture the foreground app's selection
/// after three quick presses. The returned monitor is intentionally leaked so
/// AppKit keeps it installed for the lifetime of the process.
fn install_shift_capture_monitor(sender: CommandSender) {
    let tracker = Arc::new(Mutex::new(ShiftTracker {
        count: 0,
        last_press: None,
        shift_active: false,
    }));
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| {
        let event = unsafe { event.as_ref() };
        if !matches!(event.keyCode(), 56 | 60) {
            return;
        }

        let shift_active = event.modifierFlags().contains(NSEventModifierFlags::Shift);
        // Publish global shift state for worktable_view's Shift+right-click handling.
        SHIFT_HELD.store(shift_active, Ordering::SeqCst);
        let should_capture = tracker
            .lock()
            .map(|mut tracker| {
                let pressed = shift_active && !tracker.shift_active;
                tracker.shift_active = shift_active;
                pressed && tracker.register_press()
            })
            .unwrap_or(false);

        if should_capture {
            capture_foreground_selection(sender.clone());
        }
    });

    let monitor =
        NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::FlagsChanged, &handler);
    if let Some(monitor) = monitor {
        std::mem::forget(monitor);
        std::mem::forget(handler);
    } else {
        eprintln!("Worktable: failed to install the global Shift monitor");
    }
}

fn capture_foreground_selection(sender: CommandSender) {
    thread::spawn(move || {
        let pasteboard = NSPasteboard::generalPasteboard();
        let before_count = pasteboard.changeCount();
        let string_type = unsafe { NSPasteboardTypeString };
        let previous_text = pasteboard
            .stringForType(string_type)
            .map(|value| value.to_string());

        post_copy_shortcut();
        thread::sleep(Duration::from_millis(80));

        let after_count = pasteboard.changeCount();
        if after_count == before_count {
            return;
        }

        // Try image first: check pasteboard for TIFF/PNG/JPEG data.
        if let Some((path, mime)) = try_capture_image_from_pasteboard(&pasteboard) {
            // For image capture we don't restore previous string — the image
            // replaces the pasteboard contents and we preserve the file on disk.
            sender.send(AppCommand::CaptureImage {
                path,
                mime_type: mime,
            });
            return;
        }

        let captured = pasteboard
            .stringForType(string_type)
            .map(|value| value.to_string())
            .filter(|value| !value.trim().is_empty());

        // Restore previous string content if we changed the pasteboard.
        if let Some(previous_text) = previous_text {
            let previous_text = NSString::from_str(&previous_text);
            let _ = pasteboard.setString_forType(&previous_text, string_type);
        }

        if let Some(text) = captured {
            sender.send(AppCommand::CaptureText(text));
        }
    });
}

/// Try to extract image data from the pasteboard, writing it to
/// `~/.worktable/images/<uuid>.<ext>` and returning (path, mime_type).
fn try_capture_image_from_pasteboard(pasteboard: &NSPasteboard) -> Option<(String, String)> {
    // Order matters: prefer TIFF first (most general), then PNG, then JPEG variants.
    let tiff_type = unsafe { NSPasteboardTypeTIFF };
    if let Some(data) = pasteboard.dataForType(tiff_type)
        && data.length() > 0
            && let Some(result) = write_image_data(&data, "tiff", "image/tiff") {
                return Some(result);
            }

    let png_type = unsafe { NSPasteboardTypePNG };
    if let Some(data) = pasteboard.dataForType(png_type)
        && data.length() > 0
            && let Some(result) = write_image_data(&data, "png", "image/png") {
                return Some(result);
            }

    // JPEG via UTI string "public.jpeg" (no dedicated constant in objc2-app-kit).
    let jpeg_type = NSString::from_str("public.jpeg");
    if let Some(data) = pasteboard.dataForType(&jpeg_type)
        && data.length() > 0
            && let Some(result) = write_image_data(&data, "jpg", "image/jpeg") {
                return Some(result);
            }
    // Alternate UTI "public.jpg" on some systems.
    let jpg_type = NSString::from_str("public.jpg");
    if let Some(data) = pasteboard.dataForType(&jpg_type)
        && data.length() > 0
            && let Some(result) = write_image_data(&data, "jpg", "image/jpeg") {
                return Some(result);
            }
    // HEIC / HEIF fallback
    let heic_type = NSString::from_str("public.heic");
    if let Some(data) = pasteboard.dataForType(&heic_type)
        && data.length() > 0
            && let Some(result) = write_image_data(&data, "heic", "image/heic") {
                return Some(result);
            }
    let heif_type = NSString::from_str("public.heif");
    if let Some(data) = pasteboard.dataForType(&heif_type)
        && data.length() > 0
            && let Some(result) = write_image_data(&data, "heif", "image/heif") {
                return Some(result);
            }

    None
}

fn write_image_data(data: &NSData, ext: &str, mime_type: &str) -> Option<(String, String)> {
    let bytes = data.to_vec();
    if bytes.is_empty() {
        return None;
    }
    let dir = resolve_images_dir()?;
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let filename = format!("{}.{}", uuid::Uuid::new_v4(), ext);
    let path = dir.join(&filename);
    if std::fs::write(&path, &bytes).is_ok() {
        Some((path.to_string_lossy().into_owned(), mime_type.to_owned()))
    } else {
        None
    }
}

fn resolve_images_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("WORKTABLE_IMAGES_DIR")
        && !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".worktable").join("images"))
}

fn post_copy_shortcut() {
    let Some(key_down) = CGEvent::new_keyboard_event(None, 8, true) else {
        return;
    };
    CGEvent::set_flags(Some(&key_down), CGEventFlags::MaskCommand);
    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&key_down));

    let Some(key_up) = CGEvent::new_keyboard_event(None, 8, false) else {
        return;
    };
    CGEvent::set_flags(Some(&key_up), CGEventFlags::MaskCommand);
    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&key_up));
}

fn build_menu(sender: CommandSender, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    unsafe {
        menu.setAutoenablesItems(false);

        let open = NSMenuItem::new(mtm);
        open.setTitle(&NSString::from_str("Open Window"));
        menu.addItem(&open);

        let show = NSMenuItem::new(mtm);
        show.setTitle(&NSString::from_str("Show / Hide Worktable"));
        menu.addItem(&show);

        let note = NSMenuItem::new(mtm);
        note.setTitle(&NSString::from_str("New Note"));
        note.setKeyEquivalent(&NSString::from_str("n"));
        note.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        menu.addItem(&note);

        let link = NSMenuItem::new(mtm);
        link.setTitle(&NSString::from_str("New Link"));
        link.setKeyEquivalent(&NSString::from_str("l"));
        link.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        menu.addItem(&link);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit = NSMenuItem::new(mtm);
        quit.setTitle(&NSString::from_str("Quit Worktable"));
        quit.setKeyEquivalent(&NSString::from_str("q"));
        quit.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        menu.addItem(&quit);

        // Tag each actionable item and point them at the target object.
        let target = MenuTarget::new(sender);
        let items = [&open, &show, &note, &link, &quit];
        for (index, ns_item) in items.iter().enumerate() {
            ns_item.setTag(index as isize);
            ns_item.setTarget(Some(&*target));
            ns_item.setAction(Some(sel!(menuItemClicked:)));
        }
        // NSMenuItem targets are not retained by AppKit. Keep the target alive
        // for the lifetime of the status menu.
        std::mem::forget(target);
    }
    menu
}

/// Build a monochrome (template) image from a named SF Symbol so it adapts to
/// the menu-bar appearance.
fn make_template_image() -> Option<Retained<NSImage>> {
    let _mtm = MainThreadMarker::new()?;
    let symbol = NSString::from_str("square.grid.2x2");
    let image: Retained<NSImage> =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol, None)?;
    image.setTemplate(true);
    image.setSize(NSSize::new(18.0, 18.0));
    Some(image)
}

/// Keep the app alive / present when the status item is used.
#[allow(dead_code)]
pub fn activate_app() {
    let mtm = MainThreadMarker::new();
    if let Some(mtm) = mtm {
        let app = NSApplication::sharedApplication(mtm);
        unsafe {
            let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
        }
    }
}
