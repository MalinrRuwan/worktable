//! macOS menu-bar (status item) integration.
//!
//! GPUI 0.2.x does not expose a public API for the macOS status bar, so we drive
//! `NSStatusItem` / `NSMenu` directly with `objc2`. The item shows a template
//! image that adapts to dark/light menu bars and, when clicked, presents a menu
//! whose items dispatch into the app over a simple command channel.

#![cfg(target_os = "macos")]

use std::{
    cell::RefCell,
    path::PathBuf,
    ptr::NonNull,
    sync::{Arc, Mutex},
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
    NSStatusBar, NSStatusItem,
};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
use objc2_foundation::{NSData, NSSize, NSString};

/// The brand glyph as a menu-bar template (alpha only, so macOS tints it for
/// light/dark menu bars).
const MENU_ICON: &[u8] = include_bytes!("../assets/menu_icon.png");
/// The "lit" glyph shown briefly after a triple-Shift capture.
const MENU_ICON_LIT: &[u8] = include_bytes!("../assets/menu_icon_lit.png");
/// The glyph plus a red dot, shown while macOS has not granted Accessibility.
/// It is not a template image: the dot must stay red on both menu-bar tones.
const MENU_ICON_WARNING: &[u8] = include_bytes!("../assets/menu_icon_warning.png");
/// The app icon shown in the Dock and the About panel.
const APP_ICON: &[u8] = include_bytes!("../assets/app_icon.png");

/// Menu-bar glyph size in points (the SVG is 861×692).
const MENU_ICON_SIZE: NSSize = NSSize {
    width: 18.0,
    height: 14.5,
};

/// Menu-bar presentation state shared by trust updates and capture feedback.
///
/// `lit` belongs to the two-blink capture animation; `accessibility_warning`
/// belongs to the permission state. Keeping both here means a blink that ends
/// while Accessibility is off returns to the warning glyph instead of the
/// plain template.
struct StatusState {
    accessibility_warning: bool,
    lit: bool,
}

thread_local! {
    /// The live status item, so capture feedback can swap its image.
    static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    static STATUS_STATE: RefCell<StatusState> = const {
        RefCell::new(StatusState {
            accessibility_warning: false,
            lit: false,
        })
    };
}

/// Whether the process is trusted for Accessibility (global capture and
/// synthesized copy events need it). Reads the same system flag that gates the
/// event tap below.
pub(crate) fn accessibility_trusted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

/// Open System Settings → Privacy & Security → Accessibility.
pub(crate) fn open_accessibility_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

/// Show the standard macOS About panel (App menu → About Worktable).
pub(crate) fn show_about_panel() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.orderFrontStandardAboutPanel(None);
    }
}

/// Hide every other application (App menu → Hide Others).
pub(crate) fn hide_other_applications() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.hideOtherApplications(None);
    }
}

/// Reveal all hidden applications (App menu → Show All).
pub(crate) fn show_all_applications() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.unhideAllApplications(None);
    }
}

/// Virtual keycodes we synthesise or inspect (HIToolbox values).
const KEYCODE_C: u16 = 8;
const KEYCODE_LEFT_SHIFT: u16 = 56;
const KEYCODE_RIGHT_SHIFT: u16 = 60;

/// How long the synthesized copy waits before reading the pasteboard.
const COPY_SETTLE: Duration = Duration::from_millis(80);

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
                2 => AppCommand::Quit,
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

/// Create the status item in the macOS menu bar. Returns `false` when the
/// process is not on the main thread or AppKit refuses the item; the caller
/// logs that and continues without a menu-bar presence.
pub fn install(sender: CommandSender) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };

    let status_bar = NSStatusBar::systemStatusBar();
    // 22 is the standard menu bar icon slot size.
    let item = status_bar.statusItemWithLength(22.0);

    // Read the grant before the first frame so an untrusted app opens with the
    // red dot already visible instead of correcting itself on the next poll.
    let accessibility_warning = !accessibility_trusted();
    STATUS_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.accessibility_warning = accessibility_warning;
        state.lit = false;
    });

    if let Some(button) = item.button(mtm) {
        let image = if accessibility_warning {
            make_warning_image()
        } else {
            make_template_image()
        };
        if let Some(image) = image {
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

    // Keep the native item alive for the lifetime of the process (the status
    // bar normally retains it too) and make it reachable for capture feedback.
    STATUS_ITEM.with(|slot| *slot.borrow_mut() = Some(item.clone()));
    std::mem::forget(item);
    install_shift_capture_monitor(sender);

    true
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
        if !matches!(event.keyCode(), KEYCODE_LEFT_SHIFT | KEYCODE_RIGHT_SHIFT) {
            return;
        }

        let shift_active = event.modifierFlags().contains(NSEventModifierFlags::Shift);
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
        crate::logging::error("failed to install the global Shift monitor");
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
        thread::sleep(COPY_SETTLE);

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
        && let Some(result) = write_image_data(&data, "tiff", "image/tiff")
    {
        return Some(result);
    }

    let png_type = unsafe { NSPasteboardTypePNG };
    if let Some(data) = pasteboard.dataForType(png_type)
        && data.length() > 0
        && let Some(result) = write_image_data(&data, "png", "image/png")
    {
        return Some(result);
    }

    // JPEG via UTI string "public.jpeg" (no dedicated constant in objc2-app-kit).
    let jpeg_type = NSString::from_str("public.jpeg");
    if let Some(data) = pasteboard.dataForType(&jpeg_type)
        && data.length() > 0
        && let Some(result) = write_image_data(&data, "jpg", "image/jpeg")
    {
        return Some(result);
    }
    // Alternate UTI "public.jpg" on some systems.
    let jpg_type = NSString::from_str("public.jpg");
    if let Some(data) = pasteboard.dataForType(&jpg_type)
        && data.length() > 0
        && let Some(result) = write_image_data(&data, "jpg", "image/jpeg")
    {
        return Some(result);
    }
    // HEIC / HEIF fallback
    let heic_type = NSString::from_str("public.heic");
    if let Some(data) = pasteboard.dataForType(&heic_type)
        && data.length() > 0
        && let Some(result) = write_image_data(&data, "heic", "image/heic")
    {
        return Some(result);
    }
    let heif_type = NSString::from_str("public.heif");
    if let Some(data) = pasteboard.dataForType(&heif_type)
        && data.length() > 0
        && let Some(result) = write_image_data(&data, "heif", "image/heif")
    {
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
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".worktable").join("images"))
}

fn post_copy_shortcut() {
    let Some(key_down) = CGEvent::new_keyboard_event(None, KEYCODE_C, true) else {
        return;
    };
    CGEvent::set_flags(Some(&key_down), CGEventFlags::MaskCommand);
    CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&key_down));

    let Some(key_up) = CGEvent::new_keyboard_event(None, KEYCODE_C, false) else {
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

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit = NSMenuItem::new(mtm);
        quit.setTitle(&NSString::from_str("Quit Worktable"));
        quit.setKeyEquivalent(&NSString::from_str("q"));
        quit.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        menu.addItem(&quit);

        // Tag each actionable item and point them at the target object.
        let target = MenuTarget::new(sender);
        let items = [&open, &show, &quit];
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

/// Decode an embedded PNG into an `NSImage` of the given point size.
fn embedded_image(bytes: &'static [u8], template: bool) -> Option<Retained<NSImage>> {
    let data = NSData::with_bytes(bytes);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    image.setTemplate(template);
    image.setSize(MENU_ICON_SIZE);
    Some(image)
}

/// The monochrome template glyph: macOS tints it for the menu-bar appearance.
fn make_template_image() -> Option<Retained<NSImage>> {
    embedded_image(MENU_ICON, true)
}

/// The lit glyph (warm, with a halo) shown as capture feedback.
fn make_lit_image() -> Option<Retained<NSImage>> {
    embedded_image(MENU_ICON_LIT, false)
}

/// The glyph with a red dot, shown while Accessibility is missing. A
/// non-template image keeps the dot red on light and dark menu bars.
fn make_warning_image() -> Option<Retained<NSImage>> {
    embedded_image(MENU_ICON_WARNING, false)
}

/// Show or hide the app's Dock presence. Closing the window in background
/// mode switches to `Accessory` (menu bar only); opening the window restores
/// `Regular`.
pub(crate) fn set_dock_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible {
        objc2_app_kit::NSApplicationActivationPolicy::Regular
    } else {
        objc2_app_kit::NSApplicationActivationPolicy::Accessory
    };
    if !app.setActivationPolicy(policy) {
        crate::logging::error(format!(
            "failed to switch the activation policy (visible: {visible})"
        ));
    }
}

/// Bring the app back from background mode: unhide first, activate, then
/// restore the `Regular` policy. AppKit defers the Dock tile when the policy
/// changes while the app is hidden, so the order matters — doing this before
/// `activate` leaves the icon missing.
// `NSApplication.activate()` is macOS 14+; the bundle targets 13.0, so keep
// the availability-safe call (the same one GPUI itself makes).
#[allow(deprecated)]
pub(crate) fn restore_dock_and_unhide() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.unhide(None);
    app.activateIgnoringOtherApps(true);
    let _ = app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Regular);
}

/// Set the application icon (Dock, app switcher, About panel).
pub(crate) fn set_app_icon() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(image) = embedded_image(APP_ICON, false) else {
        return;
    };
    // 512pt is the customary app-icon point size; AppKit scales it for the
    // Dock, app switcher, and About panel.
    image.setSize(NSSize::new(512.0, 512.0));
    let app = NSApplication::sharedApplication(mtm);
    unsafe { app.setApplicationIconImage(Some(&image)) };
}

/// Show or hide the installed menu-bar item immediately. The item stays
/// installed (and the global capture monitor keeps running) so turning the
/// preference back on does not need a relaunch.
pub(crate) fn set_visible(visible: bool) {
    let Some(_mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS_ITEM.with(|slot| {
        let Ok(item) = slot.try_borrow() else {
            return;
        };
        if let Some(item) = item.as_ref() {
            item.setVisible(visible);
        }
    });
}

/// Show or hide the Accessibility warning without waiting for the next
/// capture blink. Main thread only; a no-op before `install` creates the item.
pub(crate) fn set_accessibility_warning(trusted: bool) {
    let (warning, lit) = STATUS_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.accessibility_warning = !trusted;
        (state.accessibility_warning, state.lit)
    });
    apply_status_image(warning, lit);
}

/// Swap the menu-bar image to the lit glyph (or back). Main thread only.
fn set_status_image(lit: bool) {
    let warning = STATUS_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.lit = lit;
        state.accessibility_warning
    });
    apply_status_image(warning, lit);
}

/// Which glyph the menu bar should show for the current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusImage {
    Lit,
    Warning,
    Template,
}

/// An in-progress blink wins, then the Accessibility warning, then the
/// ordinary template glyph.
fn status_image_kind(warning: bool, lit: bool) -> StatusImage {
    if lit {
        StatusImage::Lit
    } else if warning {
        StatusImage::Warning
    } else {
        StatusImage::Template
    }
}

/// Paint the current state: an in-progress blink wins, then the Accessibility
/// warning, then the ordinary template glyph.
fn apply_status_image(warning: bool, lit: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    STATUS_ITEM.with(|slot| {
        let Ok(item) = slot.try_borrow() else {
            return;
        };
        let Some(item) = item.as_ref() else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };
        let image = match status_image_kind(warning, lit) {
            StatusImage::Lit => make_lit_image(),
            StatusImage::Warning => make_warning_image(),
            StatusImage::Template => make_template_image(),
        };
        if let Some(image) = image {
            button.setImage(Some(&image));
        }
    });
}

/// Feedback for a captured selection: blink the menu-bar glyph twice so the
/// user sees the lap light up. Runs on the GPUI main thread; the timers keep
/// frames/events flowing without touching AppKit from another thread. Each
/// off beat returns to the warning glyph while Accessibility is missing.
pub(crate) fn flash_capture_feedback(cx: &mut gpui::App) {
    const ON: Duration = Duration::from_millis(130);
    const OFF: Duration = Duration::from_millis(130);
    cx.spawn(async move |cx| {
        for _ in 0..2 {
            set_status_image(true);
            cx.background_executor().timer(ON).await;
            set_status_image(false);
            cx.background_executor().timer(OFF).await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The warning glyph is a real 2x asset with a red dot and neutral lamp
    /// pixels; a template image would tint the dot away, so the decode also
    /// proves the dot survives as color.
    #[test]
    fn status_image_prefers_blink_then_warning_then_template() {
        assert_eq!(status_image_kind(true, true), StatusImage::Lit);
        assert_eq!(status_image_kind(false, true), StatusImage::Lit);
        assert_eq!(status_image_kind(true, false), StatusImage::Warning);
        assert_eq!(status_image_kind(false, false), StatusImage::Template);
    }

    #[test]
    fn warning_menu_icon_keeps_a_red_dot() {
        let image = image::load_from_memory(MENU_ICON_WARNING).expect("warning icon decodes");
        let rgba = image.to_rgba8();
        assert_eq!(
            rgba.dimensions(),
            (72, 58),
            "the warning glyph stays at the menu bar's 2x size"
        );

        let mut red = 0;
        let mut neutral = 0;
        for pixel in rgba.pixels() {
            let [r, g, b, a] = pixel.0;
            if a > 180 && r > 150 && r > g.saturating_add(50) && r > b.saturating_add(50) {
                red += 1;
            }
            if a > 100 && r.abs_diff(g) < 12 && r.abs_diff(b) < 12 {
                neutral += 1;
            }
        }
        assert!(red > 40, "the red dot is visible ({red} red pixels)");
        assert!(
            neutral > 200,
            "the lamp glyph keeps neutral strokes ({neutral} neutral pixels)"
        );
    }
}
