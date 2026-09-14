//! Real-render visual test runner for Worktable.
//!
//! Mirrors Zed's `zed_visual_test_runner`: macOS-only, opens windows **off-screen**
//! with the real Metal renderer (`VisualTestAppContext`), simulates pointer clicks
//! and keystrokes, and captures PNG frames to `target/visual_tests/` so the UI can
//! be inspected visually and diffed against baselines.
//!
//! GPUI's `VisualTestAppContext` uses the real `MacPlatform`; windows are placed
//! at (-10000, -10000) so nothing is shown on screen while the compositor still
//! renders every frame. It must run on the process main thread, which is why this
//! is a standalone binary (like Zed's runner) rather than a `#[test]` — libtest
//! runs tests on worker threads and macOS AppKit aborts there.
//!
//! ## Usage
//!
//! ```sh
//! # run the flow + write screenshots
//! cargo run -p worktable-app --bin worktable_visual_test --features visual-tests
//!
//! # refresh PNG baselines (when the UI intentionally changes)
//! UPDATE_BASELINE=1 cargo run -p worktable-app --bin worktable_visual_test --features visual-tests
//! ```
//!
//! ## Environment
//!
//! - `UPDATE_BASELINE=1` — save captures as baselines instead of only output.
//! - `VISUAL_TEST_OUTPUT_DIR` — output directory (default `target/visual_tests`).

const DEMO_PNG: &[u8] = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x08\x00\x00\x00\x08\x08\x02\x00\x00\x00\x4b\x6d\x29\xdc\x00\x00\x00\x25\x49\x44\x41\x54\x78\x9c\x63\x60\x60\x60\x50\x50\x50\x70\x70\x70\x48\x48\x48\x68\x68\x68\x58\xb0\x60\xc1\x81\x03\x07\x1e\x3c\x78\xc0\x30\xb4\x24\x00\x58\x99\x54\x01\x67\xf0\x51\x8b\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82";

#[path = "../actions.rs"]
mod actions;
#[path = "../assets.rs"]
mod assets;
#[path = "../assistant.rs"]
mod assistant;
#[path = "../design.rs"]
mod design;
#[path = "../entry_actions.rs"]
mod entry_actions;
#[path = "../format.rs"]
mod format;
#[path = "../github.rs"]
mod github;
#[path = "../preferences.rs"]
mod preferences;
// The runner mirrors the app's modules but drives only the visual flow, so
// production entry points (status-item install, capture commands) and some
// view actions are intentionally unused here. The main binary build still
// checks them with dead-code warnings enabled.
#[path = "../service.rs"]
#[allow(dead_code)]
mod service;
#[path = "../status_item.rs"]
#[allow(dead_code)]
mod status_item;
#[path = "../worktable_view.rs"]
#[allow(dead_code)]
mod worktable_view;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{AppContext as _, Modifiers, VisualTestAppContext, point, px};
use gpui_component::{Root, WindowExt as _};
use worktable_ai::WorktableEntry;

use service::WorktableService;
use worktable_view::{AppMode, WorktableView};

const WINDOW_SIZE: gpui::Size<gpui::Pixels> = gpui::Size {
    width: px(390.),
    height: px(884.),
};

/// Header row: 30px titlebar inset + py_2 (8) + 40px menu button → y ≈ 58.
/// The menu button is the trailing flex item: 390 - 16 - 20 = 354.
const MENU_BUTTON_CENTER: (f32, f32) = (354.0, 58.0);

fn sample_entries() -> Vec<WorktableEntry> {
    fn entry(content: &str, created_at: i64) -> WorktableEntry {
        WorktableEntry {
            id: format!("visual-{created_at}"),
            content: content.to_owned(),
            title: None,
            source: "Worktable".to_owned(),
            created_at,
        }
    }

    vec![
        entry(
            "Negation in inherited configs. The moment a config can extend a base or preset, \
             someone needs to remove an extension the...",
            1_000,
        ),
        entry(
            "Use TOML as the default declarative format, backed by a published schema",
            900,
        ),
        entry(
            "gitignore — universal ignore rules for versioned files",
            800,
        ),
        // Multiline entries (as captured from real selections) — regression
        // cover for card overlap: clamped multi-line bodies must stay inside
        // their fixed-height virtual-list row.
        entry(
            "Elon Musk\n\n@elonmusk\n·\n5h\nTry Grok 4.6 using the Grok Build harness or Cursor app for max usefulness",
            950,
        ),
        entry("fda", 975),
        // Layout stress: a title, an unbreakable URL, wide glyphs, and an
        // image with a long filename must all stay inside their rows.
        WorktableEntry {
            id: "visual-titled".to_owned(),
            content: "Short body under a title".to_owned(),
            title: Some("Design decisions".to_owned()),
            source: "Worktable".to_owned(),
            created_at: 850,
        },
        entry(
            "https://example.com/a/really/long/unbroken/path/segment/that/keeps/going/and/going/and/going/without/any/spaces/at/all?with=query&and=parameters",
            825,
        ),
        entry("深色模式下的界面调整 — ノートのレイアウト確認 🎨✨", 812),
        entry(
            "/tmp/demo_photo_with_a_very_long_filename_for_layout_checking_worktable.png",
            805,
        ),
    ]
}

/// Seed a throwaway SQLite database with `entries` and return its path.
fn seeded_db(entries: Vec<WorktableEntry>) -> String {
    let path = std::env::temp_dir().join(format!("worktable-visual-{}.db", uuid::Uuid::new_v4()));
    let path_str = path.to_string_lossy().into_owned();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt
        .block_on(worktable_ai::WorktableRuntime::connect(&path_str))
        .unwrap();
    for entry in entries {
        rt.block_on(runtime.insert_entry(&entry)).unwrap();
    }
    // Captures drive the main UI; the tour has its own dedicated steps.
    runtime.set_config("onboarding_completed", "1").unwrap();
    drop(runtime);

    path_str
}

/// Load `themes/ayu.json` into the component theme registry, mirroring
/// `main::init_theme` but synchronously (the runner resolves the path relative
/// to the manifest dir, not the process CWD).
fn load_worktable_theme(cx: &mut gpui::App) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes/ayu.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        eprintln!("visual test: themes/ayu.json not found, using default theme");
        return;
    };
    if gpui_component::ThemeRegistry::global_mut(cx)
        .load_themes_from_str(&contents)
        .is_ok()
    {
        // Both variants: dark mode must use Ayu Dark, not the default dark
        // palette (whose list selection is blue).
        let light = gpui_component::ThemeRegistry::global(cx)
            .themes()
            .get("Ayu Light")
            .cloned();
        let dark = gpui_component::ThemeRegistry::global(cx)
            .themes()
            .get("Ayu Dark")
            .cloned();
        if let Some(light) = light {
            gpui_component::Theme::global_mut(cx).apply_config(&light);
        }
        if let Some(dark) = dark {
            gpui_component::Theme::global_mut(cx).apply_config(&dark);
        }
        let mode = gpui_component::Theme::global(cx).mode;
        gpui_component::Theme::change(mode, None, cx);
        // Same Tahoe radius scale as `main::init_theme`.
        let t = gpui_component::Theme::global_mut(cx);
        t.radius = px(10.);
        t.radius_lg = px(14.);
    }
}

/// Capture the current frame of `window` to `<output_dir>/<name>.png`.
fn capture(
    cx: &mut VisualTestAppContext,
    window_handle: gpui::AnyWindowHandle,
    name: &str,
) -> anyhow::Result<()> {
    // Let the splash / entrance animations finish (real wall time — the view
    // keys its splash off `Instant`, not the test clock).
    std::thread::sleep(std::time::Duration::from_millis(900));
    cx.update_window(window_handle, |_, window, _| window.refresh())
        .ok();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window_handle)?;
    let (w, h) = image.dimensions();
    // Off-screen windows render at the platform scale factor (2x on retina),
    // so wide 1200pt windows capture at 2400px — assert generous bounds only.
    anyhow::ensure!(
        (380..=2400).contains(&w) && (600..=2400).contains(&h),
        "unexpected capture size: {w}x{h}"
    );
    println!("    capture {name}: {w}x{h}");

    let output_dir = std::env::var("VISUAL_TEST_OUTPUT_DIR")
        .unwrap_or_else(|_| "target/visual_tests".to_string());
    let dir = std::path::PathBuf::from(&output_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.png"));
    image.save(&path)?;
    println!("  ✓ {name}: {}", path.display());
    Ok(())
}

/// The full Figma-flow run:
/// entries → hamburger menu → Settings → ⌘2 assistant → Ask Agent, capturing a
/// PNG at every step and asserting the mode transitions.
fn run_visual_tests() -> anyhow::Result<()> {
    let db = seeded_db(sample_entries());
    let service = Arc::new(WorktableService::new_for_test(&db)?);

    let mut cx = VisualTestAppContext::with_asset_source(
        gpui_platform::current_platform(false),
        Arc::new(assets::AppAssets),
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(bindings());
        load_worktable_theme(cx);
    });
    // Config/github/provider reads complete on real Tokio worker threads and
    // wake GPUI futures off-thread; parking must be allowed for that.
    cx.background_executor.allow_parking();
    cx.run_until_parked();

    let holder: Rc<RefCell<Option<gpui::Entity<WorktableView>>>> = Rc::new(RefCell::new(None));
    let holder_for_window = holder.clone();
    let service_for_window = service.clone();
    let window = cx.open_offscreen_window(WINDOW_SIZE, move |window, cx| {
        let view = cx.new(|cx| WorktableView::new(service_for_window.clone(), window, cx));
        view.update(cx, |this, cx| {
            this.set_theme_mode(worktable_view::AppThemeMode::Light, window, cx)
        });
        *holder_for_window.borrow_mut() = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    let view = holder.borrow().clone().expect("view was built");
    let handle: gpui::AnyWindowHandle = window.into();

    cx.update_window(handle, |_, window, _| window.refresh())
        .ok();
    cx.run_until_parked();

    println!("— step 1: entries —");
    assert_eq!(cx.read_entity(&view, |v, _| v.entries.len()), 9);
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
    capture(&mut cx, handle, "worktable_entries")?;

    println!("— step 1a: onboarding — welcome, accessibility, provider —");
    cx.update(|cx| {
        view.update(cx, |this, cx| this.start_onboarding(cx));
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(200));
    capture(&mut cx, handle, "worktable_onboarding")?;
    cx.update(|cx| {
        view.update(cx, |this, cx| this.advance_onboarding(cx));
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(150));
    capture(&mut cx, handle, "worktable_onboarding_accessibility")?;
    cx.update(|cx| {
        view.update(cx, |this, cx| this.advance_onboarding(cx));
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(150));
    capture(&mut cx, handle, "worktable_onboarding_provider")?;
    cx.update(|cx| {
        view.update(cx, |this, cx| this.skip_onboarding(cx));
    });
    cx.run_until_parked();

    println!("— step 1b: entry detail morph —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            let id = this
                .entries
                .first()
                .map(|entry| entry.id.clone())
                .unwrap_or_default();
            // Long content proves the detail body scrolls. The real card's
            // rect (recorded at prepaint) is the morph origin.
            if let Some(entry) = this.entries.iter_mut().find(|entry| entry.id == id) {
                entry.content = (0..140)
                    .map(|line| format!("line {line} of a long captured note"))
                    .collect::<Vec<_>>()
                    .join("\n");
                // The detail capture also proves the selectable title box.
                entry.title = Some("Design decisions".to_owned());
            }
            let origin = this.entry_origin(&id);
            this.open_entry_modal(&id, origin, cx);
        });
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_entry_modal")?;
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.close_entry_modal(cx);
            // Restore the short content so later captures are unchanged.
            if let Some(entry) = this.entries.first_mut() {
                entry.content =
                    "Negation in inherited configs. The moment a config can extend a base or \
                     preset, someone needs to remove an extension the..."
                        .to_owned();
                entry.title = None;
            }
        });
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.run_until_parked();

    println!("— step 1c: entry detail editor —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            let id = this
                .entries
                .first()
                .map(|entry| entry.id.clone())
                .unwrap_or_default();
            let origin = this.entry_origin(&id);
            this.open_entry_modal(&id, origin, cx);
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(400));
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |this, cx| this.start_entry_edit(window, cx));
    })
    .ok();
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_entry_editor")?;
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.cancel_entry_edit(cx);
            this.close_entry_modal(cx);
        });
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.run_until_parked();

    println!("— step 2: menu button opens the library menu —");
    cx.simulate_click(
        handle,
        point(px(MENU_BUTTON_CENTER.0), px(MENU_BUTTON_CENTER.1)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_menu_open")?;

    println!("— step 3: Settings navigates (first menu item: Down, Enter) —");
    cx.simulate_keystrokes(handle, "down");
    cx.simulate_keystrokes(handle, "enter");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);
    capture(&mut cx, handle, "worktable_settings")?;

    println!("— step 3-general: General category — background toggle —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.show_settings_at(worktable_view::SettingsTab::General, cx)
        });
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_settings_general")?;

    println!("— step 3a: Appearance category — theme + welcome tour —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.show_settings_at(worktable_view::SettingsTab::Appearance, cx)
        });
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_settings_appearance")?;

    println!("— step 3b: Data category → GitHub stars dialog —");
    // Same handler the category row calls (the runner only needs the capture).
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.show_settings_at(worktable_view::SettingsTab::Data, cx)
        });
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_settings_data")?;
    // Configure opens the GitHub dialog over Settings (same handler the
    // button calls).
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |this, cx| this.open_github_dialog(window, cx));
    })
    .ok();
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Settings,
        "the dialog overlays Settings"
    );
    capture(&mut cx, handle, "worktable_github_dialog")?;
    cx.update_window(handle, |_, window, cx| window.close_dialog(cx))
        .ok();
    cx.run_until_parked();

    println!("— step 3c: provider configuration dialog —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.show_settings_at(worktable_view::SettingsTab::Providers, cx)
        });
    });
    cx.run_until_parked();
    let provider_id = cx.read_entity(&view, |v, _| v.providers.first().map(|p| p.id.clone()));
    if let Some(provider_id) = provider_id {
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |this, cx| {
                this.open_provider_dialog(&provider_id, window, cx)
            });
        })
        .ok();
        cx.run_until_parked();
        capture(&mut cx, handle, "worktable_provider_dialog")?;
        // Close it again so the following steps start from plain Settings.
        cx.update_window(handle, |_, window, cx| window.close_dialog(cx))
            .ok();
        cx.run_until_parked();
    }

    println!("— step 4: ⌘2 jumps to the assistant —");
    // Ensure the worktable view is focused so global keybindings resolve (Settings contains inputs).
    cx.update_window(handle, |_, window, cx| {
        let fh = view.read(cx).focus_handle.clone();
        window.focus(&fh, cx);
    })
    .ok();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle, "cmd-2");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Assistant);
    capture(&mut cx, handle, "worktable_assistant")?;

    println!("— step 5: ⌘1 back to entries, page toggle → assistant —");
    cx.update_window(handle, |_, window, cx| {
        let fh = view.read(cx).focus_handle.clone();
        window.focus(&fh, cx);
    })
    .ok();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle, "cmd-1");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
    // Same handler the header's page toggle calls; interaction tests cover
    // the pointer path.
    cx.update(|cx| {
        view.update(cx, |this, cx| this.show_assistant(cx));
    });
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Assistant);
    capture(&mut cx, handle, "worktable_ask_agent")?;

    // Back to entries for the remaining visual checks (sorting, topics, composer, helix + photos).
    cx.update_window(handle, |_, window, cx| {
        let fh = view.read(cx).focus_handle.clone();
        window.focus(&fh, cx);
    })
    .ok();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle, "cmd-1");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);

    println!("— step 6: sort A–Z (ButtonGroup) —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.set_sort_mode(worktable_view::SortMode::Alpha, cx)
        });
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, _| window.refresh())
        .ok();
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_sort_alpha")?;

    println!("— step 7: group by Topic —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.set_sort_mode(worktable_view::SortMode::Topic, cx)
        });
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, _| window.refresh())
        .ok();
    cx.run_until_parked();
    {
        let count = cx.read_entity(&view, |v, _| v.visible_entries().len());
        println!("    topic visible count: {}", count);
        let topics: Vec<String> = cx.read_entity(&view, |v, _| {
            v.visible_entries()
                .iter()
                .map(|e| worktable_view::helix_primary_topic(e))
                .collect()
        });
        println!("    topics: {:?}", topics);
    }
    capture(&mut cx, handle, "worktable_topic")?;

    println!("— step 8: add photo (image entry) → knowledge topic —");
    let _ = std::fs::write("/tmp/demo_photo_sunset.png", DEMO_PNG);
    let _ = std::fs::write(
        "/tmp/demo_photo_with_a_very_long_filename_for_layout_checking_worktable.png",
        DEMO_PNG,
    );
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.add_captured_image(
                "/tmp/demo_photo_sunset.png".to_string(),
                "image/png".to_string(),
                cx,
            )
        });
    });
    cx.run_until_parked();
    // Give the knowledge sync thread a moment (allow_parking already set)
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.run_until_parked();
    // Back to Time so the new photo appears at top
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.set_sort_mode(worktable_view::SortMode::Time, cx)
        });
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_image_entry")?;
    // Open the photo's detail: the image itself with the hover download
    // affordance (the modal capture drives the real card -> panel morph).
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            let id = this
                .entries
                .iter()
                .find(|entry| entry.content.contains("demo_photo"))
                .map(|entry| entry.id.clone());
            if let Some(id) = id {
                this.open_entry_modal(
                    &id,
                    gpui::Bounds::new(
                        gpui::point(gpui::px(40.), gpui::px(120.)),
                        gpui::size(gpui::px(280.), gpui::px(108.)),
                    ),
                    cx,
                );
            }
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(350));
    capture(&mut cx, handle, "worktable_image_modal")?;
    cx.update(|cx| {
        view.update(cx, |this, cx| this.close_entry_modal(cx));
    });
    cx.run_until_parked();
    // Verify the knowledge graph contains the photo via its title/path
    {
        let db_path = cx.read_entity(&view, |v, _| v.service.database_path().to_owned());
        let helix_path = worktable_helix::helix_path_for_sqlite(&db_path);
        let client = worktable_helix::HelixClient::open_embedded(helix_path);
        let hits = client.search_blocking("demo_photo", 10).unwrap_or_default();
        assert!(!hits.is_empty(), "Helix should contain the photo entry");
        println!("    helix photo hits: {}", hits.len());
    }

    println!("— step 9: note bar — focused input —");
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |this, cx| this.focus_composer(window, cx));
    })
    .ok();
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(200));
    capture(&mut cx, handle, "worktable_composer_focused")?;

    println!("— step 10: multi-select (cmd + shift) —");
    // Select first, cmd-add second, shift-range to third if available
    cx.update(|cx| {
        view.update(cx, |this, _| {
            let ids: Vec<String> = this.visible_entry_ids();
            if ids.len() >= 2 {
                this.select_at(ids[0].clone(), false);
                this.select_at(ids[1].clone(), true);
            }
        });
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_multiselect")?;

    // ---- Agent loading states (Thinking dots + bobbing waiting dots) --------
    println!("— step 11: assistant loading states —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::{ChatMessage, Role};
            this.messages = vec![
                ChatMessage::user("Summarize my notes"),
                ChatMessage {
                    role: Role::Assistant,
                    text: String::new(),
                    thinking: "Looking through the entries to find the common themes…".into(),
                    streaming: true,
                    citations: Vec::new(),
                    thinking_collapsed: false,
                },
            ];
            this.assistant_busy = true;
            // The reasoning block renders only with the Settings → UI toggle on.
            this.show_thinking = true;
            this.show_assistant(cx);
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(400));
    capture(&mut cx, handle, "worktable_agent_thinking")?;

    // Waiting-for-first-token state: the user message is in, no assistant
    // bubble yet — the S1 orb gives the status.
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::ChatMessage;
            this.messages = vec![ChatMessage::user("Summarize my notes")];
            this.assistant_busy = true;
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(400));
    capture(&mut cx, handle, "worktable_agent_waiting")?;

    println!("— step 11b: streaming answer (typewriter + caret) —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::{ChatMessage, Role};
            this.messages = vec![
                ChatMessage::user("Explain attention"),
                ChatMessage {
                    role: Role::Assistant,
                    text: "Transformers scale well with data and compute, though attention is \
                           quadratic in sequence length. The answer types out as the model \
                           streams, with the caret holding the end of the line."
                        .into(),
                    thinking: String::new(),
                    streaming: true,
                    citations: Vec::new(),
                    thinking_collapsed: false,
                },
            ];
            this.assistant_busy = true;
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(700));
    capture(&mut cx, handle, "worktable_agent_streaming")?;

    println!("— step 11c: inline citations —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::ChatMessage;
            this.assistant_busy = false;
            this.messages = vec![
                ChatMessage::user("Explain attention"),
                ChatMessage {
                    role: assistant::Role::Assistant,
                    text: "## Findings\n\nTransformers **scale well** with data and \
                           compute[1], though attention is *quadratic* in sequence length[2].\n\n\
                           - memory grows with `O(n²)`\n- long prompts need care"
                        .into(),
                    thinking: String::new(),
                    streaming: false,
                    citations: vec![
                        worktable_ui::CitationRef {
                            n: 1,
                            label: "Attention Is All You Need".into(),
                            snippet: "The dominant sequence transduction models are based…".into(),
                            host: "arxiv.org".into(),
                            url: "https://arxiv.org/abs/1706.03762".into(),
                        },
                        worktable_ui::CitationRef {
                            n: 2,
                            label: "Efficient Transformers: A Survey".into(),
                            snippet: "A survey of efficient transformer architectures…".into(),
                            host: "arxiv.org".into(),
                            url: "https://arxiv.org/abs/2009.06732".into(),
                        },
                    ],
                    thinking_collapsed: false,
                },
            ];
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(300));
    capture(&mut cx, handle, "worktable_agent_citations")?;

    println!("— step 11e: markdown answer —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::ChatMessage;
            this.assistant_busy = false;
            this.messages = vec![
                ChatMessage::user("Format a status report"),
                ChatMessage::assistant(
                    "## Weekly notes\n\n**Highlights**\n\n- Shipped the entries list\n- Fixed `streaming` selection\n\n1. Review the [GPUI Kit guides](https://gpui-kit.com/docs/coding-guides/)\n2. Re-run `cargo test --workspace`\n\n> Assistant text is selectable with the mouse.\n\n```rust\nlet answer = 42;\n```",
                ),
            ];
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(300));
    capture(&mut cx, handle, "worktable_agent_markdown")?;

    println!("— step 11f: knowledge search tool status —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::ChatMessage;
            this.show_thinking = false;
            this.messages = vec![ChatMessage::user("What did I save about sunsets?")];
            this.active_tool = Some("search_knowledge".to_owned());
            this.assistant_busy = true;
            this.knowledge_building = false;
            this.knowledge_status = None;
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(400));
    capture(&mut cx, handle, "worktable_agent_tool_search")?;
    cx.update(|cx| {
        view.update(cx, |this, _| {
            this.active_tool = None;
            this.assistant_busy = false;
        });
    });
    cx.run_until_parked();

    println!("— step 11d: globe loader (knowledge build) —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.knowledge_building = true;
            this.knowledge_status = Some("Building knowledge…".to_owned());
            this.messages.clear();
            this.assistant_busy = false;
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(300));
    capture(&mut cx, handle, "worktable_agent_globe")?;
    cx.update(|cx| {
        view.update(cx, |this, _| {
            this.knowledge_building = false;
            this.knowledge_status = None;
        });
    });

    // Reset chat and go back to entries.
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.messages.clear();
            this.assistant_busy = false;
            this.show_entries(cx);
        });
    });
    cx.run_until_parked();

    // ---- Wide desktop layout (1200×800) --------------------------------------
    println!("— step 12: wide desktop layout —");
    {
        let holder: Rc<RefCell<Option<gpui::Entity<WorktableView>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let service_for_window = service.clone();
        let wide_window = cx.open_offscreen_window(
            gpui::Size {
                width: px(1200.),
                height: px(800.),
            },
            move |window, cx| {
                let view = cx.new(|cx| WorktableView::new(service_for_window.clone(), window, cx));
                view.update(cx, |this, cx| {
                    this.set_theme_mode(worktable_view::AppThemeMode::Light, window, cx)
                });
                view.update(cx, |this, cx| {
                    this.set_theme_mode(worktable_view::AppThemeMode::Light, window, cx)
                });
                *holder_for_window.borrow_mut() = Some(view.clone());
                cx.new(|cx| Root::new(view, window, cx))
            },
        )?;
        let wide_view = holder.borrow().clone().expect("wide view was built");
        let wide_handle: gpui::AnyWindowHandle = wide_window.into();
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        assert_eq!(
            cx.read_entity(&wide_view, |v, _| v.mode),
            AppMode::Entries,
            "wide window starts on entries"
        );
        capture(&mut cx, wide_handle, "worktable_wide_entries")?;

        // The morph must grow out of the card's real (centered) rect on a
        // wide window, not from a window-space offset.
        cx.update(|cx| {
            wide_view.update(cx, |this, cx| {
                let id = this
                    .entries
                    .first()
                    .map(|entry| entry.id.clone())
                    .unwrap_or_default();
                let origin = this.entry_origin(&id);
                this.open_entry_modal(&id, origin, cx);
            });
        });
        cx.run_until_parked();
        capture(&mut cx, wide_handle, "worktable_wide_entry_modal")?;
        cx.update(|cx| {
            wide_view.update(cx, |this, cx| this.close_entry_modal(cx));
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        cx.run_until_parked();

        // Ask Agent on the wide layout — must show ONLY the assistant pane.
        cx.update(|cx| wide_view.update(cx, |this, cx| this.show_assistant(cx)));
        cx.run_until_parked();
        capture(&mut cx, wide_handle, "worktable_wide_assistant")?;

        // And back — entries only, no assistant residue.
        cx.update(|cx| wide_view.update(cx, |this, cx| this.show_entries(cx)));
        cx.run_until_parked();
        capture(&mut cx, wide_handle, "worktable_wide_entries_back")?;
    }

    // ---- Narrow-boundary layout (710×800): slide clip check ------------------
    println!("— step 13: narrow boundary slide —");
    {
        let holder: Rc<RefCell<Option<gpui::Entity<WorktableView>>>> = Rc::new(RefCell::new(None));
        let holder_for_window = holder.clone();
        let service_for_window = service.clone();
        let edge_window = cx.open_offscreen_window(
            gpui::Size {
                width: px(710.),
                height: px(800.),
            },
            move |window, cx| {
                let view = cx.new(|cx| WorktableView::new(service_for_window.clone(), window, cx));
                view.update(cx, |this, cx| {
                    this.set_theme_mode(worktable_view::AppThemeMode::Light, window, cx)
                });
                view.update(cx, |this, cx| {
                    this.set_theme_mode(worktable_view::AppThemeMode::Light, window, cx)
                });
                *holder_for_window.borrow_mut() = Some(view.clone());
                cx.new(|cx| Root::new(view, window, cx))
            },
        )?;
        let edge_view = holder.borrow().clone().expect("edge view was built");
        let edge_handle: gpui::AnyWindowHandle = edge_window.into();
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        cx.update(|cx| edge_view.update(cx, |this, cx| this.show_assistant(cx)));
        cx.run_until_parked();
        // Let the 200ms slide finish before capturing.
        std::thread::sleep(std::time::Duration::from_millis(500));
        capture(&mut cx, edge_handle, "worktable_edge_assistant")?;

        cx.update(|cx| edge_view.update(cx, |this, cx| this.show_entries(cx)));
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(500));
        capture(&mut cx, edge_handle, "worktable_edge_entries")?;
    }

    println!("— step 14: dark-mode entries (selection + hover colors) —");
    cx.update(|cx| {
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        view.update(cx, |this, cx| {
            this.show_entries(cx);
            this.select_at("visual-1000".into(), false);
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(250));
    capture(&mut cx, handle, "worktable_dark_entries")?;

    println!("\nAll visual steps passed.");
    Ok(())
}

/// Same app keybindings as `main.rs` (`bindings()` is private there, this
/// runner reproduces them so `simulate_keystrokes` resolves the actions).
fn bindings() -> Vec<gpui::KeyBinding> {
    vec![
        gpui::KeyBinding::new("cmd-1", actions::ShowEntries, None),
        gpui::KeyBinding::new("cmd-2", actions::ShowAssistant, None),
        gpui::KeyBinding::new("cmd-,", actions::ShowSettings, None),
        gpui::KeyBinding::new("escape", actions::CancelComposer, None),
        gpui::KeyBinding::new("cmd-enter", actions::SubmitComposer, None),
    ]
}

fn main() -> anyhow::Result<()> {
    let result = std::panic::catch_unwind(run_visual_tests);
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(_) => {
            eprintln!("Worktable visual tests panicked");
            std::process::exit(1);
        }
    }
}
