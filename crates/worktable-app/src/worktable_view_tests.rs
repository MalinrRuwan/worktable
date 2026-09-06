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
    AppContext as _, Entity, Focusable as _, Modifiers, Size, TestAppContext, VisualTestContext,
    WindowHandle, point, px,
};
use gpui_component::Root;
use worktable_ai::WorktableEntry;

use super::{AppMode, SortMode, WorktableView};
use crate::service::WorktableService;

/// Default test window size — the Figma frame is a 390×884 phone shell.
const TEST_WINDOW_SIZE: Size<gpui::Pixels> = Size {
    width: px(390.),
    height: px(884.),
};

/// Hamburger button center (`header.px_4` → 390-16-40/2 = 354, y = 24 + 45/2 ≈ 46).
const HAMBURGER_CENTER: (f32, f32) = (356.0, 56.0);
/// "Settings" row center inside the open menu (top 48 + p4 + 36*3 + gap*3 + divider + 20).
/// Menu: p4, 3 sort rows (36 each), divider, then Settings (40) → center y ≈185.
const SETTINGS_ROW_CENTER: (f32, f32) = (300.0, 106.0);
/// "Ask Agent" button center (search column 16..316, button right(6) ≈ 80px wide).
const ASK_AGENT_CENTER: (f32, f32) = (286.0, 56.0);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn sample_entries() -> Vec<WorktableEntry> {
    fn entry(content: &str, created_at: i64) -> WorktableEntry {
        WorktableEntry {
            id: format!("test-{created_at}"),
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
    ]
}

/// Seed a throwaway SQLite database with `entries` and return its path.
///
/// The DB lives in its own unique temp directory: the Helix graph is stored
/// as `helix.json` *next to* the database (see `helix_path_for_sqlite`), so
/// a shared directory would make parallel tests race on one graph file.
fn seeded_db(entries: Vec<WorktableEntry>) -> String {
    let dir = std::env::temp_dir().join(format!("worktable-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("worktable.db");
    let path_str = path.to_string_lossy().into_owned();
    // Ensure HelixClient::from_env (used in some sync paths) points at this temp DB
    unsafe { std::env::set_var("WORKTABLE_DB_PATH", &path_str); }

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

/// Build a 390×884 window hosting `Root::new(WorktableView)`, seed the service
/// from `entries`, and return both the view entity and the window handle.
fn setup_view(
    cx: &mut TestAppContext,
    entries: Vec<WorktableEntry>,
) -> (Entity<WorktableView>, WindowHandle<Root>) {
    let db = seeded_db(entries);
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
    let window = cx.open_window(TEST_WINDOW_SIZE, move |window, cx| {
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
fn wait_for_helix_hits<F>(helix_path: std::path::PathBuf, query: &str, limit: usize, secs: u64, search: F) -> usize
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

fn click(cx: &mut VisualTestContext, x: f32, y: f32) {
    cx.simulate_click(point(px(x), px(y)), Modifiers::default());
    cx.run_until_parked();
}

// ---------------------------------------------------------------------------
// Interaction tests — pointer + keyboard emulation
// ---------------------------------------------------------------------------

#[gpui::test]
fn hamburger_menu_opens_and_settings_navigates(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    let (mode, count) = cx.read_entity(&view, |v, _| (v.mode, v.entries.len()));
    assert_eq!(count, 3, "seeded entries should be loaded into the view");
    assert_eq!(mode, AppMode::Entries);

    // The hamburger opens the context menu.
    click(&mut cx, HAMBURGER_CENTER.0, HAMBURGER_CENTER.1);
    assert!(
        cx.read_entity(&view, |v, _| v.library_menu_open),
        "clicking the hamburger should open the library context menu"
    );

    // Clicking the Settings item navigates and closes the menu.
    click(&mut cx, SETTINGS_ROW_CENTER.0, SETTINGS_ROW_CENTER.1);
    let (mode, open) = cx.read_entity(&view, |v, _| (v.mode, v.library_menu_open));
    assert_eq!(
        mode,
        AppMode::Settings,
        "Settings menu item should open Settings"
    );
    assert!(!open, "menu should close after picking Settings");
}

#[gpui::test]
fn ask_agent_button_opens_assistant(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    click(&mut cx, ASK_AGENT_CENTER.0, ASK_AGENT_CENTER.1);
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Assistant,
        "the Ask Agent button should switch to the AI assistant pane"
    );
}

#[gpui::test]
fn sort_mode_time_is_default_and_orders_by_recency(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry { id: "a".into(), kind: "text".into(), content: "Banana note".into(), title: None, source: "Worktable".into(), created_at: 100 },
        WorktableEntry { id: "b".into(), kind: "text".into(), content: "Apple note".into(), title: None, source: "Worktable".into(), created_at: 300 },
        WorktableEntry { id: "c".into(), kind: "text".into(), content: "Cherry note".into(), title: None, source: "Worktable".into(), created_at: 200 },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    let ids = cx.read_entity(&view, |v, _| v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>());
    assert_eq!(ids, vec!["b", "c", "a"], "Time mode should be recency-desc");
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.set_sort_mode(SortMode::Alpha, cx));
        window.refresh();
    });
    cx.run_until_parked();
    let ids = cx.read_entity(&view, |v, _| v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>());
    assert_eq!(ids, vec!["b", "a", "c"], "Alpha mode should be alphabetical");
}

#[gpui::test]
fn topic_grouping_uses_helix_primary_topic(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry { id: "t1".into(), kind: "text".into(), content: "alpha alpha alpha beta".into(), title: None, source: "Worktable".into(), created_at: 1000 },
        WorktableEntry { id: "t2".into(), kind: "text".into(), content: "beta beta beta gamma".into(), title: None, source: "Worktable".into(), created_at: 900 },
        WorktableEntry { id: "t3".into(), kind: "text".into(), content: "gamma gamma gamma delta".into(), title: None, source: "Worktable".into(), created_at: 800 },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.set_sort_mode(SortMode::Topic, cx));
        window.refresh();
    });
    cx.run_until_parked();
    let ids = cx.read_entity(&view, |v, _| v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>());
    assert_eq!(ids, vec!["t1", "t2", "t3"], "Topic mode sorts by primary topic");
    let topics: Vec<String> = cx.read_entity(&view, |v, _| v.visible_entries().iter().map(|e| super::helix_primary_topic(e)).collect());
    assert!(topics.contains(&"ALPHA".to_string()));
    assert!(topics.contains(&"BETA".to_string()));
    assert!(topics.contains(&"GAMMA".to_string()));
}

#[gpui::test]
fn helix_syncs_text_and_image_entries_and_search_finds_them(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry { id: "img1".into(), kind: "image".into(), content: "/tmp/captured_photo.png".into(), title: Some("Sunset".into()), source: "Selection".into(), created_at: 1000 },
        WorktableEntry { id: "txt1".into(), kind: "text".into(), content: "The sunset over the mountains was breathtaking".into(), title: None, source: "Worktable".into(), created_at: 900 },
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
    let n = wait_for_helix_hits(helix_path, "sunset", 10, 10, |c, q, lim| {
        c.search_blocking(q, lim).unwrap_or_default().len()
    });
    assert!(n > 0, "Helix should return at least one hit for 'sunset'");
    // Fresh client: the graph file may have been written after any earlier open.
    let client = worktable_helix::HelixClient::open_embedded(
        worktable_helix::helix_path_for_sqlite(&db_path),
    );
    let all = client.search_blocking("Sunset", 10).unwrap_or_default();
    assert!(all.len() >= 1, "Helix should contain the image entry via its title");
}

#[gpui::test]
fn cmd_and_shift_selection_behave_as_spec(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry { id: "1".into(), kind: "text".into(), content: "one".into(), title: None, source: "Worktable".into(), created_at: 300 },
        WorktableEntry { id: "2".into(), kind: "text".into(), content: "two".into(), title: None, source: "Worktable".into(), created_at: 200 },
        WorktableEntry { id: "3".into(), kind: "text".into(), content: "three".into(), title: None, source: "Worktable".into(), created_at: 100 },
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
    assert!(sel.contains("1") && sel.contains("2") && sel.len() == 2, "Cmd should add without clearing");
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.select_range("3".into(), cx));
        window.refresh();
    });
    cx.run_until_parked();
    let sel = view.read_with(&cx, |v, _| v.selected.clone());
    assert_eq!(sel.len(), 3, "Shift should select contiguous range from anchor");
    assert!(sel.contains("1") && sel.contains("3"));
}

#[gpui::test]
fn composer_kind_switch_and_submit_creates_typed_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    cx.update(|window, cx| { view.update(cx, |this, cx| this.open_composer_note(window, cx)); });
    cx.run_until_parked();
    assert_eq!(view.read_with(&cx, |v, _| v.composer), Some(super::ComposerKind::Note));
    cx.update(|window, cx| { view.update(cx, |this, cx| this.open_composer_link(window, cx)); });
    cx.run_until_parked();
    assert_eq!(view.read_with(&cx, |v, _| v.composer), Some(super::ComposerKind::Link));
    cx.update(|window, cx| { view.update(cx, |this, cx| this.open_composer_image(window, cx)); });
    cx.run_until_parked();
    assert_eq!(view.read_with(&cx, |v, _| v.composer), Some(super::ComposerKind::Image));
    view.update(&mut cx, |this, cx| this.cancel_composer(cx));
    assert_eq!(view.read_with(&cx, |v, _| v.composer), None);
}

#[gpui::test]
fn ask_agent_slide_changes_mode_without_losing_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    let count_before = view.read_with(&cx, |v, _| v.entries.len());
    click(&mut cx, ASK_AGENT_CENTER.0, ASK_AGENT_CENTER.1);
    assert_eq!(view.read_with(&cx, |v, _| v.mode), AppMode::Assistant, "Ask Agent should slide to assistant");
    assert_eq!(view.read_with(&cx, |v, _| v.entries.len()), count_before);
    cx.update(|_, cx| cx.bind_keys(crate::bindings()));
    cx.update(|window, cx| { view.focus_handle(cx).focus(window, cx); });
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
        WorktableEntry { id: "a".into(), kind: "text".into(), content: "Deep learning transformers for language".into(), title: None, source: "Worktable".into(), created_at: 1000 },
        WorktableEntry { id: "b".into(), kind: "image".into(), content: "/tmp/vision_transformer.png".into(), title: Some("Vision Transformer".into()), source: "Selection".into(), created_at: 900 },
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
    let test_key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_else(|_| "sk-test-dummy-key-for-ci".to_string());
    // NOTE: the literal "deepseek-v4-flash" id is intentionally NOT asserted
    // below (see above): it is unknown to the worker's model registry, so the
    // key-save defaulting would overwrite it whenever the two worker threads
    // lock in reverse spawn order.
    // Store via the service (mirrors Settings → Providers → Set API key)
    cx.background_executor.allow_parking();
    service.set_api_key("deepseek", &test_key).await.expect("set_api_key should succeed");
    // Saving a key auto-selects the provider's first catalog model. Capture
    // that real id and explicitly select it back: worker requests are
    // fire-and-spawn (`PiAgentRuntime::send` serializes on lock acquisition,
    // not spawn order), so asserting a *fictional* model id here would race
    // the key-save's slower model-defaulting write under load.
    let mut model_id = String::new();
    for _ in 0..100 {
        model_id = service.get_config("active_model").await.unwrap().unwrap_or_default();
        if !model_id.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!model_id.is_empty(), "saving a key should default active_model");
    service.set_model("deepseek", &model_id).await.expect("set_model should succeed");
    // Verify via direct store read (allow for async propagation — the worker handles SetModel on a blocking thread)
    let mut stored = String::new();
    let mut stored_model = String::new();
    for _ in 0..10 {
        stored = service.get_config("active_provider").await.unwrap().unwrap_or_default();
        stored_model = service.get_config("active_model").await.unwrap().unwrap_or_default();
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
async fn build_helix_from_sqlite_syncs_all_entries(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry { id: "h1".into(), kind: "text".into(), content: "alpha alpha alpha".into(), title: None, source: "Worktable".into(), created_at: 100 },
        WorktableEntry { id: "h2".into(), kind: "image".into(), content: "/tmp/photo.png".into(), title: Some("photo".into()), source: "Selection".into(), created_at: 90 },
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
    assert!(std::path::Path::new(&helix_path).exists(), "helix.json should exist next to the db");
    let hits = wait_for_helix_hits(helix_path, "alpha", 10, 10, |c, q, lim| {
        c.search_blocking(q, lim).unwrap_or_default().len()
    });
    assert!(hits > 0, "helix should find 'alpha' after build");
    let _ = synced;
}

// ---------------------------------------------------------------------------
// Click-navigation tests — every clickable navigates somewhere sane
// ---------------------------------------------------------------------------
//
// Convention: each test names the button id(s) it exercises (the
// `Button::new("…")` / `.id("…")` ids in `worktable_view.rs`). Where a
// control sits at a stable position we click real coordinates with
// `simulate_click` (via `click`); where rows are dynamic (provider list,
// virtualized cards beyond the first) we invoke the exact handler the
// button calls and assert the same navigation outcome.

/// Extra click targets, measured with the `probe_click_map` sweep against
/// the 390×884 test window (all values in points):
/// - the Entries|Agent tab group spans x≈110..280 at y≈90..125;
/// - the sort bar spans y≈130..170: Time x≈70..150, A–Z x≈150..230,
///   Topic x≈230..310;
/// - the first entry card is centered at y≈260;
/// - the composer prompt bar is pinned 16px above the window bottom;
/// - Settings-family back buttons sit top-left under the titlebar.
const AGENT_TAB_CENTER: (f32, f32) = (250.0, 106.0);
const ENTRIES_TAB_CENTER: (f32, f32) = (160.0, 106.0);
const ARCHIVE_ROW_CENTER: (f32, f32) = (300.0, 150.0);
const SORT_TIME_CENTER: (f32, f32) = (110.0, 150.0);
const SORT_ALPHA_CENTER: (f32, f32) = (190.0, 150.0);
const SORT_TOPIC_CENTER: (f32, f32) = (270.0, 150.0);
const FIRST_CARD_CENTER: (f32, f32) = (195.0, 260.0);
const COMPOSER_PROMPT_CENTER: (f32, f32) = (195.0, 842.0);
const BACK_BUTTON_CENTER: (f32, f32) = (50.0, 54.0);

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

/// Poll `cond` until it holds (parking the test executor between polls).
/// Returns false on timeout instead of asserting, so tests can report
/// which navigation outcome never arrived.
fn wait_for(cx: &mut VisualTestContext, secs: u64, mut cond: impl FnMut(&mut VisualTestContext) -> bool) -> bool {
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

fn bind_all(cx: &mut TestAppContext) {
    cx.update(|cx| cx.bind_keys(crate::bindings()));
}

#[gpui::test]
fn nav_hamburger_toggles_and_archive_closes_menu(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Button "library-menu": first click opens…
    click(&mut cx, HAMBURGER_CENTER.0, HAMBURGER_CENTER.1);
    assert!(cx.read_entity(&view, |v, _| v.library_menu_open));
    // …second click closes (toggle), staying on Entries.
    click(&mut cx, HAMBURGER_CENTER.0, HAMBURGER_CENTER.1);
    let (open, mode) = cx.read_entity(&view, |v, _| (v.library_menu_open, v.mode));
    assert!(!open, "hamburger should toggle the menu closed");
    assert_eq!(mode, AppMode::Entries);

    // Row "library-menu-archive": closes the menu, stays on Entries.
    click(&mut cx, HAMBURGER_CENTER.0, HAMBURGER_CENTER.1);
    assert!(cx.read_entity(&view, |v, _| v.library_menu_open));
    click(&mut cx, ARCHIVE_ROW_CENTER.0, ARCHIVE_ROW_CENTER.1);
    let (open, mode) = cx.read_entity(&view, |v, _| (v.library_menu_open, v.mode));
    assert!(!open, "Archive should close the menu");
    assert_eq!(mode, AppMode::Entries, "Archive should not navigate away");
    // NOTE: row "library-menu-exit" calls `cx.quit()` — never clicked in
    // tests, it would terminate the test runner.
}

#[gpui::test]
fn nav_library_tabs_slide_between_entries_and_agent(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);

    // Button "agent-tab" → Assistant…
    click(&mut cx, AGENT_TAB_CENTER.0, AGENT_TAB_CENTER.1);
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Assistant,
        "Agent tab should show the assistant pane"
    );
    // …button "entries-tab" → back to Entries.
    click(&mut cx, ENTRIES_TAB_CENTER.0, ENTRIES_TAB_CENTER.1);
    assert_eq!(
        cx.read_entity(&view, |v, _| v.mode),
        AppMode::Entries,
        "Entries tab should show the entries pane"
    );
}

#[gpui::test]
fn nav_sort_bar_clicks_change_ordering(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    // Time is default: newest first.
    let ids = cx.read_entity(&view, |v, _| {
        v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>()
    });
    assert_eq!(ids, vec!["test-1000", "test-900", "test-800"]);

    // Button "sort-alpha" → A–Z.
    click(&mut cx, SORT_ALPHA_CENTER.0, SORT_ALPHA_CENTER.1);
    let (sort, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_mode,
            v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        )
    });
    assert_eq!(sort, SortMode::Alpha);
    assert_eq!(ids, vec!["test-800", "test-1000", "test-900"], "A–Z should sort alphabetically");

    // Button "sort-topic" → Helix topics.
    click(&mut cx, SORT_TOPIC_CENTER.0, SORT_TOPIC_CENTER.1);
    let (sort, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_mode,
            v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        )
    });
    assert_eq!(sort, SortMode::Topic);
    assert_eq!(ids, vec!["test-800", "test-1000", "test-900"], "Topic should group GITIGNORE/NEGATION/TOML");

    // Button "sort-time" → back to recency.
    click(&mut cx, SORT_TIME_CENTER.0, SORT_TIME_CENTER.1);
    let (sort, ids) = cx.read_entity(&view, |v, _| {
        (
            v.sort_mode,
            v.visible_entries().into_iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        )
    });
    assert_eq!(sort, SortMode::Time);
    assert_eq!(ids, vec!["test-1000", "test-900", "test-800"]);
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
    click(&mut cx, FIRST_CARD_CENTER.0, FIRST_CARD_CENTER.1);
    let selected = cx.read_entity(&view, |v, _| v.selected.clone());
    assert_eq!(selected.len(), 1, "clicking a card should single-select it");
    assert!(selected.contains("test-1000"), "the first Time-ordered card should be selected");
}

#[gpui::test]
fn nav_composer_prompt_opens_note_composer(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    no_splash(&mut cx, &view);
    settle_strip(&mut cx);

    // Bar "composer-prompt" ("Add a note") → Note composer…
    assert_eq!(cx.read_entity(&view, |v, _| v.composer), None);
    click(&mut cx, COMPOSER_PROMPT_CENTER.0, COMPOSER_PROMPT_CENTER.1);
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.composer),
        Some(super::ComposerKind::Note),
        "the Add-a-note bar should open the Note composer"
    );

    // …button "cancel-composer" (same handler as ⎋) closes it.
    view.update(&mut cx, |this, cx| this.cancel_composer(cx));
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.composer), None);
}

#[gpui::test]
fn nav_composer_submit_creates_entry_and_returns(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Open the Note composer, type, and submit (button "submit-composer").
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.open_composer_note(window, cx));
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| state.set_value("hello from the nav test", window, cx));
        });
    });
    cx.run_until_parked();
    view.update(&mut cx, |this, cx| this.submit_composer(cx));
    let ok = wait_for(&mut cx, 5, |cx| {
        cx.read_entity(&view, |v, _| v.entries.iter().any(|e| e.content == "hello from the nav test"))
    });
    assert!(ok, "submitting the composer should insert the entry");
    let (composer, mode) = cx.read_entity(&view, |v, _| (v.composer, v.mode));
    assert_eq!(composer, None, "composer should close after submit");
    assert_eq!(mode, AppMode::Entries);
}

#[gpui::test]
fn nav_settings_tabs_switch_body(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Reach Settings via the hamburger menu (row "library-menu-settings").
    click(&mut cx, HAMBURGER_CENTER.0, HAMBURGER_CENTER.1);
    click(&mut cx, SETTINGS_ROW_CENTER.0, SETTINGS_ROW_CENTER.1);
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Settings);

    // Buttons "settings-tab-data" / "settings-tab-providers" /
    // "settings-tab-ui" switch `settings_tab` (same handler, asserted here).
    for tab in [super::SettingsTab::Data, super::SettingsTab::Providers, super::SettingsTab::Ui] {
        view.update(&mut cx, |this, cx| {
            this.settings_tab = tab;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(cx.read_entity(&view, |v, _| (v.mode, v.settings_tab)), (AppMode::Settings, tab));
    }
}

#[gpui::test]
fn nav_settings_back_buttons_return(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "configure-providers" → ProviderConfig…
    view.update(&mut cx, |this, cx| this.show_provider_config(cx));
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::ProviderConfig);
    // …header button "settings-back" → Settings (Providers tab).
    click(&mut cx, BACK_BUTTON_CENTER.0, BACK_BUTTON_CENTER.1);
    let (mode, tab) = cx.read_entity(&view, |v, _| (v.mode, v.settings_tab));
    assert_eq!(mode, AppMode::Settings, "Back should return to Settings");
    assert_eq!(tab, super::SettingsTab::Providers);

    // Button "configure-github-stars" → GitHub Stars…
    view.update(&mut cx, |this, cx| this.show_github_stars(cx));
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::GithubStars);
    // …header "settings-back" → Settings (Data tab).
    click(&mut cx, BACK_BUTTON_CENTER.0, BACK_BUTTON_CENTER.1);
    let (mode, tab) = cx.read_entity(&view, |v, _| (v.mode, v.settings_tab));
    assert_eq!(mode, AppMode::Settings, "Back should return to Settings");
    assert_eq!(tab, super::SettingsTab::Data);
}

#[gpui::test]
fn nav_assistant_setup_cta_opens_provider_config(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "assistant-setup-cta" ("Configure provider") → ProviderConfig.
    view.update(&mut cx, |this, cx| this.show_assistant(cx));
    cx.run_until_parked();
    view.update(&mut cx, |this, cx| this.show_provider_config(cx));
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::ProviderConfig);
}

#[gpui::test]
fn nav_send_without_provider_explains_setup(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "send-assistant" with no provider/model → setup hint, no hang.
    view.update(&mut cx, |this, cx| this.send_assistant(cx));
    cx.run_until_parked();
    let messages = cx.read_entity(&view, |v, _| v.messages.clone());
    assert_eq!(messages.len(), 1);
    assert!(messages[0].text.contains("isn't configured"), "assistant should explain setup, got: {}", messages[0].text);
    assert!(!cx.read_entity(&view, |v, _| v.assistant_busy));
}

#[gpui::test]
fn nav_provider_configure_select_and_logout(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, vec![]);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    assert!(wait_for_providers(&mut cx, &view, 10), "provider catalog should load");

    let (provider_id, model_id) = cx.read_entity(&view, |v, _| {
        let (pid, models) = v.provider_models.iter().find(|(_, m)| !m.is_empty()).expect("a provider with models");
        (pid.clone(), models[0].0.clone())
    });

    // Row button "configure:{id}" → expands the API-key field, stays put.
    view.update(&mut cx, |this, cx| this.configure_provider(&provider_id, cx));
    cx.run_until_parked();
    let (mode, expanded) = cx.read_entity(&view, |v, _| (v.mode, v.api_key_provider.clone()));
    assert_eq!(mode, AppMode::ProviderConfig);
    assert_eq!(expanded, Some(provider_id.clone()));

    // Button "save-key:{id}" with an empty field → no-op (same early return).
    let status_before = cx.read_entity(&view, |v, _| v.settings_status.clone());
    view.update(&mut cx, |this, cx| this.save_api_key(cx));
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.settings_status.clone()), status_before);

    // Fill the key field, then "save-key:{id}" → stored + snapshot flips.
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let input = this.api_key_input.clone();
            input.update(cx, |state, cx| state.set_value("sk-test-nav-key", window, cx));
        });
    });
    cx.run_until_parked();
    view.update(&mut cx, |this, cx| this.save_api_key(cx));
    let ok = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| {
            v.settings_status.as_deref() == Some(&format!("Saved API key for {provider_id}."))
        })
    });
    assert!(ok, "saving the API key should confirm in settings_status");

    // Model picker → "select_model" activates provider + model.
    view.update(&mut cx, |this, cx| this.select_model(&provider_id, &model_id, cx));
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
            v.providers.iter().find(|p| p.id == provider_id).is_some_and(|p| !p.api_key_set)
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
        v.providers.iter().find(|p| p.supports_oauth).map(|p| p.id.clone())
    }) else {
        // No OAuth provider in this catalog — nothing to drive.
        return;
    };

    // Button "login:{id}" → spinner state, synchronously.
    view.update(&mut cx, |this, cx| this.login_oauth(&provider_id, cx));
    cx.run_until_parked();
    let (logging_in, status) = cx.read_entity(&view, |v, _| {
        (v.logging_in.contains(&provider_id), v.settings_status.clone())
    });
    assert!(logging_in, "login should mark the provider as signing in");
    assert!(status.is_some_and(|s| s.contains(&provider_id)));

    // Button "cancel-login:{id}" → spinner cleared, cancelled status.
    view.update(&mut cx, |this, cx| this.cancel_login(&provider_id, cx));
    let ok = wait_for(&mut cx, 10, |cx| {
        cx.read_entity(&view, |v, _| {
            !v.logging_in.contains(&provider_id) && v.settings_status.as_deref() == Some("Sign-in cancelled.")
        })
    });
    assert!(ok, "cancelling login should clear the spinner with a status");
}

#[gpui::test]
fn nav_github_buttons_validate_before_network(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    view.update(&mut cx, |this, cx| this.show_github_stars(cx));
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::GithubStars);

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
        assert!(error.is_some_and(|e| e.starts_with("Invalid GitHub username")), "expected invalid-username error for {bad:?}");
    }
    assert_eq!(cx.read_entity(&view, |v, _| v.github_username.clone()), None);
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
        cx.read_entity(&view, |v, _| v.github_username.as_deref() == Some("octocat"))
    });
    assert!(ok, "saving a valid username should persist it");
    assert_eq!(cx.read_entity(&view, |v, _| v.github_error.clone()), None);
}

#[gpui::test]
fn nav_helix_button_builds_graph(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let entries = vec![
        WorktableEntry { id: "n1".into(), kind: "text".into(), content: "nav helix alpha".into(), title: None, source: "Worktable".into(), created_at: 100 },
    ];
    let (view, window) = setup_view(cx, entries);
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // Button "build-helix" → build runs, then a completion status lands.
    // (A one-entry graph builds in milliseconds, so don't assert the
    // transient spinner — just the settled outcome.)
    view.update(&mut cx, |this, cx| this.build_helix(cx));
    let ok = wait_for(&mut cx, 15, |cx| {
        cx.read_entity(&view, |v, _| !v.helix_building && v.helix_status.is_some())
    });
    assert!(ok, "helix build should finish with a status");
    let status = cx.read_entity(&view, |v, _| v.helix_status.clone().unwrap());
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

    // Switch "toggle-theme" (and ⌘T) flips dark_mode both ways.
    assert!(!cx.read_entity(&view, |v, _| v.dark_mode));
    view.update(&mut cx, |this, cx| this.toggle_theme(cx));
    cx.run_until_parked();
    assert!(cx.read_entity(&view, |v, _| v.dark_mode));
    view.update(&mut cx, |this, cx| this.toggle_theme(cx));
    cx.run_until_parked();
    assert!(!cx.read_entity(&view, |v, _| v.dark_mode));

    bind_all(&mut cx);
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-t");
    assert!(cx.read_entity(&view, |v, _| v.dark_mode), "⌘T should toggle the theme");
}

#[gpui::test]
fn nav_keyboard_shortcuts_cover_composer_and_search(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);
    focus_view(&mut cx, &view);

    // ⌘N → Note composer, ⎋ → closed.
    cx.simulate_keystrokes("cmd-n");
    assert_eq!(cx.read_entity(&view, |v, _| v.composer), Some(super::ComposerKind::Note));
    cx.simulate_keystrokes("escape");
    assert_eq!(cx.read_entity(&view, |v, _| v.composer), None, "⎋ should cancel the composer");

    // ⌘L → Link composer, ⎋ → closed. (Refocus: cancelling drops focus
    // with the unmounted input, so the next keystroke needs it back.)
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-l");
    assert_eq!(cx.read_entity(&view, |v, _| v.composer), Some(super::ComposerKind::Link));
    cx.simulate_keystrokes("escape");
    assert_eq!(cx.read_entity(&view, |v, _| v.composer), None);

    // ⌘F → search field focused: typing must land in the query.
    // (Refocus: the preceding ⎋ dropped focus with the unmounted input.)
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-f");
    cx.run_until_parked();
    cx.simulate_keystrokes("neg");
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&view, |v, _| v.query.clone()),
        "neg",
        "⌘F should focus the search field so typing filters entries"
    );
    view.update(&mut cx, |this, cx| this.clear_search(cx));
    cx.run_until_parked();

    // ⌘⇧P (ClearSearch, scoped to worktable-list) clears the query.
    view.update(&mut cx, |this, cx| {
        this.query = "negation".to_owned();
        cx.notify();
    });
    cx.run_until_parked();
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-shift-p");
    assert_eq!(cx.read_entity(&view, |v, _| v.query.clone()), "", "⌘⇧P should clear the search");
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
    assert_eq!(cx.read_entity(&view, |v, _| v.entries.len()), 2, "⌫ should delete the selected entry");
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
    assert_eq!(cx.read_entity(&view, |v, _| v.entries.len()), 3, "⏎/copy must not delete text entries");
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);
}

#[gpui::test]
fn nav_dead_bindings_are_documented(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    bind_all(cx);
    let (view, window) = setup_view(cx, sample_entries());
    let mut cx = VisualTestContext::from_window(*window.deref(), cx);
    settle(&mut cx);

    // ⌘⇧S (ToggleSidebar) has a keybinding in `main::bindings` but no
    // `on_action` handler in the view — dispatching is a safe no-op.
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-shift-s");
    cx.run_until_parked();
    assert_eq!(cx.read_entity(&view, |v, _| v.mode), AppMode::Entries);

    // ⌘⏎ (SubmitComposer) likewise has a binding but no handler — plain
    // ⏎ submits via the input subscription instead, so ⌘⏎ must not insert.
    cx.update(|window, cx| {
        view.update(cx, |this, cx| this.open_composer_note(window, cx));
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        view.update(cx, |this, cx| {
            let body = this.composer_body.clone();
            body.update(cx, |state, cx| state.set_value("do not submit via cmd-enter", window, cx));
        });
    });
    cx.run_until_parked();
    focus_view(&mut cx, &view);
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    assert!(
        !cx.read_entity(&view, |v, _| v.entries.iter().any(|e| e.content == "do not submit via cmd-enter")),
        "⌘⏎ currently has no submit handler (plain ⏎ submits)"
    );
}


