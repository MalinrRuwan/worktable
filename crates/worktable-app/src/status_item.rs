//! macOS menu-bar (status item) integration.
//!
//! GPUI 0.2.x does not expose a public API for the macOS status bar, so we drive
//! `NSStatusItem` / `NSMenu` directly with `objc2`. The item shows a template
//! image that adapts to dark/light menu bars and, when clicked, presents a menu
//! whose items dispatch into the app over a simple command channel.

#![cfg(target_os = "macos")]

use crate::service::AppCommand;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSImage, NSMenuItem, NSMenu, NSStatusBar, NSStatusItem,
};
use objc2_foundation::{NSPoint, NSSize, NSString};
use std::sync::Arc;

/// A small Objective-C object that forwards menu clicks to the command sender.
struct MenuTargetIvars {
    sender: Arc<crate::service::CommandSender>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "GPUIWorktableMenuTarget"]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(menuItemClicked:))]
        fn menu_item_clicked(&self, sender: &NSMenuItem) {
            let result = unsafe { sender.tag() };
            let ivar = self.ivars().sender.clone();
            let command = match result {
                0 => AppCommand::ToggleWindow,
                1 => AppCommand::NewNote,
                2 => AppCommand::NewLink,
                3 => AppCommand::Quit,
                _ => return,
            };
            ivar.send(command);
        }
    }
);

impl MenuTarget {
    fn new(sender: Arc<crate::service::CommandSender>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(MenuTargetIvars { sender });
        unsafe { msg_send![super(this), init] }
    }
}

/// Create the status item in the macOS menu bar.
pub fn install(sender: Arc<crate::service::CommandSender>) -> Option<()> {
    let mtm = MainThreadMarker::new()?;

    unsafe {
        let status_bar = NSStatusBar::systemStatusBar();
        // 22 is the standard menu bar icon slot size.
        let item = NSStatusItem::alloc(mtm);
        let item: Retained<NSStatusItem> =
            msg_send![item, initWithStatusBar: &*status_bar, length: 22.0];

        let button = item.button();
        if let Some(image) = make_template_image() {
            button.setImage(Some(&image));
            button.setImageScaling(1); // NSImageScaleProportionallyDown
        }
        button.setToolTip(&NSString::from_str("Worktable"));

        let menu = build_menu(sender, mtm);
        item.setMenu(Some(&menu));

        // Keep the status item alive. `NSStatusItem` returned by the system is
        // retained by the status bar, and `item` past this function is dropped;
        // the strong reference held by `NSStatusBar` keeps it installed. Because
        // the item object is already permanently owned by the status bar, we
        // simply forget our local handle.
        let _ = item;
    }

    Some(())
}

fn build_menu(sender: Arc<crate::service::CommandSender>, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    unsafe {
        menu.setAutoenablesItems(false);

        let show = NSMenuItem::new(mtm);
        show.setTitle(&NSString::from_str("Show / Hide Worktable"));
        menu.addItem(&show);

        let note = NSMenuItem::new(mtm);
        note.setTitle(&NSString::from_str("New Note"));
        note.setKeyEquivalent(&NSString::from_str("n"));
        note.setKeyEquivalentModifierMask(1 << 20); // Command
        menu.addItem(&note);

        let link = NSMenuItem::new(mtm);
        link.setTitle(&NSString::from_str("New Link"));
        link.setKeyEquivalent(&NSString::from_str("l"));
        link.setKeyEquivalentModifierMask(1 << 20);
        menu.addItem(&link);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit = NSMenuItem::new(mtm);
        quit.setTitle(&NSString::from_str("Quit Worktable"));
        quit.setKeyEquivalent(&NSString::from_str("q"));
        quit.setKeyEquivalentModifierMask(1 << 20);
        menu.addItem(&quit);

        // Tag each actionable item and point them at the target object.
        let target = MenuTarget::new(sender);
        let items = [&show, &note, &link, &quit];
        for (index, ns_item) in items.iter().enumerate() {
            ns_item.setTag(index as isize);
            let target_any: *const AnyObject = target.as_ref().as_ptr();
            ns_item.setTarget(Some(&*target_any));
            ns_item.setAction(Some(sel!(menuItemClicked:)));
        }
    }
    menu
}

/// Build a monochrome (template) image from a named SF Symbol so it adapts to
/// the menu-bar appearance.
fn make_template_image() -> Option<Retained<NSImage>> {
    let mtm = MainThreadMarker::new()?;
    let symbol = NSString::from_str("square.grid.2x2");
    unsafe {
        let image = NSImage::alloc(mtm);
        let image: Retained<NSImage> =
            msg_send![image, initWithSymbolConfiguration: nil];
        let image: Retained<NSImage> = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &symbol,
            None,
        )?;
        image.setTemplate(true);
        image.setSize(NSSize::new(18.0, 18.0));
        let _ = image;
        Some(image)
    }
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
