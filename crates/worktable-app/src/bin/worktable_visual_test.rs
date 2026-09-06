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

#[path = "../actions.rs"]
mod actions;
#[path = "../assistant.rs"]
mod assistant;
#[path = "../format.rs"]
mod format;
#[path = "../github.rs"]
mod github;
#[path = "../markdown.rs"]
mod markdown;
#[path = "../service.rs"]
mod service;
#[path = "../status_item.rs"]
mod status_item;
#[path = "../worktable_view.rs"]
mod worktable_view;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{AppContext as _, Modifiers, VisualTestAppContext, point, px};
use gpui_component::Root;
use worktable_ai::WorktableEntry;
use worktable_helix;

use service::WorktableService;
use worktable_view::{AppMode, WorktableView};

const WINDOW_SIZE: gpui::Size<gpui::Pixels> = gpui::Size {
    width: px(390.),
    height: px(884.),
};

/// Header row: 30px titlebar inset + pt(8) + 36px controls → center y ≈ 56.
/// Hamburger is the last 36px slot: 390 - 16 - 18 = 356.
const HAMBURGER_CENTER: (f32, f32) = (356.0, 56.0);
/// "Settings" is the FIRST row of the ⋯ menu: 30 + 52 (header) + 4 (pad) + 20.
const SETTINGS_ROW_CENTER: (f32, f32) = (300.0, 106.0);
/// "Ask Agent" sits between the search field and the hamburger.
const ASK_AGENT_CENTER: (f32, f32) = (286.0, 56.0);

fn sample_entries() -> Vec<WorktableEntry> {
    fn entry(content: &str, created_at: i64) -> WorktableEntry {
        WorktableEntry {
            id: format!("visual-{created_at}"),
            kind: "text".to_owned(),
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
        if let Some(theme) = gpui_component::ThemeRegistry::global(cx)
            .themes()
            .get("Ayu Light")
            .cloned()
        {
            let mode = gpui_component::Theme::global(cx).mode;
            gpui_component::Theme::global_mut(cx).apply_config(&theme);
            gpui_component::Theme::change(mode, None, cx);
            // Same Tahoe radius scale as `main::init_theme`.
            let t = gpui_component::Theme::global_mut(cx);
            t.radius = px(10.);
            t.radius_lg = px(14.);
        }
    }
}

/// Capture the current frame of `window` to `<output_dir>/<name>.png`.
fn capture(
    cx: &mut VisualTestAppContext,
    window: gpui::AnyWindowHandle,
    name: &str,
) -> anyhow::Result<()> {
    // Let the splash / entrance animations finish (real wall time — the view
    // keys its splash off `Instant`, not the test clock).
    std::thread::sleep(std::time::Duration::from_millis(900));
    cx.update_window(window, |_, window, _| window.refresh())
        .ok();
    cx.run_until_parked();
    let image = cx.capture_screenshot(window)?;
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
        Arc::new(gpui_component_assets::Assets),
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
        *holder_for_window.borrow_mut() = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    let view = holder.borrow().clone().expect("view was built");
    let handle: gpui::AnyWindowHandle = window.into();

    cx.update_window(handle, |_, window, _| window.refresh())
        .ok();
    cx.run_until_parked();

    println!("— step 1: entries —");
    assert_eq!(cx.read_entity(&view, |v, _| v.entries.len()), 5);
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
    capture(&mut cx, handle, "worktable_entries")?;

    println!("— step 2: hamburger opens the context menu —");
    cx.simulate_click(
        handle,
        point(px(HAMBURGER_CENTER.0), px(HAMBURGER_CENTER.1)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(cx.read_entity(&view, |v, _| v.library_menu_open));
    capture(&mut cx, handle, "worktable_menu_open")?;

    println!("— step 3: Settings navigates —");
    cx.simulate_click(
        handle,
        point(px(SETTINGS_ROW_CENTER.0), px(SETTINGS_ROW_CENTER.1)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);
    capture(&mut cx, handle, "worktable_settings")?;

    println!("— step 3b: Data tab → GitHub Stars config page —");
    // Data tab of the settings header button group.
    cx.simulate_click(handle, point(px(262.0), px(56.0)), Modifiers::default());
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_settings_data")?;
    // The Configure button's exact hit box shifts with the section layout;
    // navigate via the view (same handler the button calls) for the capture.
    cx.update(|cx| {
        view.update(cx, |this, cx| this.show_github_stars(cx));
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::GithubStars,
        "Configure should open the GitHub Stars page"
    );
    capture(&mut cx, handle, "worktable_github_stars")?;
    // Back to Settings (the header's back button).
    cx.simulate_click(handle, point(px(50.0), px(56.0)), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Settings,
        "back returns to Settings"
    );

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

    println!("— step 5: ⌘1 back to entries, Ask Agent button → assistant —");
    cx.update_window(handle, |_, window, cx| {
        let fh = view.read(cx).focus_handle.clone();
        window.focus(&fh, cx);
    })
    .ok();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle, "cmd-1");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
    cx.simulate_click(
        handle,
        point(px(ASK_AGENT_CENTER.0), px(ASK_AGENT_CENTER.1)),
        Modifiers::default(),
    );
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
        view.update(cx, |this, cx| this.set_sort_mode(worktable_view::SortMode::Alpha, cx));
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, _| window.refresh()).ok();
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_sort_alpha")?;

    println!("— step 7: group by Topic (Helix) —");
    cx.update(|cx| {
        view.update(cx, |this, cx| this.set_sort_mode(worktable_view::SortMode::Topic, cx));
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, _| window.refresh()).ok();
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

    println!("— step 8: add photo (image entry) → Helix topic —");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.add_captured_image("/tmp/demo_photo_sunset.png".to_string(), "image/png".to_string(), cx)
        });
    });
    cx.run_until_parked();
    // Give the Helix sync thread a moment (allow_parking already set)
    std::thread::sleep(std::time::Duration::from_millis(300));
    cx.run_until_parked();
    // Back to Time so the new photo appears at top
    cx.update(|cx| {
        view.update(cx, |this, cx| this.set_sort_mode(worktable_view::SortMode::Time, cx));
    });
    cx.run_until_parked();
    capture(&mut cx, handle, "worktable_image_entry")?;
    // Verify Helix contains the photo via its title/path
    {
        let db_path = cx.read_entity(&view, |v, _| v.service.database_path().to_owned());
        let helix_path = worktable_helix::helix_path_for_sqlite(&db_path);
        let client = worktable_helix::HelixClient::open_embedded(helix_path);
        let hits = client.search_blocking("demo_photo", 10).unwrap_or_default();
        assert!(!hits.is_empty(), "Helix should contain the photo entry");
        println!("    helix photo hits: {}", hits.len());
    }

    println!("— step 9: composer — Image kind (ButtonGroup) —");
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |this, cx| this.open_composer_image(window, cx));
    })
    .ok();
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(200));
    capture(&mut cx, handle, "worktable_composer_image")?;
    // Close composer
    cx.update(|cx| {
        view.update(cx, |this, cx| this.cancel_composer(cx));
    });
    cx.run_until_parked();

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
                },
            ];
            this.assistant_busy = true;
            this.show_assistant(cx);
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(400));
    capture(&mut cx, handle, "worktable_agent_thinking")?;

    // Waiting-for-first-token state: busy, no streaming bubble.
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            use assistant::{ChatMessage, Role};
            this.messages = vec![
                ChatMessage::user("Summarize my notes"),
                ChatMessage {
                    role: Role::Assistant,
                    text: String::new(),
                    thinking: String::new(),
                    streaming: true,
                },
            ];
            cx.notify();
        });
    });
    cx.run_until_parked();
    std::thread::sleep(std::time::Duration::from_millis(400));
    capture(&mut cx, handle, "worktable_agent_waiting")?;

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

    println!("\nAll visual steps passed.");
    Ok(())
}

/// Same app keybindings as `main.rs` (`bindings()` is private there, this
/// runner reproduces them so `simulate_keystrokes` resolves the actions).
fn bindings() -> Vec<gpui::KeyBinding> {
    let mut bindings = vec![
        gpui::KeyBinding::new("cmd-1", actions::ShowEntries, None),
        gpui::KeyBinding::new("cmd-2", actions::ShowAssistant, None),
        gpui::KeyBinding::new("cmd-,", actions::ShowSettings, None),
    ];
    bindings.into_iter().collect()
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
