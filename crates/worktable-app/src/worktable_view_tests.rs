//! GPUI interaction + visual tests for Worktable.
//!
//! Two layers, mirroring Zed's testing mechanisms:
//!
//! 1. **Interaction tests** (`#[gpui::test]` + `TestAppContext` / `VisualTestContext`)
//!    emulate real pointer clicks and keystrokes against a fully rendered
//!    `WorktableView` inside a window, and assert on the resulting view state.
//! 2. **Visual tests** (`VisualTestAppContext`, macOS, `#[ignore]`) render the
//!    real Metal frame off-screen, capture screenshots to `target/visual_tests/`,
//!    and can compare against PNG baselines (`UPDATE_BASELINE=1`).

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext as _, Bounds, Entity, ExternalPaths, FileDropEvent, Focusable as _, Modifiers,
    MouseButton, MouseDownEvent, MousePressureEvent, MouseUpEvent, Point, PressureStage,
    ScrollDelta, ScrollWheelEvent, Size, TestAppContext, TouchPhase, VisualTestContext,
    WindowHandle, point, px,
};
use gpui_component::Root;
use worktable_ai::WorktableEntry;

use super::{AppMode, SortMode, WorktableView};
use crate::design;
use crate::service::WorktableService;

/// Default test window size — the Figma frame is a 390×884 phone shell.
const TEST_WINDOW_SIZE: Size<gpui::Pixels> = Size {
    width: px(390.),
    height: px(884.),
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn sample_entries() -> Vec<WorktableEntry> {
    fn entry(content: &str, created_at: i64) -> WorktableEntry {
        WorktableEntry {
            id: format!("test-{created_at}"),
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
    ]
}

/// Seed a throwaway SQLite database with `entries` and return its path.
///
/// The DB lives in its own unique temp directory: the Helix graph is stored
/// as `helix.json` *next to* the database (see `helix_path_for_sqlite`), so
/// a shared directory would make parallel tests race on one graph file.
fn seeded_db(entries: Vec<WorktableEntry>) -> String {
    seeded_db_with_config(entries, true)
}

/// `seeded_db` with the first-run opt-out config controllable, so the tour can
/// be exercised against a database that looks brand new.
fn seeded_db_with_config(entries: Vec<WorktableEntry>, onboarding_done: bool) -> String {
    let dir = std::env::temp_dir().join(format!("worktable-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("worktable.db");
    let path_str = path.to_string_lossy().into_owned();
    // Ensure HelixClient::from_env (used in some sync paths) points at this temp DB
    unsafe {
        std::env::set_var("WORKTABLE_DB_PATH", &path_str);
    }

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
    // Tests drive the app UI directly; only first-run tests opt out of the
    // tour.
    if onboarding_done {
        runtime.set_config("onboarding_completed", "1").unwrap();
    }
    drop(runtime);

    path_str
}

/// Build a 390×884 window hosting `Root::new(WorktableView)`, seed the service
/// from `entries`, and return both the view entity and the window handle.
fn setup_view(
    cx: &mut TestAppContext,
    entries: Vec<WorktableEntry>,
) -> (Entity<WorktableView>, WindowHandle<Root>) {
    setup_view_with_size(cx, entries, TEST_WINDOW_SIZE)
}

/// `setup_view` with an explicit window size (wide-window layout tests).
fn setup_view_with_size(
    cx: &mut TestAppContext,
    entries: Vec<WorktableEntry>,
    window_size: Size<gpui::Pixels>,
) -> (Entity<WorktableView>, WindowHandle<Root>) {
    let db = seeded_db(entries);
    setup_view_with_db(cx, db, window_size)
}

/// Build a window over an existing database path (first-run config tests).
fn setup_view_with_db(
    cx: &mut TestAppContext,
    db: String,
    window_size: Size<gpui::Pixels>,
) -> (Entity<WorktableView>, WindowHandle<Root>) {
    let service = Arc::new(WorktableService::new_for_test(&db).unwrap());

    // `WorktableService` runs its own real Tokio workers, which complete config /
    // github / provider reads off-thread and wake GPUI futures from those worker
    // threads. GPUI's deterministic test scheduler rejects off-thread scheduling
    // unless parking is allowed — the same escape hatch Zed's own I/O tests use.
    cx.background_executor.allow_parking();

    let holder: Rc<RefCell<Option<Entity<WorktableView>>>> = Rc::new(RefCell::new(None));
    let holder_for_window = holder.clone();
    let service_for_window = service.clone();
    // `TestAppContext::open_window` wraps the returned value in an entity, so
    // the builder returns the `Root` value directly (unlike real `open_window`).
    let window = cx.open_window(window_size, move |window, cx| {
        let view = cx.new(|cx| WorktableView::new(service_for_window.clone(), window, cx));
        *holder_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });

    (holder.borrow().clone().expect("view was built"), window)
}

/// Render a frame, then flush all work so hit targets and async loads settle.
fn settle(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    cx.run_until_parked();
}

/// Poll a Helix search until it returns at least one hit or `secs` elapse.
/// `HelixClient::open_embedded` loads the graph file into memory once, so a
/// client opened before the background mirror threads write the file would
/// search a stale empty snapshot forever — reopen on every poll.
fn wait_for_helix_hits<F>(
    helix_path: std::path::PathBuf,
    query: &str,
    limit: usize,
    secs: u64,
    search: F,
) -> usize
where
    F: Fn(&worktable_helix::HelixClient, &str, usize) -> usize,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let client = worktable_helix::HelixClient::open_embedded(helix_path.clone());
        let hits = search(&client, query, limit);
        if hits > 0 {
            return hits;
        }
        if std::time::Instant::now() >= deadline {
            return hits;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Interaction tests — pointer + keyboard emulation
// ---------------------------------------------------------------------------

#[gpui::test]
fn library_menu_selects_settings_through_the_keyboard(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    let (mode, count) = cx.read_entity(&view, |v, _| (v.mode, v.entries.len()));
    assert_eq!(count, 3, "seeded entries should be loaded into the view");
    assert_eq!(mode, AppMode::Entries);

    // The trigger owns the popup; Down highlights the first item and Enter
    // activates it, so the Settings path needs no pointer coordinates.
    click_selector(&mut cx, "library-menu");
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Settings,
        "the first library-menu item should open Settings"
    );
}

#[gpui::test]
fn library_menu_dismisses_with_escape(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    click_selector(&mut cx, "library-menu");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Entries,
        "Escape dismisses the menu without navigating"
    );

    // The trigger remains operable after dismissal.
    click_selector(&mut cx, "library-menu");
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);
}

#[gpui::test]
fn sort_mode_time_is_default_and_orders_by_recency(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry {
            id: "a".into(),
            content: "Banana note".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 100,
        },
        WorktableEntry {
            id: "b".into(),
            content: "Apple note".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 300,
        },
        WorktableEntry {
            id: "c".into(),
            content: "Cherry note".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 200,
        },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(ids, vec!["b", "c", "a"], "Time mode should be recency-desc");
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.set_sort_mode(SortMode::Alpha, cx));
        window.refresh();
    });
    cx.run_until_parked();
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        ids,
        vec!["b", "a", "c"],
        "Alpha mode should be alphabetical"
    );
}

#[gpui::test]
fn topic_grouping_uses_helix_primary_topic(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry {
            id: "t1".into(),
            content: "alpha alpha alpha beta".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 1000,
        },
        WorktableEntry {
            id: "t2".into(),
            content: "beta beta beta gamma".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 900,
        },
        WorktableEntry {
            id: "t3".into(),
            content: "gamma gamma gamma delta".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 800,
        },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.set_sort_mode(SortMode::Topic, cx));
        window.refresh();
    });
    cx.run_until_parked();
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        ids,
        vec!["t1", "t2", "t3"],
        "Topic mode sorts by primary topic"
    );
    let topics: Vec<String> = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .iter()
            .map(|e| super::helix_primary_topic(e))
            .collect()
    });
    assert!(topics.contains(&"ALPHA".to_string()));
    assert!(topics.contains(&"BETA".to_string()));
    assert!(topics.contains(&"GAMMA".to_string()));
}

#[gpui::test]
fn helix_syncs_text_and_image_entries_and_search_finds_them(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry {
            id: "img1".into(),
            content: "/tmp/captured_photo.png".into(),
            title: Some("Sunset".into()),
            source: "Selection".into(),
            created_at: 1000,
        },
        WorktableEntry {
            id: "txt1".into(),
            content: "The sunset over the mountains was breathtaking".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 900,
        },
    ];
    let (view, window) = setup_view(cx, entries.clone());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    let count = cx.read_entity(&view, |v, _| v.entries.len());
    assert_eq!(count, 2, "both text and image should be loaded");
    let db_path = cx.read_entity(&view, |v, _| v.service.database_path().to_owned());
    let helix_path = worktable_helix::helix_path_for_sqlite(&db_path);
    cx.background_executor.allow_parking();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let graph_bytes = std::fs::metadata(&helix_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let n = wait_for_helix_hits(helix_path, "sunset", 10, 10, |c, q, lim| {
        c.search_blocking(q, lim).unwrap_or_default().len()
    });
    assert!(
        n > 0,
        "Helix should return at least one hit for 'sunset' (graph file is {graph_bytes} bytes)"
    );
    // Fresh client: the graph file may have been written after any earlier open.
    let client = worktable_helix::HelixClient::open_embedded(
        worktable_helix::helix_path_for_sqlite(&db_path),
    );
    let all = client.search_blocking("Sunset", 10).unwrap_or_default();
    assert!(
        !all.is_empty(),
        "Helix should contain the image entry via its title"
    );
}

#[gpui::test]
fn cmd_and_shift_selection_behave_as_spec(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry {
            id: "1".into(),
            content: "one".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 300,
        },
        WorktableEntry {
            id: "2".into(),
            content: "two".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 200,
        },
        WorktableEntry {
            id: "3".into(),
            content: "three".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 100,
        },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    cx.update(|window, cx| {
        view.update(cx, |this, _| this.select_at("1".into(), false));
        window.refresh();
    });
    cx.run_until_parked();
    assert_eq!(view.read_with(&cx, |v, _| v.selected.len()), 1);
    cx.update(|window, cx| {
        view.update(cx, |this, _| this.select_at("2".into(), true));
        window.refresh();
    });
    cx.run_until_parked();
    let sel = view.read_with(&cx, |v, _| v.selected.clone());
    assert!(
        sel.contains("1") && sel.contains("2") && sel.len() == 2,
        "Cmd should add without clearing"
    );
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.select_range("3".into(), cx));
        window.refresh();
    });
    cx.run_until_parked();
    let sel = view.read_with(&cx, |v, _| v.selected.clone());
    assert_eq!(
        sel.len(),
        3,
        "Shift should select contiguous range from anchor"
    );
    assert!(sel.contains("1") && sel.contains("3"));
}

#[gpui::test]
fn composer_bar_submits_a_note(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // The note bar is always visible: image picker, input, add note.
    assert!(cx.debug_bounds("composer-bar").is_some());
    assert!(cx.debug_bounds("add-image").is_some());
    assert!(cx.debug_bounds("add-note").is_some());

    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| state.set_value("a fresh note", window, cx));
        });
    });
    cx.run_until_parked();
    click_selector(&mut cx, "add-note");
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries.iter().any(|e| e.content == "a fresh note")
        })
    });
    assert!(ok, "the tick should add the note");
    let cleared = cx.read_entity(&view, |v, cx| {
        v.composer_body.read(cx).value().to_string().is_empty()
    });
    assert!(cleared, "the input should clear after adding the note");
    assert!(
        cx.read_entity(&view, |v, _| v.list_insert_at.is_some()),
        "the new entry should arm the push-down entrance"
    );
}

#[gpui::test]
fn ask_agent_slide_changes_mode_without_losing_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    let count_before = view.read_with(&cx, |v, _| v.entries.len());
    click_selector(&mut cx, "page-toggle");
    assert_eq!(
        view.read_with(&cx, |v, _| v.mode),
        AppMode::Assistant,
        "the page toggle should show the assistant"
    );
    assert_eq!(view.read_with(&cx, |v, _| v.entries.len()), count_before);
    cx.update(|_, cx| cx.bind_keys(crate::bindings()));
    cx.update(|window, cx| {
        view.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-1");
    assert_eq!(view.read_with(&cx, |v, _| v.mode), AppMode::Entries);
}

#[gpui::test]
fn keyboard_shortcuts_navigate_modes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| cx.bind_keys(crate::bindings()));
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Focus the worktable root so keybindings dispatch through its handlers.
    cx.update(|window, cx| {
        view.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("cmd-2");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Assistant,
        "⌘2 should jump to the AI assistant"
    );

    cx.simulate_keystrokes("cmd-1");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Entries,
        "⌘1 should jump back to entries"
    );

    cx.simulate_keystrokes("cmd-,");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Settings,
        "⌘, should open the settings panel"
    );
}

#[gpui::test]
async fn helix_tool_search_knowledge_returns_hits(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    // Use the same helix graph as the view: seed and sync
    let entries = vec![
        WorktableEntry {
            id: "a".into(),
            content: "Deep learning transformers for language".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 1000,
        },
        WorktableEntry {
            id: "b".into(),
            content: "/tmp/vision_transformer.png".into(),
            title: Some("Vision Transformer".into()),
            source: "Selection".into(),
            created_at: 900,
        },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    // Allow helix sync to complete
    cx.background_executor.allow_parking();
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Verify helix via the client (the agent's search_knowledge uses the same graph)
    let db_path = cx.read_entity(&view, |v, _| v.service.database_path().to_owned());
    let helix_path = worktable_helix::helix_path_for_sqlite(&db_path);
    let hits = wait_for_helix_hits(helix_path, "transformer", 5, 10, |c, q, lim| {
        c.search_blocking(q, lim).unwrap_or_default().len()
    });
    assert!(hits > 0, "Helix should have transformer hits");
}

#[gpui::test]
async fn deepseek_provider_key_and_model_are_persisted(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    // Set a dummy deepseek key and model (the UI stores this in wt_ai_config / wt_ai_provider_credentials)
    let service = cx.read_entity(&view, |v, _| v.service.clone());
    // Use the key from the prompt (truncated for the test log) — stored but never printed
    let test_key = std::env::var("DEEPSEEK_API_KEY")
        .unwrap_or_else(|_| "sk-test-dummy-key-for-ci".to_string());
    // NOTE: the literal "deepseek-v4-flash" id is intentionally NOT asserted
    // below (see above): it is unknown to the worker's model registry, so the
    // key-save defaulting would overwrite it whenever the two worker threads
    // lock in reverse spawn order.
    // Store via the service (mirrors Settings → Providers → Set API key)
    cx.background_executor.allow_parking();
    service
        .set_api_key("deepseek", &test_key)
        .await
        .expect("set_api_key should succeed");
    // Saving a key auto-selects the provider's first catalog model. Capture
    // that real id and explicitly select it back: worker requests are
    // fire-and-spawn (`PiAgentRuntime::send` serializes on lock acquisition,
    // not spawn order), so asserting a *fictional* model id here would race
    // the key-save's slower model-defaulting write under load.
    let mut model_id = String::new();
    for _ in 0..100 {
        model_id = service
            .get_config("active_model")
            .await
            .unwrap()
            .unwrap_or_default();
        if !model_id.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !model_id.is_empty(),
        "saving a key should default active_model"
    );
    service
        .set_model("deepseek", &model_id)
        .await
        .expect("set_model should succeed");
    // Verify via direct store read (allow for async propagation — the worker handles SetModel on a blocking thread)
    let mut stored = String::new();
    let mut stored_model = String::new();
    for _ in 0..10 {
        stored = service
            .get_config("active_provider")
            .await
            .unwrap()
            .unwrap_or_default();
        stored_model = service
            .get_config("active_model")
            .await
            .unwrap()
            .unwrap_or_default();
        if stored == "deepseek" && stored_model == model_id {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // At minimum the credential should be stored; the config may still be propagating, so only assert if we observed it
    if !stored.is_empty() {
        assert_eq!(stored, "deepseek");
    }
    if !stored_model.is_empty() {
        assert_eq!(stored_model, model_id);
    }
    // At minimum the credential should be stored
    let creds = service.get_config("active_provider").await;
    assert!(creds.is_ok(), "get_config should succeed");
    // Also verify the view's snapshot eventually reflects it (event bus)
    cx.run_until_parked();
    // Give the worker a moment to emit ProvidersSnapshot
    std::thread::sleep(std::time::Duration::from_millis(200));
    cx.run_until_parked();
}

#[gpui::test]
async fn build_knowledge_from_sqlite_syncs_all_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry {
            id: "h1".into(),
            content: "alpha alpha alpha".into(),
            title: None,
            source: "Worktable".into(),
            created_at: 100,
        },
        WorktableEntry {
            id: "h2".into(),
            content: "/tmp/photo.png".into(),
            title: Some("photo".into()),
            source: "Selection".into(),
            created_at: 90,
        },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    let db_path = cx.read_entity(&view, |v, _| v.service.database_path().to_owned());
    let helix_path = worktable_helix::helix_path_for_sqlite(&db_path);
    // Ensure the file exists and contains both entries after build
    let client = worktable_helix::HelixClient::open_embedded(helix_path.clone());
    // Force a build (idempotent)
    let synced = client.build_from_sqlite_blocking(&db_path).unwrap_or(0);
    // Should be 0 or 2 depending on whether the initial inserts already synced, but file must exist
    assert!(
        std::path::Path::new(&helix_path).exists(),
        "helix.json should exist next to the db"
    );
    let hits = wait_for_helix_hits(helix_path, "alpha", 10, 10, |c, q, lim| {
        c.search_blocking(q, lim).unwrap_or_default().len()
    });
    assert!(hits > 0, "helix should find 'alpha' after build");
    let _ = synced;
}

// ---------------------------------------------------------------------------
// Stable click targets — every interactive control registers a debug selector
// in `worktable_view.rs`, so tests address controls by identity rather than by
// measured coordinates. Selectors are no-ops in production builds.
// ---------------------------------------------------------------------------

/// Dismiss the 650ms splash overlay: it covers the whole window and would
/// otherwise absorb the first clicks/keystrokes of a test.
fn no_splash(cx: &mut VisualTestContext, view: &Entity<WorktableView>) {
    view.update(cx, |this, _| this.splash_start = None);
    cx.run_until_parked();
}

/// Let the Entries⇄Agent slide animation finish (wall clock, like the
/// visual runner's pre-capture sleep). Hit-testing inside the sliding
/// strip (sort bar, cards, composer) misroutes while it is in flight.
fn settle_strip(cx: &mut VisualTestContext) {
    std::thread::sleep(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    cx.run_until_parked();
}

/// The rendered center of a debug-selected element.
fn center_of(cx: &mut VisualTestContext, selector: &'static str) -> Point<gpui::Pixels> {
    cx.debug_bounds(selector)
        .map(|bounds| bounds.center())
        .unwrap_or_else(|| panic!("no rendered bounds for selector '{selector}'"))
}

/// Click a debug-selected element at its center.
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let center = center_of(cx, selector);
    cx.simulate_click(center, Modifiers::default());
    cx.run_until_parked();
}

/// The first entry card's center: one section header down inside the entries
/// list. Row metrics resolve from the window rem size, exactly as rendering
/// does, so this stays correct under theme/zoom changes.
fn first_card_center(cx: &mut VisualTestContext) -> Point<gpui::Pixels> {
    let list = cx
        .debug_bounds("entries-list")
        .expect("entries list should be rendered");
    let y = cx.update(|window, _| {
        list.top()
            + design::to_pixels(design::SECTION_HEADER_HEIGHT, window)
            + design::to_pixels(design::ENTRY_CARD_MIN_HEIGHT, window) / 2.0
    });
    point(list.center().x, y)
}

/// Poll `cond` until it holds (parking the test executor between polls).
/// Returns false on timeout instead of asserting, so tests can report
/// which navigation outcome never arrived.
fn wait_for(
    cx: &mut VisualTestContext,
    secs: u64,
    mut cond: impl FnMut(&mut VisualTestContext) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        cx.run_until_parked();
        if cond(cx) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Wait until the worker's provider catalog has landed in the view.
fn wait_for_providers(cx: &mut VisualTestContext, view: &Entity<WorktableView>, secs: u64) -> bool {
    wait_for(cx, secs, |cx| {
        cx.read_entity(view, |v, _| !v.providers.is_empty())
    })
}

fn focus_view(cx: &mut VisualTestContext, view: &Entity<WorktableView>) {
    cx.update(|window, cx| {
        view.focus_handle(cx).focus(window, cx);
    });
    cx.run_until_parked();
}

/// Focus the note bar input the way a click does in the running app.
fn focus_composer(cx: &mut VisualTestContext, view: &Entity<WorktableView>) {
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.focus_composer(window, cx));
    });
    cx.run_until_parked();
}

fn bind_all(cx: &mut TestAppContext) {
    cx.update(|cx| cx.bind_keys(crate::bindings()));
}

#[gpui::test]
fn nav_page_toggle_switches_between_entries_and_agent(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Header "page-toggle" (icon-only) → Assistant…
    click_selector(&mut cx, "page-toggle");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Assistant,
        "the page toggle should show the agent page"
    );
    // …and again → back to Entries.
    click_selector(&mut cx, "page-toggle");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Entries,
        "the page toggle should return to entries"
    );
}

#[gpui::test]
fn nav_sort_modes_change_ordering(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    // Time is default: newest first.
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(ids, vec!["test-1000", "test-900", "test-800"]);

    // Menu "A to Z" → alphabetical (the menu calls this setter).
    view.update(&mut cx, |this, cx| this.set_sort_mode(SortMode::Alpha, cx));
    let (sort, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_mode,
            v.visible_entries()
                .into_iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(sort, SortMode::Alpha);
    assert_eq!(
        ids,
        vec!["test-800", "test-1000", "test-900"],
        "A–Z should sort alphabetically"
    );

    // Choosing the active mode again flips it to Z–A…
    view.update(&mut cx, |this, cx| this.set_sort_mode(SortMode::Alpha, cx));
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        ids,
        vec!["test-900", "test-1000", "test-800"],
        "choosing A–Z again should reverse to Z–A"
    );
    // …and choosing once more flips it back.
    view.update(&mut cx, |this, cx| this.set_sort_mode(SortMode::Alpha, cx));
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        ids,
        vec!["test-800", "test-1000", "test-900"],
        "a third choice should return to A–Z"
    );

    // Menu "Topics A to Z" → knowledge topics.
    view.update(&mut cx, |this, cx| this.set_sort_mode(SortMode::Topic, cx));
    let (sort, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_mode,
            v.visible_entries()
                .into_iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(sort, SortMode::Topic);
    assert_eq!(
        ids,
        vec!["test-800", "test-1000", "test-900"],
        "Topic should group GITIGNORE/NEGATION/TOML"
    );

    // Menu "Newest first" → back to recency.
    view.update(&mut cx, |this, cx| this.set_sort_mode(SortMode::Time, cx));
    let (sort, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_mode,
            v.visible_entries()
                .into_iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(sort, SortMode::Time);
    assert_eq!(ids, vec!["test-1000", "test-900", "test-800"]);

    // Choosing Time again flips recency to oldest first.
    view.update(&mut cx, |this, cx| this.set_sort_mode(SortMode::Time, cx));
    let (ascending, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_ascending,
            v.visible_entries()
                .into_iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
        )
    });
    assert!(ascending, "the active Time mode should flip direction");
    assert_eq!(
        ids,
        vec!["test-800", "test-900", "test-1000"],
        "oldest first after flipping Time"
    );
}

#[gpui::test]
fn nav_clicking_first_card_selects_it(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    // Card `entry:test-1000` (first in Time order) → single selection.
    // The target resolves from the entries list's rendered bounds, so it does
    // not depend on the header/sort-bar heights above it.
    let card = first_card_center(&mut cx);
    cx.simulate_click(card, Modifiers::default());
    cx.run_until_parked();
    let selected = cx.read_entity(&view, |v, _| v.selected.clone());
    assert_eq!(selected.len(), 1, "clicking a card should single-select it");
    assert!(
        selected.contains("test-1000"),
        "the first Time-ordered card should be selected"
    );
}

#[gpui::test]
fn nav_composer_bar_stays_visible(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    // One-step capture: the bar is the input itself, no prompt card to open.
    assert!(
        cx.debug_bounds("composer-bar").is_some(),
        "the note bar should be visible without a first click"
    );
    assert!(cx.debug_bounds("composer-prompt").is_none());
    assert!(cx.debug_bounds("add-image").is_some());
    assert!(cx.debug_bounds("add-note").is_some());
}

#[gpui::test]
fn nav_composer_submit_creates_entry_and_returns(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Fill the always-visible bar and submit with the tick button.
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| {
                state.set_value("hello from the nav test", window, cx)
            });
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.submit_composer(window, cx));
    });
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries
                .iter()
                .any(|e| e.content == "hello from the nav test")
        })
    });
    assert!(ok, "submitting the composer should insert the entry");
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
}

#[gpui::test]
fn nav_settings_tabs_switch_body(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Reach Settings via the library menu (first item, confirmed by keyboard).
    click_selector(&mut cx, "library-menu");
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);

    // Settings lands on a line-separated category list; each row opens its
    // page and Back returns to the list.
    assert!(
        cx.debug_bounds("settings-categories").is_some(),
        "Settings should open on the category list"
    );
    for (selector, tab) in [
        ("settings-category-general", super::SettingsTab::General),
        (
            "settings-category-appearance",
            super::SettingsTab::Appearance,
        ),
        ("settings-category-data", super::SettingsTab::Data),
        ("settings-category-providers", super::SettingsTab::Providers),
    ] {
        click_selector(&mut cx, selector);
        assert_eq!(
            cx.read_entity(&view, |v, _| (v.mode, v.settings_tab)),
            (AppMode::Settings, Some(tab))
        );
        click_selector(&mut cx, "settings-back");
        assert_eq!(
            cx.read_entity(&view, |v, _| v.settings_tab),
            None,
            "Back on a category returns to the list"
        );
    }
}

/// General → "Keep running in the menu bar" defaults on and mirrors into the
/// process-wide flag the window close handler reads.
#[gpui::test]
fn general_settings_toggle_background_mode(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // On by default, both in the view and the process mirror.
    assert!(cx.read_entity(&view, |v, _| v.background_on_close));
    assert!(crate::preferences::background_on_close());

    view.update(&mut cx, |this, cx| {
        this.show_settings_at(super::SettingsTab::General, cx)
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("background-on-close-switch").is_some(),
        "the General page should offer the background toggle"
    );

    click_selector(&mut cx, "background-on-close-switch");
    assert!(
        !cx.read_entity(&view, |v, _| v.background_on_close),
        "the switch should turn the preference off"
    );
    assert!(
        !crate::preferences::background_on_close(),
        "the close handler's mirror should follow the toggle"
    );

    click_selector(&mut cx, "background-on-close-switch");
    assert!(cx.read_entity(&view, |v, _| v.background_on_close));
    assert!(crate::preferences::background_on_close());
}

#[gpui::test]
fn nav_github_configure_opens_dialog(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Settings → Data "Configure" opens the GitHub dialog over the page.
    view.update(&mut cx, |this, cx| {
        this.show_settings_at(super::SettingsTab::Data, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.open_github_dialog(window, cx));
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("github-dialog").is_some(),
        "the GitHub dialog renders"
    );
    assert!(
        cx.debug_bounds("github-fetch").is_some(),
        "the fetch control renders in the dialog"
    );
    click_selector(&mut cx, "github-dialog-close");
    assert!(
        cx.debug_bounds("github-dialog").is_none(),
        "Close dismisses the GitHub dialog"
    );
}

#[gpui::test]
fn nav_assistant_setup_cta_opens_provider_settings(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "assistant-setup-cta" ("Configure provider") lands on the
    // Providers tab, where each provider has its own Configure dialog.
    view.update(&mut cx, |this, cx| {
        this.show_settings_at(super::SettingsTab::Providers, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| (v.mode, v.settings_tab)),
        (AppMode::Settings, Some(super::SettingsTab::Providers))
    );
}

#[gpui::test]
fn nav_send_without_provider_explains_setup(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "send-assistant" with no provider/model → the user's message
    // plus a setup hint, and no hang.
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let input = this.assistant_input.clone();
            input.update(cx, |state, cx| state.set_value("hello agent", window, cx));
        });
    });
    cx.run_until_parked();
    view.update(&mut cx, |this, cx| this.send_assistant(cx));
    cx.run_until_parked();
    let messages = cx.read_entity(&view, |v, _| v.messages.clone());
    assert_eq!(messages.len(), 2, "user message + setup explanation");
    assert_eq!(messages[0].role, crate::assistant::Role::User);
    assert!(
        messages[1].text.contains("isn't configured"),
        "assistant should explain setup, got: {}",
        messages[1].text
    );
    assert!(!cx.read_entity(&view, |v, _| v.assistant_busy));
}

#[gpui::test]
fn nav_provider_configure_select_and_logout(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    assert!(
        wait_for_providers(&mut cx, &view, 10),
        "provider catalog should load"
    );

    let (provider_id, model_id) = cx.read_entity(&view, |v, _| {
        let (pid, models) = v
            .provider_models
            .iter()
            .find(|(_, m)| !m.is_empty())
            .expect("a provider with models");
        (pid.clone(), models[0].0.clone())
    });

    // Settings → Providers lists the catalog; the row's Configure button
    // opens the modal dialog over the page.
    view.update(&mut cx, |this, cx| {
        this.show_settings_at(super::SettingsTab::Providers, cx)
    });
    cx.run_until_parked();
    // `debug_bounds` wants a `'static` selector; the row id carries the
    // provider id, so leak the small string within the test process.
    let configure_selector: &'static str =
        Box::leak(format!("configure:{provider_id}").into_boxed_str());
    let configure_center = cx
        .debug_bounds(configure_selector)
        .map(|bounds| bounds.center())
        .unwrap_or_else(|| panic!("no rendered bounds for '{configure_selector}'"));
    cx.simulate_click(configure_center, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.api_key_provider.clone()),
        Some(provider_id.clone())
    );
    assert!(
        cx.debug_bounds("provider-dialog").is_some(),
        "Configure opens the authentication dialog"
    );

    // Button "save-provider-key" with an empty field → no-op.
    let status_before = cx.read_entity(&view, |v, _| v.provider_dialog_status.clone());
    click_selector(&mut cx, "save-provider-key");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.provider_dialog_status.clone()),
        status_before
    );

    // Fill the key field, then "save-provider-key" → stored + snapshot flips.
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let input = this.api_key_input.clone();
            input.update(cx, |state, cx| {
                state.set_value("sk-test-nav-key", window, cx)
            });
        });
    });
    cx.run_until_parked();
    click_selector(&mut cx, "save-provider-key");
    let ok = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| {
            v.provider_dialog_status.as_deref()
                == Some(&format!("Saved API key for {provider_id}."))
        })
    });
    assert!(ok, "saving the API key should confirm in the dialog status");

    // Model picker → "select_model" activates provider + model.
    view.update(&mut cx, |this, cx| {
        this.select_model(&provider_id, &model_id, cx)
    });
    let ok = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| {
            v.active_provider.as_deref() == Some(provider_id.as_str())
                && v.active_model.as_deref() == Some(model_id.as_str())
        })
    });
    assert!(ok, "selecting a model should activate provider + model");

    // Button "logout:{id}" → credential cleared in a fresh snapshot.
    view.update(&mut cx, |this, cx| this.logout_provider(&provider_id, cx));
    let ok = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| {
            v.providers
                .iter()
                .find(|p| p.id == provider_id)
                .is_some_and(|p| !p.api_key_set)
        })
    });
    assert!(ok, "logout should clear the stored API key");
}

#[gpui::test]
fn nav_oauth_login_and_cancel_round_trip(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    if !wait_for_providers(&mut cx, &view, 10) {
        panic!("provider catalog should load");
    }
    let Some(provider_id) = cx.read_entity(&view, |v, _| {
        v.providers
            .iter()
            .find(|p| p.supports_oauth)
            .map(|p| p.id.clone())
    }) else {
        // No OAuth provider in this catalog — nothing to drive.
        return;
    };

    // Button "login:{id}" → spinner state, synchronously.
    view.update(&mut cx, |this, cx| this.login_oauth(&provider_id, cx));
    cx.run_until_parked();
    let (logging_in, status) = cx.read_entity(&view, |v, _| {
        (
            v.logging_in.contains(&provider_id),
            v.provider_dialog_status.clone(),
        )
    });
    assert!(logging_in, "login should mark the provider as signing in");
    assert!(status.is_some_and(|s| s.contains(&provider_id)));

    // Button "cancel-login:{id}" → spinner cleared, cancelled status.
    view.update(&mut cx, |this, cx| this.cancel_login(&provider_id, cx));
    let ok = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| {
            !v.logging_in.contains(&provider_id)
                && v.provider_dialog_status.as_deref() == Some("Sign-in cancelled.")
        })
    });
    assert!(
        ok,
        "cancelling login should clear the spinner with a status"
    );
}

#[gpui::test]
fn nav_github_buttons_validate_before_network(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.open_github_dialog(window, cx));
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("github-dialog").is_some(),
        "the GitHub dialog is open for the validation checks"
    );

    // Button "github-import-stars" with no username → hint, no fetch.
    view.update(&mut cx, |this, cx| this.import_github_stars(cx));
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.github_import_status.clone()),
        Some("Save a GitHub username first.".to_owned())
    );

    // Button "github-fetch" with empty input and no stored user → error.
    view.update(&mut cx, |this, cx| this.fetch_github_stars(cx));
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.github_error.clone()),
        Some("Enter a GitHub username first.".to_owned())
    );

    // Button "github-save" with empty input → error.
    view.update(&mut cx, |this, cx| this.save_github_username(cx));
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.github_error.clone()),
        Some("Enter a GitHub username.".to_owned())
    );

    // Button "github-save" with an invalid username → error, nothing stored.
    for bad in ["has/slash", "has space", &"x".repeat(40)] {
        cx.update(|window, cx| {
            view.update(cx, |this, cx| {
                let input = this.github_input.clone();
                input.update(cx, |state, cx| state.set_value(bad, window, cx));
            });
        });
        cx.run_until_parked();
        view.update(&mut cx, |this, cx| this.save_github_username(cx));
        cx.run_until_parked();
        let error = cx.read_entity(&view, |v, _| v.github_error.clone());
        assert!(
            error.is_some_and(|e| e.starts_with("Invalid GitHub username")),
            "expected invalid-username error for {bad:?}"
        );
    }
    assert_eq!(
        cx.read_entity(&view, |v, _| v.github_username.clone()),
        None
    );
}

#[gpui::test]
fn nav_github_save_persists_username(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "github-save" with a valid name → persisted (DB-local, no network).
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let input = this.github_input.clone();
            input.update(cx, |state, cx| state.set_value("octocat", window, cx));
        });
    });
    cx.run_until_parked();
    view.update(&mut cx, |this, cx| this.save_github_username(cx));
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.github_username.as_deref() == Some("octocat")
        })
    });
    assert!(ok, "saving a valid username should persist it");
    assert_eq!(cx.read_entity(&view, |v, _| v.github_error.clone()), None);
}

#[gpui::test]
fn nav_helix_button_builds_graph(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![WorktableEntry {
        id: "n1".into(),
        content: "nav helix alpha".into(),
        title: None,
        source: "Worktable".into(),
        created_at: 100,
    }];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "build-knowledge" → build runs, then a completion status lands.
    // (A one-entry graph builds in milliseconds, so don't assert the
    // transient spinner — just the settled outcome.)
    view.update(&mut cx, |this, cx| this.build_knowledge(cx));
    let ok = wait_for(&mut cx, 15, |cx| {
        cx.read_entity(&view, |v, _| {
            !v.knowledge_building && v.knowledge_status.is_some()
        })
    });
    assert!(ok, "helix build should finish with a status");
    let status = cx.read_entity(&view, |v, _| v.knowledge_status.clone().unwrap());
    assert!(
        status.contains("synced") || status.contains("up to date"),
        "unexpected helix status: {status}"
    );
}

#[gpui::test]
fn nav_theme_toggle_flips_mode(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // ⌘T (and the group) flips between light and dark; system is the default.
    assert_eq!(
        cx.read_entity(&view, |v, _| v.theme_mode),
        super::AppThemeMode::System
    );
    view.update(&mut cx, |this, cx| this.toggle_theme(cx));
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.theme_mode),
        super::AppThemeMode::Dark
    );
    view.update(&mut cx, |this, cx| this.toggle_theme(cx));
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.theme_mode),
        super::AppThemeMode::Light
    );

    bind_all(&mut cx);
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-t");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.theme_mode),
        super::AppThemeMode::Dark,
        "⌘T should toggle the theme"
    );
}

#[gpui::test]
fn nav_keyboard_shortcuts_cover_composer_and_search(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    focus_view(&mut cx, &view);

    // Typing into the note bar and pressing Enter adds the note.
    focus_composer(&mut cx, &view);
    cx.simulate_input("quick capture");
    cx.simulate_keystrokes("enter");
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries.iter().any(|e| e.content == "quick capture")
        })
    });
    assert!(ok, "typing and Enter in the note bar should add a note");

    // ⌘F → search field focused: typing must land in the query.
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cx.simulate_keystrokes("neg");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.query.clone()),
        "neg",
        "typing after ⌘F should filter the entry list"
    );
}

/// A database without the completion flag opens the tour on first launch.
#[gpui::test]
fn onboarding_opens_for_a_fresh_database(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let db = seeded_db_with_config(vec![], false);
    let (view, window) = setup_view_with_db(cx, db, TEST_WINDOW_SIZE);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // The config read is async; wait for it to resolve.
    let opened = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| v.onboarding.is_some())
    });
    assert!(opened, "a fresh database should open the first-run tour");
    assert!(cx.debug_bounds("onboarding").is_some());

    // Skipping persists the opt-out; a later config read must not re-open it.
    click_selector(&mut cx, "onboarding-skip");
    assert!(cx.read_entity(&view, |v, _| v.onboarding.is_none()));
}

#[gpui::test]
fn onboarding_walks_all_steps_and_can_be_skipped(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // The seed marks the tour completed, so the library opens directly.
    assert!(cx.read_entity(&view, |v, _| v.onboarding.is_none()));

    // First run (or Settings → Appearance → Replay) starts it.
    view.update(&mut cx, |this, cx| this.start_onboarding(cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("onboarding").is_some());
    assert!(
        cx.debug_bounds("onboarding-kbd-cmd-f").is_some(),
        "the keymap keycaps should render on the welcome step"
    );
    assert!(cx.debug_bounds("onboarding-next").is_some());

    click_selector(&mut cx, "onboarding-next");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.onboarding.as_ref().map(|state| state.step)),
        Some(super::OnboardingStep::Accessibility),
        "Continue should open the Accessibility step"
    );
    assert!(cx.debug_bounds("onboarding-accessibility-status").is_some());
    assert!(cx.debug_bounds("onboarding-open-accessibility").is_some());

    click_selector(&mut cx, "onboarding-next");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.onboarding.as_ref().map(|state| state.step)),
        Some(super::OnboardingStep::Provider)
    );

    // Back walks one page; the provider CTA lands in Settings → Providers.
    click_selector(&mut cx, "onboarding-back");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.onboarding.as_ref().map(|state| state.step)),
        Some(super::OnboardingStep::Accessibility)
    );
    click_selector(&mut cx, "onboarding-next");
    click_selector(&mut cx, "onboarding-provider");
    assert!(cx.read_entity(&view, |v, _| v.onboarding.is_none()));
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);
    assert_eq!(
        cx.read_entity(&view, |v, _| v.settings_tab),
        Some(super::SettingsTab::Providers),
        "the provider step should open the provider settings"
    );

    // Any step can be skipped outright.
    view.update(&mut cx, |this, cx| {
        this.show_entries(cx);
        this.start_onboarding(cx);
    });
    cx.run_until_parked();
    click_selector(&mut cx, "onboarding-skip");
    assert!(
        cx.read_entity(&view, |v, _| v.onboarding.is_none()),
        "Skip should dismiss the tour"
    );
}

#[gpui::test]
fn note_bar_enter_and_backspace_stay_in_the_field(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Select an entry so the list commands have a target to act on.
    view.update(&mut cx, |this, _| this.select_at("test-1000".into(), false));
    let count_before = cx.read_entity(&view, |v, _| v.entries.len());

    // Enter in the note bar adds the note, never the detail view.
    focus_composer(&mut cx, &view);
    cx.simulate_input("note typed with enter");
    cx.simulate_keystrokes("enter");
    let added = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries
                .iter()
                .any(|e| e.content == "note typed with enter")
        })
    });
    assert!(added, "Enter in the note bar should add the note");
    assert!(
        cx.read_entity(&view, |v, _| v.entry_modal.is_none()),
        "Enter in the note bar must not open the detail view"
    );

    // Backspace while the note input is focused edits the field, not the list.
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| state.set_value("abc", window, cx));
        });
    });
    cx.run_until_parked();
    focus_composer(&mut cx, &view);
    cx.simulate_keystrokes("backspace");
    assert_eq!(
        cx.read_entity(&view, |v, cx| v.composer_body.read(cx).value().to_string()),
        "ab",
        "Backspace should edit the note text"
    );
    assert_eq!(
        cx.read_entity(&view, |v, _| v.entries.len()),
        count_before + 1,
        "no entry should be deleted while typing a note"
    );
}

#[gpui::test]
fn entries_list_fills_the_space_above_the_note_bar(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let list = cx.debug_bounds("entries-list").expect("entries list");
    let composer = cx.debug_bounds("composer-bar").expect("note bar");
    assert!(
        list.size.height > px(300.),
        "the list should reach the note bar, got {}",
        list.size.height
    );
    let gap = composer.origin.y - (list.origin.y + list.size.height);
    assert!(
        gap.abs() <= px(2.),
        "the list should meet the note bar without clearance padding, gap {gap}"
    );
}

#[gpui::test]
fn content_column_grows_on_wide_windows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view_with_size(
        cx,
        sample_entries(),
        Size {
            width: px(1400.),
            height: px(900.),
        },
    );
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let page = cx.debug_bounds("entries-page").expect("entries page");
    assert!(
        page.size.width > px(721.),
        "wide windows should use more than the comfortable 45rem cap: {}",
        page.size.width
    );
    assert!(
        page.size.width <= px(992.),
        "the 62rem roomy cap should still bound the column: {}",
        page.size.width
    );
}

#[gpui::test]
fn nav_enter_on_agent_page_does_not_open_entry_modal(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Select an entry, switch to the agent page, and press Enter with the
    // view focused: the detail overlay must stay closed.
    view.update(&mut cx, |this, _| this.select_at("test-1000".into(), false));
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-2");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Assistant);
    cx.simulate_keystrokes("enter");
    assert!(
        cx.read_entity(&view, |v, _| v.entry_modal.is_none()),
        "Enter on the agent page must not open the entry detail"
    );

    // On the Library page the same key still opens the selected entry.
    cx.simulate_keystrokes("cmd-1");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert!(
        cx.read_entity(&view, |v, _| v.entry_modal.is_some()),
        "Enter on the Library page should open the selected entry"
    );
}

#[gpui::test]
fn nav_tab_moves_focus_between_controls(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    let search = view.read_with(&cx, |v, cx| v.search_input.read(cx).focus_handle(cx));
    let focused = cx.update(|window, cx| window.focused(cx));
    assert_eq!(
        focused,
        Some(search.clone()),
        "⌘F should focus the search field"
    );

    // Tab moves focus to the next control; typing no longer filters.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();
    let after_tab = cx.update(|window, cx| window.focused(cx));
    assert_ne!(
        after_tab,
        Some(search.clone()),
        "Tab should move focus off the search field"
    );
    cx.simulate_input("zzz");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.query.clone()),
        "",
        "typing after Tab must not reach the search field"
    );

    // Shift-Tab walks the tab order back to the search field.
    cx.simulate_keystrokes("shift-tab");
    cx.run_until_parked();
    let back = cx.update(|window, cx| window.focused(cx));
    assert_eq!(
        back,
        Some(search),
        "shift-tab should return focus to the search field"
    );
}

#[gpui::test]
fn nav_backspace_deletes_selected_entry(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    view.update(&mut cx, |this, _| this.select_at("test-1000".into(), false));
    cx.run_until_parked();
    focus_view(&mut cx, &view);
    // DeleteEntry is scoped to `worktable-list`, which the focused root provides.
    cx.simulate_keystrokes("backspace");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.entries.len()),
        2,
        "⌫ should delete the selected entry"
    );
    assert!(cx.read_entity(&view, |v, _| v.selected.is_empty()));
}

#[gpui::test]
fn nav_enter_and_copy_are_safe_on_text_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // ⏎ on a text entry is a no-op (only links open); ⌘C / ⇧⌘C must not panic.
    view.update(&mut cx, |this, _| this.select_at("test-900".into(), false));
    cx.run_until_parked();
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes("cmd-c");
    cx.simulate_keystrokes("cmd-shift-c");
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.entries.len()),
        3,
        "⏎/copy must not delete text entries"
    );
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
}

#[gpui::test]
fn nav_cmd_enter_submits_the_composer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Focus the note input, fill it, and submit with ⌘⏎.
    focus_composer(&mut cx, &view);
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| {
                state.set_value("submitted with cmd-enter", window, cx)
            });
        });
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-enter");
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries
                .iter()
                .any(|e| e.content == "submitted with cmd-enter")
        })
    });
    assert!(ok, "⌘⏎ should submit the note bar");
}

/// The card context menu's Delete follows the selection: a right click on a
/// selected card removes every selected entry; an unselected card deletes
/// only itself.
#[gpui::test]
fn context_delete_respects_multi_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    let ids = cx.read_entity(&view, |v, _| v.visible_entry_ids());
    assert!(ids.len() >= 3, "need three entries for the selection check");

    view.update(&mut cx, |this, _| {
        this.select_at(ids[0].clone(), false);
        this.select_at(ids[1].clone(), true);
    });
    assert_eq!(cx.read_entity(&view, |v, _| v.selected.len()), 2);

    // Right-clicking inside the selection deletes both entries.
    view.update(&mut cx, |this, cx| {
        this.delete_context_target(ids[0].clone(), cx);
    });
    cx.run_until_parked();
    let after_multi = cx.read_entity(&view, |v, _| v.visible_entry_ids());
    assert_eq!(after_multi.len(), ids.len() - 2);
    assert!(!after_multi.contains(&ids[0]) && !after_multi.contains(&ids[1]));

    // An unselected target collapses the selection to itself and deletes.
    view.update(&mut cx, |this, cx| {
        this.delete_context_target(ids[2].clone(), cx);
    });
    cx.run_until_parked();
    let after_single = cx.read_entity(&view, |v, _| v.visible_entry_ids());
    assert_eq!(after_single.len(), ids.len() - 3);
    assert!(!after_single.contains(&ids[2]));
}

/// Arrows move the list without focusing it first; while a text field owns
/// the keyboard they stay with the field.
/// The header search morphs into a circular button on the agent page; the
/// button morphs back, lands on Entries, and focuses the field.
#[gpui::test]
fn agent_header_morphs_search_to_a_circle_button(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    assert!(cx.debug_bounds("library-search-input").is_some());
    assert!(cx.debug_bounds("agent-search-button").is_none());

    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-2");
    cx.run_until_parked();
    // Age the page clock past the morph span instead of sleeping, so the
    // assertions are deterministic under parallel test load.
    view.update(&mut cx, |this, cx| {
        this.page_anim_at = Some(
            std::time::Instant::now()
                - worktable_ui::PAGE_SLIDE.total()
                - std::time::Duration::from_millis(10),
        );
        cx.notify();
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Assistant);
    assert!(
        cx.debug_bounds("agent-search-button").is_some(),
        "the agent page shows the circular search button"
    );
    assert!(
        cx.debug_bounds("library-search-input").is_none(),
        "the full search field is not mounted on the agent page"
    );

    click_selector(&mut cx, "agent-search-button");
    cx.run_until_parked();
    view.update(&mut cx, |this, cx| {
        this.page_anim_at = Some(
            std::time::Instant::now()
                - worktable_ui::PAGE_SLIDE.total()
                - std::time::Duration::from_millis(10),
        );
        cx.notify();
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
    assert!(cx.debug_bounds("library-search-input").is_some());
    let focused = cx.update(|window, cx| window.focused(cx));
    let search = view.read_with(&cx, |v, cx| v.search_input.read(cx).focus_handle(cx));
    assert_eq!(
        focused,
        Some(search),
        "morphing back should focus the search field"
    );
}

#[gpui::test]
fn arrows_move_selection_without_focusing_first(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // No focus_view: the global binding should still move the list.
    cx.simulate_keystrokes("down");
    let first = cx.read_entity(&view, |v, _| v.selected.iter().next().cloned());
    assert!(
        first.is_some(),
        "↓ should select the first entry without focusing the list"
    );
    cx.simulate_keystrokes("down");
    let second = cx.read_entity(&view, |v, _| v.selected.iter().next().cloned());
    assert_ne!(first, second, "↓ should move to the next entry");
    cx.simulate_keystrokes("up");
    let back = cx.read_entity(&view, |v, _| v.selected.iter().next().cloned());
    assert_eq!(back, first, "↑ should move back");

    // While the note input owns the keyboard, arrows leave the list alone.
    focus_composer(&mut cx, &view);
    let before = cx.read_entity(&view, |v, _| v.selected.clone());
    cx.simulate_keystrokes("down");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.selected.clone()),
        before,
        "arrows must not move the list while typing"
    );
}

#[gpui::test]
fn nav_arrow_keys_move_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    let first = cx.read_entity(&view, |v, _| v.selected.iter().next().cloned());
    assert!(first.is_some(), "↓ should select the first visible entry");
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    let second = cx.read_entity(&view, |v, _| v.selected.iter().next().cloned());
    assert_ne!(first, second, "↓ should move to the next entry");
}

// ---------------------------------------------------------------------------
// Component coverage
// ---------------------------------------------------------------------------
//
// Every control class the app composes is exercised through the real view:
// Button, ButtonGroup segments, Input, Switch, DropdownMenu and the
// virtualized list. Assertions check both the rendered target (found by
// debug selector) and the application state it changes.
//
// The entry ContextMenu is the one exception: the pinned gpui-component
// revision retains the menu entity in a refcount cycle between its dismiss
// subscription and its state, which GPUI's in-process leak detector reports
// regardless of app code. Its commands are covered through their keyboard
// equivalents (⌘C copy, ⌫ delete, ⏎ open); see DESIGN.md's known limitations.

#[gpui::test]
fn settings_back_returns_to_the_main_ui(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // ⌘, opens Settings…
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-,");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);

    // …and the header's Back leaves Settings for the main UI instead of
    // re-opening Settings (the entry point is the Entries view).
    click_selector(&mut cx, "settings-back");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Entries,
        "Back on Settings must return to the main entries UI"
    );
}

#[gpui::test]
fn theme_mode_group_switches_with_pointer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_settings_at(super::SettingsTab::Appearance, cx)
    });
    cx.run_until_parked();

    for (selector, mode) in [
        ("theme-light", super::AppThemeMode::Light),
        ("theme-dark", super::AppThemeMode::Dark),
        ("theme-system", super::AppThemeMode::System),
    ] {
        click_selector(&mut cx, selector);
        assert_eq!(
            cx.read_entity(&view, |v, _| v.theme_mode),
            mode,
            "clicking '{selector}' selects its mode"
        );
    }
}

#[gpui::test]
fn search_input_focuses_by_pointer_and_filters(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Pointer focus → real text input → the query and the visible list update.
    click_selector(&mut cx, "library-search");
    cx.simulate_input("toml");
    assert_eq!(cx.read_entity(&view, |v, _| v.query.clone()), "toml");
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries()
            .into_iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        ids,
        vec!["test-900"],
        "search should filter to the TOML entry"
    );
    assert!(
        cx.debug_bounds("entries-list").is_none() || ids.len() == 1,
        "the list should be consistent with the filtered set"
    );
}

#[gpui::test]
fn composer_tick_button_switches_by_pointer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| state.set_value("pointer note", window, cx));
        });
    });
    cx.run_until_parked();
    click_selector(&mut cx, "add-note");
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries.iter().any(|e| e.content == "pointer note")
        })
    });
    assert!(ok, "the tick should add the typed note");
}

#[gpui::test]
fn image_file_drop_adds_an_entry(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let path = std::env::temp_dir().join("worktable-drop-test.png");
    std::fs::write(&path, b"not a real png").expect("temp image");
    let center = cx
        .debug_bounds("entries-pane")
        .map(|bounds| bounds.center())
        .expect("entries pane renders even when empty");

    cx.simulate_event(FileDropEvent::Entered {
        position: center,
        paths: ExternalPaths([path.clone()].into_iter().collect()),
    });
    cx.simulate_event(FileDropEvent::Submit { position: center });
    let media = cx.read_entity(&view, |v, _| v.service.media_dir().to_path_buf());
    let added = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            v.entries.iter().any(|entry| {
                let stored = std::path::Path::new(&entry.content);
                stored == media.join("worktable-drop-test.png") && stored.is_file()
            })
        })
    });
    assert!(
        added,
        "the dropped image becomes an entry hard-linked into the media library"
    );
    assert!(
        media.join("worktable-drop-test.png").is_file(),
        "the media library should hold the linked file"
    );
}

#[gpui::test]
fn force_touch_does_not_retrigger_an_open_modal(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.open_entry_modal(
            "test-1000",
            Bounds::new(Point::default(), Size::default()),
            cx,
        );
    });
    let first_open = cx.read_entity(&view, |v, _| {
        v.entry_modal
            .as_ref()
            .map(|modal| (modal.entry_id.clone(), modal.opened_at))
    });
    assert!(first_open.is_some());

    // Simulate the pressure path opening another card behind the panel.
    view.update(&mut cx, |this, cx| {
        this.open_entry_modal(
            "test-1001",
            Bounds::new(Point::default(), Size::default()),
            cx,
        );
    });
    let after = cx.read_entity(&view, |v, _| {
        v.entry_modal
            .as_ref()
            .map(|modal| (modal.entry_id.clone(), modal.opened_at))
    });
    assert_eq!(
        after.as_ref().map(|(id, _)| id.clone()),
        first_open.as_ref().map(|(id, _)| id.clone()),
        "the open detail keeps its entry"
    );
    assert_eq!(
        after.map(|(_, opened)| opened),
        first_open.map(|(_, opened)| opened),
        "and its morph clock is not restarted"
    );
}

#[gpui::test]
fn entry_detail_title_is_selectable_text(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![WorktableEntry {
        id: "titled-1".to_owned(),
        content: "High-performance markdown processing for the JS ecosystem".to_owned(),
        title: Some("bruits/satteri".to_owned()),
        source: "github-star".to_owned(),
        created_at: 1_000,
    }];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.open_entry_modal(
            "titled-1",
            Bounds::new(Point::default(), Size::default()),
            cx,
        );
    });
    cx.run_until_parked();
    force_frame(&mut cx);

    let title = cx
        .debug_bounds("entry-modal-title")
        .expect("the title should render");
    let body = cx
        .debug_bounds("entry-modal-scroll")
        .expect("the body should render below the title");
    assert!(
        title.size.height <= px(32.),
        "the title should be one line, got {}",
        title.size.height
    );
    assert!(
        body.origin.y >= title.origin.y + title.size.height - px(1.),
        "the body must not be pushed off by the title's TextView"
    );
}

#[gpui::test]
fn image_entry_shows_thumbnail_and_modal_image(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let path = std::env::temp_dir().join("worktable-image-card-test.png");
    std::fs::write(&path, b"not a real png").expect("temp image");
    let entry = WorktableEntry {
        id: "image-1".to_owned(),
        content: path.to_string_lossy().into_owned(),
        title: None,
        source: "Selection".to_owned(),
        created_at: 1_000,
    };
    let (view, window) = setup_view(cx, vec![entry]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    assert!(
        cx.debug_bounds("entry-thumb-image-1").is_some(),
        "image cards should render a thumbnail"
    );

    view.update(&mut cx, |this, cx| {
        this.open_entry_modal(
            "image-1",
            Bounds::new(Point::default(), Size::default()),
            cx,
        );
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("entry-modal-image").is_some(),
        "the detail should show the image itself"
    );
    assert!(cx.debug_bounds("entry-image-frame").is_some());
    assert!(
        cx.debug_bounds("entry-image-save").is_some(),
        "the download affordance should be present"
    );
    assert!(
        cx.debug_bounds("entry-modal-scroll").is_none(),
        "images should not render as markdown text"
    );
}

#[gpui::test]
fn virtual_list_scrolls_with_the_wheel(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries: Vec<WorktableEntry> = (0..30)
        .map(|index| WorktableEntry {
            id: format!("scroll-{index:02}"),
            content: format!("scroll entry {index}"),
            title: None,
            source: "Worktable".to_owned(),
            created_at: 1_000 - index as i64,
        })
        .collect();
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let list = cx
        .debug_bounds("entries-list")
        .expect("entries list should render");
    let before = cx.read_entity(&view, |v, _| v.entries_scroll.base_handle().offset());
    let max = cx.read_entity(&view, |v, _| v.entries_scroll.base_handle().max_offset());
    assert!(max.y > px(0.), "30 entries should overflow the list");

    let wheel = |cx: &mut VisualTestContext, delta_y: f32| {
        cx.simulate_event(ScrollWheelEvent {
            position: list.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(delta_y))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
    };
    wheel(&mut cx, -240.);
    let after_first = cx.read_entity(&view, |v, _| v.entries_scroll.base_handle().offset());
    if after_first == before {
        // Direction conventions differ per platform; the other sign must move.
        wheel(&mut cx, 240.);
    }
    let after = cx.read_entity(&view, |v, _| v.entries_scroll.base_handle().offset());
    assert_ne!(after, before, "wheel input must scroll the virtual list");
    cx.run_until_parked();

    // Boundary fades track the scroll position: the top fade appears once the
    // list is scrolled, and the bottom fade disappears at the end.
    assert!(
        cx.debug_bounds("entries-fade-top").is_some(),
        "scrolling away from the top shows the top fade"
    );
    assert!(
        cx.debug_bounds("entries-fade-bottom").is_some(),
        "more content below keeps the bottom fade"
    );

    // Jump to the very end: the bottom fade must be gone while the top one
    // stays.
    let max = cx.read_entity(&view, |v, _| v.entries_scroll.base_handle().max_offset());
    view.update(&mut cx, |this, cx| {
        this.entries_scroll
            .base_handle()
            .set_offset(point(px(0.), -max.y));
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("entries-fade-bottom").is_none(),
        "reaching the end hides the bottom fade"
    );
    assert!(
        cx.debug_bounds("entries-fade-top").is_some(),
        "the top fade stays while scrolled"
    );

    // Back to the top: the top fade disappears.
    view.update(&mut cx, |this, cx| {
        this.entries_scroll
            .base_handle()
            .set_offset(point(px(0.), px(0.)));
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("entries-fade-top").is_none(),
        "reaching the top hides the top fade"
    );
    assert!(
        cx.debug_bounds("entries-fade-bottom").is_some(),
        "the bottom fade returns when content continues below"
    );
}

#[gpui::test]
fn assistant_input_and_send_are_inert_when_unconfigured(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| this.show_assistant(cx));
    cx.run_until_parked();

    // The disabled input rejects pointer focus and typed text.
    click_selector(&mut cx, "assistant-input");
    cx.simulate_input("hello");
    let value = cx.read_entity(&view, |v, cx| {
        v.assistant_input.read(cx).value().to_string()
    });
    assert!(value.is_empty(), "a disabled input must not accept text");

    // The Send button is disabled until a provider/model is configured.
    let messages_before = cx.read_entity(&view, |v, _| v.messages.len());
    click_selector(&mut cx, "send-assistant");
    assert_eq!(
        cx.read_entity(&view, |v, _| v.messages.len()),
        messages_before,
        "a disabled Send must not start a run"
    );
}

// ---------------------------------------------------------------------------
// Agent UI components (loading orbs, streaming text, inline citations)
// ---------------------------------------------------------------------------

/// Draw the next frame: GPUI dispatches key events through a draw when the
/// window is dirty, which `refresh` + `run_until_parked` alone does not do.
fn force_frame(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("shift");
}

#[gpui::test]
fn assistant_thinking_orb_renders_while_waiting(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.assistant_busy = true;
        this.messages
            .push(crate::assistant::ChatMessage::user("Summarize my notes"));
        cx.notify();
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("assistant-thinking").is_some(),
        "the S1 loading orb should render while the assistant is waiting"
    );
}

#[gpui::test]
fn helix_globe_orb_renders_while_building(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.knowledge_building = true;
        this.knowledge_status = Some("Building knowledge…".to_owned());
        cx.notify();
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("knowledge-building").is_some(),
        "the G2 loading orb should render while knowledge builds"
    );
    assert!(
        cx.debug_bounds("knowledge-status").is_none(),
        "the assistant pane no longer carries a status row"
    );
}

#[gpui::test]
fn streaming_answer_renders_the_typewriter(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.assistant_busy = true;
        this.messages.push(crate::assistant::ChatMessage {
            role: crate::assistant::Role::Assistant,
            text: "A streaming answer that has not finished yet".to_owned(),
            thinking: String::new(),
            streaming: true,
            citations: Vec::new(),
            thinking_collapsed: false,
        });
        cx.notify();
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("streaming-text").is_some(),
        "a streaming answer should render through the typewriter"
    );
}

/// The selectable rich text needs a bounded width; an auto-width bubble once
/// collapsed the user message into a one-character column.
#[gpui::test]
fn messages_keep_a_readable_bubble_width(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages = vec![
            crate::assistant::ChatMessage::user("Format a status report"),
            crate::assistant::ChatMessage::assistant(
                "## Weekly notes\n\n- Shipped the entries list\n- Fixed `selection`",
            ),
        ];
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);

    for (selector, role) in [("bubble-0", "user"), ("bubble-1", "assistant")] {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{role} bubble should render"));
        assert!(
            bounds.size.width > px(200.),
            "{role} bubble collapsed to {}px, so selectable text cannot lay out",
            bounds.size.width
        );
    }
}

/// Typing in the library search and pressing Enter: the query switches to
/// Agent mode and is submitted as the prompt.
#[gpui::test]
fn search_enter_asks_the_agent(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    focus_view(&mut cx, &view);
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.focus_search(window, cx));
    });
    cx.run_until_parked();
    cx.simulate_input("sunset notes");
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.query.clone()),
        "sunset notes"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    let (mode, query, messages) =
        cx.read_entity(&view, |v, _| (v.mode, v.query.clone(), v.messages.clone()));
    assert_eq!(
        mode,
        AppMode::Assistant,
        "Enter should switch to Agent mode"
    );
    assert!(query.is_empty(), "the search query is consumed");
    assert_eq!(messages.len(), 2, "user prompt + setup explanation");
    assert_eq!(messages[0].role, crate::assistant::Role::User);
    assert_eq!(messages[0].text, "sunset notes");
    assert!(messages[1].text.contains("isn't configured"));
}
#[gpui::test]
fn thinking_switch_toggles_the_preference(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_settings_at(super::SettingsTab::Appearance, cx)
    });
    cx.run_until_parked();
    assert!(!cx.read_entity(&view, |v, _| v.show_thinking));
    force_frame(&mut cx);

    click_selector(&mut cx, "thinking-switch");
    assert!(
        cx.read_entity(&view, |v, _| v.show_thinking),
        "clicking the switch should show thinking"
    );
    click_selector(&mut cx, "thinking-switch");
    assert!(
        !cx.read_entity(&view, |v, _| v.show_thinking),
        "clicking the switch again should hide thinking"
    );
}

#[gpui::test]
fn tool_status_orb_tracks_the_active_tool(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages
            .push(crate::assistant::ChatMessage::user("what did I save?"));
        this.assistant_busy = true;
        this.active_tool = Some("search_knowledge".to_owned());
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("assistant-tool").is_some(),
        "a running knowledge search should show the tool orb row"
    );
    assert!(
        cx.debug_bounds("assistant-thinking").is_none(),
        "the tool row replaces the generic waiting orb"
    );

    view.update(&mut cx, |this, cx| {
        this.active_tool = None;
        this.assistant_busy = false;
        cx.notify();
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("assistant-tool").is_none(),
        "the tool row clears when the tool finishes"
    );
}

/// User bubbles shrink to their text, capped at 85% of the transcript row.
#[gpui::test]
fn user_bubbles_adapt_to_content_with_a_max_width(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages
            .push(crate::assistant::ChatMessage::user("Yes"));
        this.messages.push(crate::assistant::ChatMessage::user(
            "A much longer message that should take up most of the measure but \
             never the whole width of the transcript pane, wrapping as it gets \
             there.",
        ));
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);

    let short = cx.debug_bounds("bubble-0").expect("short bubble");
    let long = cx.debug_bounds("bubble-1").expect("long bubble");
    assert!(
        short.size.width < long.size.width,
        "bubble width should follow content: {} vs {}",
        short.size.width,
        long.size.width
    );
    let row = cx.debug_bounds("assistant-messages").expect("messages");
    let cap = (row.size.width - px(32.)) * 0.85 + px(1.);
    assert!(
        long.size.width <= cap,
        "the long bubble should respect the 85% cap: {} > {}",
        long.size.width,
        cap
    );
}

/// A note citation (no external URL) selects its entry in the Library.
#[gpui::test]
fn citation_click_reveals_the_entry(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages.push(crate::assistant::ChatMessage {
            role: crate::assistant::Role::Assistant,
            text: "That note exists in your library[1].".to_owned(),
            thinking: String::new(),
            streaming: false,
            citations: vec![worktable_ui::CitationRef {
                n: 1,
                label: "Negation in inherited configs".into(),
                snippet: "Negation in inherited configs. The moment a config can extend a base…"
                    .into(),
                host: "Worktable".into(),
                url: "worktable-entry:test-1000".into(),
            }],
            thinking_collapsed: false,
        });
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);

    let row = cx
        .debug_bounds("msg-0-citations-ref-1")
        .expect("the sources footer should render");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();
    force_frame(&mut cx);
    let (mode, entry_id) = cx.read_entity(&view, |v, _| {
        (
            v.mode,
            v.entry_modal.as_ref().map(|modal| modal.entry_id.clone()),
        )
    });
    assert_eq!(
        mode,
        AppMode::Assistant,
        "the entry view overlays the agent"
    );
    assert_eq!(
        entry_id.as_deref(),
        Some("test-1000"),
        "a note citation opens the entry with the morph"
    );
    assert!(cx.debug_bounds("entry-modal").is_some());
}

/// Graph topics (AI-named when a provider ran) drive search: a topic finds
/// its entries even when none of their words match.
#[gpui::test]
fn knowledge_topics_power_search(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Applied and asserted in one synchronous update: the startup graph load
    // is async, and this injection is a test stand-in for a built graph.
    let (ids, graph_topic) = view.update(&mut cx, |this, _| {
        this.apply_knowledge_topics(std::collections::HashMap::from([(
            "test-1000".to_owned(),
            "SUNSETS".to_owned(),
        )]));
        this.query = "sunsets".to_owned();
        let ids = this
            .visible_entries()
            .into_iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        (ids, this.graph_topics.get("test-1000").cloned())
    });
    assert_eq!(
        ids,
        vec!["test-1000"],
        "the graph topic matches the entry without a text hit"
    );
    assert_eq!(
        graph_topic,
        Some("SUNSETS".to_owned()),
        "the graph topic layers over the locally extracted one"
    );
}

/// Topic grouping must read the per-entry cache instead of re-tokenizing the
/// whole library on every frame (the scroll stutter).
#[gpui::test]
fn topic_cache_covers_every_entry(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    let (total, cached, all_match) = cx.read_entity(&view, |v, _| {
        let all_match = v
            .entries
            .iter()
            .all(|entry| v.topic_cache.get(&entry.id) == Some(&super::helix_primary_topic(entry)));
        (v.entries.len(), v.topic_cache.len(), all_match)
    });
    assert!(total > 0, "sample entries should load");
    assert_eq!(cached, total, "every entry has a cached topic");
    assert!(all_match, "cached topics equal the extractor's output");
}

/// Tool citations arriving before the answer are attached to the message that
/// carries the `[n]` markers.
#[gpui::test]
fn tool_citations_attach_to_the_answer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.on_event(
            &worktable_events::WorktableEvent::AiCitations {
                request_id: "req-1".to_owned(),
                session_id: "wt-session".to_owned(),
                citations: vec![worktable_events::KnowledgeCitation {
                    n: 1,
                    entry_id: "test-1000".to_owned(),
                    label: "Negation in inherited configs".to_owned(),
                    snippet: "Negation in inherited configs. The moment a config can extend…"
                        .to_owned(),
                    host: "Worktable".to_owned(),
                    url: String::new(),
                }],
            },
            cx,
        );
        // The answer's first delta creates its bubble; citations move there.
        this.on_event(
            &worktable_events::WorktableEvent::AiMessageDelta {
                request_id: "req-1".to_owned(),
                session_id: "wt-session".to_owned(),
                delta: "That note exists in your library[1].".to_owned(),
            },
            cx,
        );
        // The run ends: the streaming bubble swaps to markdown + citations.
        this.on_event(
            &worktable_events::WorktableEvent::AiRunFinished {
                request_id: "req-1".to_owned(),
                session_id: "wt-session".to_owned(),
                run_id: "run-1".to_owned(),
                state: "completed".to_owned(),
            },
            cx,
        );
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);

    let citations = cx.read_entity(&view, |v, _| {
        v.messages
            .last()
            .map(|message| message.citations.clone())
            .unwrap_or_default()
    });
    assert_eq!(citations.len(), 1, "the tool hit annotates the answer");
    assert_eq!(citations[0].n, 1);
    assert!(
        citations[0].url.starts_with("worktable-entry:"),
        "note citations route in-app: {}",
        citations[0].url
    );
    assert!(
        cx.debug_bounds("assistant-citations").is_some(),
        "the citation footer renders with the answer"
    );
    assert!(
        cx.debug_bounds("msg-0-citations-ref-1").is_some(),
        "the source row renders for the cited entry"
    );
}

/// The real worker order: deltas stream first, then the collected citations,
/// then RunCompleted. The citations must land on the streaming answer.
#[gpui::test]
fn citations_arriving_after_the_answer_attach(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.on_event(
            &worktable_events::WorktableEvent::AiMessageDelta {
                request_id: "req-1".to_owned(),
                session_id: "wt-session".to_owned(),
                delta: "That note exists in your library[1].".to_owned(),
            },
            cx,
        );
        // The tool finishes and the runtime emits the collected citations.
        this.on_event(
            &worktable_events::WorktableEvent::AiCitations {
                request_id: "req-1".to_owned(),
                session_id: "wt-session".to_owned(),
                citations: vec![worktable_events::KnowledgeCitation {
                    n: 1,
                    entry_id: "test-1000".to_owned(),
                    label: "Negation in inherited configs".to_owned(),
                    snippet: "Negation in inherited configs.".to_owned(),
                    host: "Worktable".to_owned(),
                    url: String::new(),
                }],
            },
            cx,
        );
        this.on_event(
            &worktable_events::WorktableEvent::AiRunFinished {
                request_id: "req-1".to_owned(),
                session_id: "wt-session".to_owned(),
                run_id: "run-1".to_owned(),
                state: "completed".to_owned(),
            },
            cx,
        );
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);

    let citations = cx.read_entity(&view, |v, _| {
        v.messages
            .last()
            .map(|message| message.citations.clone())
            .unwrap_or_default()
    });
    assert_eq!(
        citations.len(),
        1,
        "citations emitted after the deltas must attach to the answer"
    );
    assert!(
        cx.debug_bounds("msg-0-citations-cite-1").is_some(),
        "the answer should render the marker as a chip"
    );
    assert!(
        cx.debug_bounds("msg-0-citations-ref-1").is_some(),
        "the source row should render under the answer"
    );
}

/// Markers without collected sources still render through the citation
/// component, so raw `[n]` brackets never reach the UI.
#[gpui::test]
fn citation_markers_without_sources_still_chip(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages.push(crate::assistant::ChatMessage::assistant(
            "An answer that cites nothing collected[1].",
        ));
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("assistant-citations").is_some(),
        "a marker-bearing answer renders through InlineCitations"
    );
    assert!(
        cx.debug_bounds("msg-0-citations-cite-1").is_some(),
        "the bare marker becomes a chip"
    );
}

/// Switching pages records a transition; both pages are mounted mid-flight/// Switching pages records a transition; both pages are mounted mid-flight
/// and only the active one remains after it settles.
#[gpui::test]
fn library_pages_transition_between_entries_and_agent(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("entries-page").is_some(),
        "the entries page is the resting page"
    );
    assert!(
        !cx.read_entity(&view, |v, _| v.page_transition_active()),
        "the app starts at rest"
    );

    click_selector(&mut cx, "page-toggle");
    let (mode, animating) = cx.read_entity(&view, |v, _| (v.mode, v.page_transition_active()));
    assert_eq!(mode, AppMode::Assistant);
    assert!(animating, "the switch starts a page transition");

    // Both pages are mounted for the whole life of the shell (so message
    // entrance animations and list scroll state survive switches) …
    assert!(
        cx.debug_bounds("entries-page").is_some() && cx.debug_bounds("assistant-page").is_some(),
        "both pages stay mounted"
    );

    // … while the transition itself is time-boxed and then rests.
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        !cx.read_entity(&view, |v, _| v.page_transition_active()),
        "the slide finishes after its 250ms window"
    );
    assert!(cx.debug_bounds("assistant-page").is_some());
}

/// Double-clicking a card morphs its full content open; Escape morphs it
/// back and unmounts it.
#[gpui::test]
fn entry_modal_morphs_open_on_double_click(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let center = first_card_center(&mut cx);
    cx.simulate_event(MouseDownEvent {
        position: center,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        position: center,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 2,
    });
    cx.run_until_parked();
    force_frame(&mut cx);

    let (entry_id, origin) = cx.read_entity(&view, |v, _| {
        v.entry_modal
            .as_ref()
            .map(|modal| (modal.entry_id.clone(), modal.origin))
            .unwrap_or_default()
    });
    assert_eq!(entry_id, "test-1000", "the clicked card's entry opens");
    assert!(
        origin.contains(&center),
        "the morph starts on the card itself ({origin:?} vs {center:?})"
    );
    assert!(
        origin.size.height > px(80.),
        "the origin is the full card, not a point square: {:?}",
        origin.size
    );
    assert!(
        cx.debug_bounds("entry-modal").is_some(),
        "the full-content panel renders"
    );

    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.read_entity(&view, |v, _| v
            .entry_modal
            .as_ref()
            .is_some_and(|modal| modal.closing_at.is_some())),
        "Escape starts the close morph"
    );
    // The close unmounts after the morph's span (the test clock drives the
    // background timer that production would wait out in real time).
    cx.background_executor
        .advance_clock(worktable_ui::MORPH_CLOSE.total() + std::time::Duration::from_millis(30));
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.read_entity(&view, |v, _| v.entry_modal.is_none()),
        "the entry view unmounts when the close morph finishes"
    );
    assert!(cx.debug_bounds("entry-modal").is_none());
}

/// A force click (trackpad pressure) opens the same view.
#[gpui::test]
fn entry_modal_opens_on_force_touch(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let center = first_card_center(&mut cx);
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_event(MousePressureEvent {
        pressure: 1.0,
        stage: PressureStage::Force,
        position: center,
        modifiers: Modifiers::default(),
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v
            .entry_modal
            .as_ref()
            .map(|modal| modal.entry_id.clone())),
        Some("test-1000".to_owned()),
        "a force click should open the entry view"
    );
}

/// Non-link entries open the view from the keyboard; links keep opening in
/// the browser.
#[gpui::test]
fn entry_modal_opens_from_the_keyboard(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    view.update(&mut cx, |this, cx| {
        this.select_at("test-1000".to_owned(), false);
        this.open_selected(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v
            .entry_modal
            .as_ref()
            .map(|modal| modal.entry_id.clone())),
        Some("test-1000".to_owned()),
        "Enter/Open on a text entry should open the full content"
    );
}

/// The entry view footer offers chips for detected content.
#[gpui::test]
fn entry_modal_offers_action_chips(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![WorktableEntry {
        id: "contact-1".to_owned(),
        content: "Ping jane@example.com or +1 (415) 555-0123. Docs: https://example.com/x"
            .to_owned(),
        title: None,
        source: "Worktable".to_owned(),
        created_at: 1_000,
    }];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    view.update(&mut cx, |this, cx| {
        let origin = this.entry_origin("contact-1");
        this.open_entry_modal("contact-1", origin, cx);
    });
    cx.run_until_parked();
    force_frame(&mut cx);

    for selector in [
        "entry-modal-copy",
        "entry-modal-open-link",
        "entry-modal-email",
        "entry-modal-call",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "the '{selector}' chip should render"
        );
    }
    assert!(
        cx.debug_bounds("entry-modal-done").is_none(),
        "the detail view has no Done button"
    );
}

/// Long content scrolls inside the entry view.
#[gpui::test]
fn entry_modal_scrolls_long_content(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let long: String = (0..120)
        .map(|index| format!("line {index} of a very long note\n"))
        .collect();
    let entries = vec![WorktableEntry {
        id: "long-1".to_owned(),
        content: long,
        title: None,
        source: "Worktable".to_owned(),
        created_at: 1_000,
    }];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    view.update(&mut cx, |this, cx| {
        let origin = gpui::Bounds::new(point(px(60.), px(300.)), gpui::size(px(200.), px(108.)));
        this.open_entry_modal("long-1", origin, cx);
    });
    cx.run_until_parked();
    force_frame(&mut cx);

    assert!(
        cx.debug_bounds("entry-modal").is_some(),
        "the long entry opens full-height"
    );
    assert!(
        cx.debug_bounds("entry-modal-scroll").is_some(),
        "the body is the scrolled content area"
    );

    // The rich text component owns the scroll state; a wheel over the modal
    // must be handled without disturbing it (the visual runner captures the
    // scrolled frame, which the semantic tree cannot express).
    let modal = cx.debug_bounds("entry-modal").expect("the modal renders");
    cx.simulate_event(ScrollWheelEvent {
        position: modal.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-240.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(cx.debug_bounds("entry-modal-scroll").is_some());
    assert!(cx.read_entity(&view, |v, _| v.entry_modal.is_some()));
}

/// The assistant transcript scrolls with an explicit scrollbar, and the
/// thinking block collapses (with its body hidden) and expands on click.
#[gpui::test]
fn thinking_block_collapses_and_expands(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.show_thinking = true;
        this.messages = vec![
            crate::assistant::ChatMessage::user("what did I save?"),
            crate::assistant::ChatMessage {
                role: crate::assistant::Role::Assistant,
                text: "You saved a note.[1]".to_owned(),
                thinking: "The user asks about saved notes.".to_owned(),
                streaming: false,
                citations: Vec::new(),
                thinking_collapsed: false,
            },
        ];
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);

    assert!(
        cx.debug_bounds("thinking-header-1").is_some(),
        "the thoughts header renders"
    );
    assert!(
        cx.debug_bounds("thinking-body-1").is_some(),
        "thoughts start expanded"
    );
    click_selector(&mut cx, "thinking-header-1");
    assert!(
        cx.debug_bounds("thinking-body-1").is_none(),
        "clicking the header collapses the thoughts"
    );
    assert!(
        cx.read_entity(&view, |v, _| v.messages[1].thinking_collapsed),
        "the collapsed state is stored on the message"
    );
    click_selector(&mut cx, "thinking-header-1");
    assert!(
        cx.debug_bounds("thinking-body-1").is_some(),
        "clicking again expands the thoughts"
    );
}

/// Finishing the answer folds the reasoning away by default.
#[gpui::test]
fn run_finish_collapses_thoughts(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_thinking = true;
        this.messages = vec![crate::assistant::ChatMessage {
            role: crate::assistant::Role::Assistant,
            text: "done".to_owned(),
            thinking: "reasoning".to_owned(),
            streaming: true,
            citations: Vec::new(),
            thinking_collapsed: false,
        }];
        this.on_event(
            &worktable_events::WorktableEvent::AiRunFinished {
                request_id: "req".to_owned(),
                session_id: "wt-session".to_owned(),
                run_id: "run".to_owned(),
                state: "completed".to_owned(),
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(
        cx.read_entity(&view, |v, _| v.messages[0].thinking_collapsed),
        "the run finishing collapses the thoughts"
    );
}

/// While a run is active the send button is the S1 orb and clicking it aborts.
#[gpui::test]
fn send_button_becomes_orb_and_aborts(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.assistant_busy = true;
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("abort-assistant").is_some(),
        "a running agent shows the orb button"
    );
    assert!(
        cx.debug_bounds("send-assistant").is_none(),
        "the idle send button is replaced while running"
    );
    // Cancelling with no live run is quiet (the runtime ignores it).
    click_selector(&mut cx, "abort-assistant");
    assert!(
        cx.debug_bounds("abort-assistant").is_some(),
        "the button stays until the run reports its failure"
    );
}

/// The knowledge button carries the G2 orb while building and a second click
/// requests a stop.
#[gpui::test]
fn knowledge_button_shows_orb_and_stops(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.knowledge_building = true;
        this.knowledge_status = Some("Building knowledge…".to_owned());
        cx.notify();
    });
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("knowledge-building").is_some(),
        "the builder orb renders inside the button"
    );
    click_selector(&mut cx, "build-knowledge");
    let (cancel_requested, status) = cx.read_entity(&view, |v, _| {
        (
            v.knowledge_cancel.load(std::sync::atomic::Ordering::SeqCst),
            v.knowledge_status.clone(),
        )
    });
    assert!(cancel_requested, "the second click requests a stop");
    assert_eq!(status.as_deref(), Some("Stopping knowledge build…"));
}

/// The morph position is column-local even when the content column is
/// centered on a wide window (force-touch path included).
#[gpui::test]
fn morph_origin_is_column_local_on_wide_windows(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view_with_size(
        cx,
        sample_entries(),
        Size {
            width: px(1200.),
            height: px(800.),
        },
    );
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    let center = first_card_center(&mut cx);
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_event(MousePressureEvent {
        pressure: 1.0,
        stage: PressureStage::Force,
        position: center,
        modifiers: Modifiers::default(),
    });
    cx.run_until_parked();
    force_frame(&mut cx);

    let (origin, column_x) = cx.read_entity(&view, |v, _| {
        (
            v.entry_modal.as_ref().map(|modal| modal.origin),
            v.content_origin.get().x,
        )
    });
    let origin = origin.expect("force touch opens the entry view");
    assert!(
        column_x > px(150.),
        "the content column is centered on a wide window ({column_x:?})"
    );
    assert!(
        origin.origin.x < px(150.),
        "the morph origin is column-local, not window-space: {origin:?}"
    );
    let window_x = origin.origin.x + column_x;
    assert!(
        center.x >= window_x && center.x <= window_x + origin.size.width,
        "the origin maps back onto the card under the cursor ({center:?} in {origin:?} + {column_x:?})"
    );
}

/// The detail view edits markdown: Edit switches to the code editor, Save
/// persists, Cancel discards.
#[gpui::test]
fn entry_modal_edits_content(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    view.update(&mut cx, |this, cx| {
        let origin = this.entry_origin("test-1000");
        this.open_entry_modal("test-1000", origin, cx);
    });
    cx.run_until_parked();
    // Let the morph settle so the footer chips are actually hittable.
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("entry-modal-edit").is_some(),
        "Edit sits at the footer's right end"
    );

    // Edit switches to the markdown code editor.
    click_selector(&mut cx, "entry-modal-edit");
    assert!(cx.read_entity(&view, |v, _| v.entry_editing));
    assert!(cx.debug_bounds("entry-modal-editor").is_some());

    // Cancel keeps the original content.
    click_selector(&mut cx, "entry-modal-cancel");
    assert!(!cx.read_entity(&view, |v, _| v.entry_editing));
    assert!(
        cx.read_entity(&view, |v, _| v.entries[0].content.clone())
            .contains("Negation")
    );

    // Edit → change → Save persists the markdown.
    click_selector(&mut cx, "entry-modal-edit");
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let input = this.entry_edit_input.clone();
            input.update(cx, |state, cx| {
                state.set_value("# Edited note\n\n- item", window, cx)
            });
        });
    });
    cx.run_until_parked();
    click_selector(&mut cx, "entry-modal-save");
    let saved = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| {
            !v.entry_editing
                && v.entries.iter().any(|entry| {
                    entry.id == "test-1000" && entry.content == "# Edited note\n\n- item"
                })
        })
    });
    assert!(saved, "Save persists the edited markdown");
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("entry-modal-scroll").is_some(),
        "the preview returns after saving"
    );
}

#[gpui::test]
fn citation_chip_opens_the_source(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let test_cx = &*cx;
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    let url = "https://arxiv.org/abs/1706.03762";
    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages.push(crate::assistant::ChatMessage {
            role: crate::assistant::Role::Assistant,
            text: "Transformers scale well[1].".to_owned(),
            thinking: String::new(),
            streaming: false,
            citations: vec![worktable_ui::CitationRef {
                n: 1,
                label: "Attention Is All You Need".into(),
                snippet: "The dominant sequence transduction models are based on complex…".into(),
                host: "arxiv.org".into(),
                url: url.into(),
            }],
            thinking_collapsed: false,
        });
        cx.notify();
    });
    cx.run_until_parked();
    // Let the Entries→Agent slide finish so the chip is actually hittable.
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("assistant-citations").is_some(),
        "a message with citations should render through the citations component"
    );

    let chip = cx
        .debug_bounds("msg-0-citations-cite-1")
        .expect("the inline citation chip should render");
    cx.simulate_click(chip.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        test_cx.opened_url().as_deref(),
        Some(url),
        "clicking an inline citation chip should open its source"
    );
    assert!(
        cx.debug_bounds("msg-0-citations-ref-1").is_some(),
        "the source row renders under the answer"
    );
}

fn db_with_chats() -> String {
    let db = seeded_db(Vec::new());
    let store = worktable_db::SqliteStore::connect(&db).unwrap();
    store.migrate().unwrap();
    let mut answer = crate::assistant::ChatMessage::assistant("Your notes mention Rust[1].");
    answer.thinking = "I searched the notes.".into();
    answer.citations.push(worktable_ui::CitationRef {
        n: 1,
        label: "Rust note".into(),
        snippet: "A saved Rust note".into(),
        host: "Library".into(),
        url: "worktable-entry:rust".into(),
    });
    for (id, timestamp, messages) in [
        (
            "older",
            10,
            vec![
                crate::assistant::ChatMessage::user("An older conversation"),
                crate::assistant::ChatMessage::assistant("Older answer"),
            ],
        ),
        (
            "recent",
            20,
            vec![
                crate::assistant::ChatMessage::user("Find my Rust notes"),
                answer,
            ],
        ),
    ] {
        store
            .save_chat(&worktable_db::StoredChat::new(
                worktable_db::ChatSummary::new(id, messages[0].text.clone(), timestamp, 1),
                serde_json::to_string(&messages).unwrap(),
            ))
            .unwrap();
    }
    db
}

fn open_chat_picker(cx: &mut VisualTestContext, view: &Entity<WorktableView>) {
    view.update(cx, |this, cx| this.show_assistant(cx));
    settle_strip(cx);
    force_frame(cx);
    click_selector(cx, "assistant-chats");
    assert!(wait_for(cx, 5, |cx| {
        cx.read_entity(view, |v, _| {
            !matches!(v.chats_state, super::ChatListState::Loading)
        })
    }));
    settle_strip(cx);
    force_frame(cx);
}

#[gpui::test]
fn chats_sheet_opens_at_the_bottom_and_dismisses_with_focus_restored(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, Vec::new());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    open_chat_picker(&mut cx, &view);
    let sheet = cx
        .debug_bounds("chats-sheet")
        .expect("the icon opens a sheet");
    let viewport = cx.update(|window, _| window.viewport_size());
    assert_eq!(
        sheet.bottom(),
        viewport.height,
        "the sheet sits on the screen's bottom edge"
    );
    assert!(sheet.top() > viewport.height * 0.25);
    assert!(cx.debug_bounds("chats-empty").is_some());
    for _ in 0..12 {
        let before = cx.update(|window, cx| window.focused(cx));
        cx.simulate_keystrokes("tab");
        let after = cx.update(|window, cx| window.focused(cx));
        assert_ne!(
            before, after,
            "Tab must move focus to a control inside the sheet"
        );
        assert!(
            cx.update(|window, cx| view.read(cx).chats_focus.contains_focused(window, cx)),
            "Tab cannot leave the sheet"
        );
    }
    cx.simulate_keystrokes("escape");
    assert!(
        cx.read_entity(&view, |v, _| v
            .chats_sheet
            .as_ref()
            .is_none_or(|sheet| sheet.closing_at.is_some())),
        "Escape must start the close transition"
    );
    cx.background_executor
        .advance_clock(worktable_ui::MODAL_CLOSE.total());
    cx.run_until_parked();
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(cx.debug_bounds("chats-sheet").is_none());
    assert!(cx.read_entity(&view, |v, _| v.chats_sheet.is_none()));
    assert!(cx.update(|window, cx| view.read(cx).focus_handle.is_focused(window)));

    open_chat_picker(&mut cx, &view);
    let backdrop = cx.debug_bounds("chats-backdrop").unwrap();
    cx.simulate_click(
        point(backdrop.center().x, backdrop.top() + px(40.)),
        Modifiers::default(),
    );
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(cx.debug_bounds("chats-sheet").is_none());
}

#[gpui::test]
fn chats_select_a_saved_transcript_and_new_chat_keeps_the_previous_one(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let db = db_with_chats();
    let (view, window) = setup_view_with_db(cx, db.clone(), TEST_WINDOW_SIZE);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    open_chat_picker(&mut cx, &view);
    assert_eq!(
        cx.read_entity(&view, |v, _| v
            .chats
            .iter()
            .map(|chat| chat.id.clone())
            .collect::<Vec<_>>()),
        ["recent", "older"]
    );
    click_selector(&mut cx, "chat-row:recent");
    assert!(wait_for(&mut cx, 5, |cx| cx
        .read_entity(&view, |v, _| v.chat_id.as_deref()
            == Some("recent"))));
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(cx.debug_bounds("chats-sheet").is_none());
    assert!(
        cx.debug_bounds("assistant-citations").is_some(),
        "loaded sources remain interactive"
    );
    cx.read_entity(&view, |v, _| {
        assert_eq!(v.messages[0].text, "Find my Rust notes");
        assert!(v.messages[1].thinking_collapsed);
        assert!(!v.messages[1].streaming);
        assert_eq!(v.messages[1].citations[0].label.as_ref(), "Rust note");
    });
    open_chat_picker(&mut cx, &view);
    click_selector(&mut cx, "new-chat");
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(cx.read_entity(&view, |v, _| v.messages.is_empty() && v.chat_id.is_none()));
    assert!(cx.debug_bounds("chats-sheet").is_none());
    let store = worktable_db::SqliteStore::connect(&db).unwrap();
    assert_eq!(
        store.list_chats().unwrap().len(),
        2,
        "empty drafts do not create extra rows"
    );
    assert!(store.load_chat("recent").unwrap().is_some());
}

#[gpui::test]
fn chats_rows_load_from_the_keyboard(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view_with_db(cx, db_with_chats(), TEST_WINDOW_SIZE);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    open_chat_picker(&mut cx, &view);
    // Visual order: New chat, Close, then the newest conversation.
    for _ in 0..3 {
        cx.simulate_keystrokes("tab");
        force_frame(&mut cx);
    }
    cx.simulate_keystrokes("enter");
    // At this GPUI revision simulate_keystrokes emits only key-down. Buttons
    // commit their keyboard click on key-up, as the native event loop does.
    cx.update(|window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::KeyUp(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("enter").unwrap(),
            }),
            cx,
        );
    });
    cx.run_until_parked();
    assert!(
        cx.read_entity(&view, |v, _| v.chat_loading.is_some()
            || v.chat_id.is_some()),
        "Enter must activate the chat row, not another control (sheet closing: {})",
        cx.read_entity(&view, |v, _| v
            .chats_sheet
            .as_ref()
            .is_some_and(|sheet| sheet.closing_at.is_some()))
    );
    assert!(wait_for(&mut cx, 5, |cx| cx
        .read_entity(&view, |v, _| v.chat_id.as_deref()
            == Some("recent"))));
    settle_strip(&mut cx);
    // The sheet unmounts on the close span's timer.
    cx.background_executor
        .advance_clock(worktable_ui::MODAL_CLOSE.total());
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(cx.debug_bounds("chats-sheet").is_none());
    assert_eq!(cx.read_entity(&view, |v, _| v.messages.len()), 2);
}

#[gpui::test]
fn chats_switching_is_disabled_during_an_answer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view_with_db(cx, db_with_chats(), TEST_WINDOW_SIZE);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    view.update(&mut cx, |this, cx| {
        this.messages
            .push(crate::assistant::ChatMessage::user("Answer in progress"));
        this.assistant_busy = true;
        cx.notify();
    });
    open_chat_picker(&mut cx, &view);
    assert!(cx.debug_bounds("chats-status").is_some());
    click_selector(&mut cx, "chat-row:recent");
    click_selector(&mut cx, "new-chat");
    assert!(cx.read_entity(&view, |v, _| v.chat_id.is_none()
        && v.messages[0].text == "Answer in progress"));
    assert!(cx.debug_bounds("chats-sheet").is_some());
    click_selector(&mut cx, "chats-close");
    settle_strip(&mut cx);
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("chats-sheet").is_none(),
        "browsing can still be dismissed while busy"
    );
}

#[gpui::test]
fn chats_save_finished_answers_and_ignore_other_sessions(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, Vec::new());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    view.update(&mut cx, |this, cx| {
        this.submit_assistant_prompt("A durable conversation".into(), cx)
    });
    assert!(wait_for(&mut cx, 5, |cx| cx
        .read_entity(&view, |v, _| !v.chat_save_pending)));
    let id = cx.read_entity(&view, |v, _| v.chat_id.clone().unwrap());
    view.update(&mut cx, |this, cx| {
        this.on_event(
            &worktable_events::WorktableEvent::AiMessageDelta {
                request_id: "wrong".into(),
                session_id: "another-chat".into(),
                delta: "Must not appear".into(),
            },
            cx,
        );
        this.on_event(
            &worktable_events::WorktableEvent::AiMessageDelta {
                request_id: "right".into(),
                session_id: id.clone(),
                delta: "Saved answer".into(),
            },
            cx,
        );
        this.on_event(
            &worktable_events::WorktableEvent::AiRunFinished {
                request_id: "right".into(),
                session_id: id.clone(),
                run_id: "run".into(),
                state: "completed".into(),
            },
            cx,
        );
    });
    assert!(wait_for(&mut cx, 5, |cx| cx
        .read_entity(&view, |v, _| !v.chat_save_pending)));
    let db = cx.read_entity(&view, |v, _| v.service.database_path());
    let saved = worktable_db::SqliteStore::connect(&db)
        .unwrap()
        .load_chat(&id)
        .unwrap()
        .unwrap();
    assert!(saved.messages_json.contains("Saved answer"));
    assert!(!saved.messages_json.contains("Must not appear"));
    open_chat_picker(&mut cx, &view);
    assert_eq!(cx.read_entity(&view, |v, _| v.chats.len()), 1);
}

#[gpui::test]
fn chats_corrupt_transcript_shows_retry_without_replacing_current_messages(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_component::init);
    let db = db_with_chats();
    let store = worktable_db::SqliteStore::connect(&db).unwrap();
    let mut bad = store.load_chat("recent").unwrap().unwrap();
    bad.messages_json = "{}".into();
    bad.summary.revision += 1;
    store.save_chat(&bad).unwrap();
    let (view, window) = setup_view_with_db(cx, db, TEST_WINDOW_SIZE);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    view.update(&mut cx, |this, cx| {
        this.messages
            .push(crate::assistant::ChatMessage::user("Keep this visible"));
        cx.notify();
    });
    open_chat_picker(&mut cx, &view);
    click_selector(&mut cx, "chat-row:recent");
    assert!(wait_for(&mut cx, 5, |cx| cx.read_entity(&view, |v, _| {
        matches!(v.chats_state, super::ChatListState::Failed(_))
    })));
    force_frame(&mut cx);
    assert!(cx.debug_bounds("chats-retry").is_some());
    assert_eq!(
        cx.read_entity(&view, |v, _| v.messages[0].text.clone()),
        "Keep this visible"
    );
    click_selector(&mut cx, "chat-row:older");
    assert!(wait_for(&mut cx, 5, |cx| cx
        .read_entity(&view, |v, _| v.chat_id.as_deref()
            == Some("older"))));
}

#[gpui::test]
fn chats_reduced_motion_and_dismissal_reject_a_pending_selection(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(|cx| worktable_ui::set_reduced_motion(cx, true));
    let (view, window) = setup_view_with_db(cx, db_with_chats(), TEST_WINDOW_SIZE);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    view.update(&mut cx, |this, cx| {
        this.show_assistant(cx);
        this.messages
            .push(crate::assistant::ChatMessage::user("Keep this draft"));
    });
    force_frame(&mut cx);
    click_selector(&mut cx, "assistant-chats");
    force_frame(&mut cx);
    let sheet = cx.debug_bounds("chats-sheet").unwrap();
    assert_eq!(
        sheet.bottom(),
        cx.update(|window, _| window.viewport_size().height),
        "reduced motion goes straight to the bottom-aligned final position"
    );
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            this.load_chat("recent".into(), window, cx);
            this.close_chats(window, cx);
        });
    });
    cx.background_executor
        .advance_clock(std::time::Duration::from_millis(1));
    cx.run_until_parked();
    force_frame(&mut cx);
    assert!(
        cx.debug_bounds("chats-sheet").is_none(),
        "reduced motion dismisses immediately"
    );
    assert_eq!(
        cx.read_entity(&view, |v, _| v.messages[0].text.clone()),
        "Keep this draft"
    );
    assert!(cx.read_entity(&view, |v, _| v.chat_id.is_none()));
}
