//! The main Worktable view: sidebar navigation, searchable entries list,
//! inline composer, the AI assistant pane, and the AI provider settings panel.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    AnimationExt as _, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    Pixels, Render, SharedString, Size, StatefulInteractiveElement as _, Styled, Subscription,
    Window, div, px, relative, size,
};
use gpui_component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu, PopupMenuItem};
use gpui_component::scroll::{ScrollableElement as _, Scrollbar, ScrollbarAxis};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, VirtualListScrollHandle, h_flex, v_flex,
    v_virtual_list,
};
use worktable_ai::WorktableEntry;
use worktable_events::{
    AuthNotifyKind, AuthPromptKind, ProviderInfo, ProvidersSnapshot, WorktableEvent,
};
use worktable_ui::{
    BOBBING_DOTS, TEXT_DOTS, ZERON_PULSE, fade_in, fade_quick, hover_blend, hover_fades_active,
    hover_listener, pulse_delta, splash_out,
};

use crate::assistant::{ChatMessage, Role, render_message, welcome_panel};
use crate::service::WorktableService;

#[cfg(test)]
#[path = "worktable_view_tests.rs"]
mod worktable_view_tests;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppMode {
    Entries,
    Assistant,
    Settings,
    ProviderConfig,
    GithubStars,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SettingsTab {
    Ui,
    Data,
    Providers,
}

/// Provider rows are two lines (name + badges, then controls); they grow
/// when the API-key field is expanded.
const PROVIDER_ROW_HEIGHT: f32 = 92.0;
const PROVIDER_ROW_EXPANDED_HEIGHT: f32 = 148.0;
/// At or above this window width the app uses the desktop layout (navigation
/// sidebar); below it the phone-style sliding panes are used.
const WIDE_LAYOUT_MIN_WIDTH: f32 = 720.0;
/// Reading-column cap so content stays comfortable on very wide windows.
const CONTENT_MAX_WIDTH: f32 = 720.0;

/// Corner-radius scale (macOS Tahoe / Liquid Glass): rounder, consistent
/// corners across every surface. One scale, referenced everywhere — a radius
/// change is a one-line edit per tier.
const RADIUS_CHIP: f32 = 6.0;
const RADIUS_ROW: f32 = 8.0;
const RADIUS_CONTROL: f32 = 10.0;
const RADIUS_CARD: f32 = 12.0;
const RADIUS_MENU: f32 = 14.0;

/// Fixed heights for the virtualized entries list: every card occupies the
/// same row height (content is line-clamped to fit), headers are slimmer.
const ENTRY_CARD_HEIGHT: f32 = 108.0;
const SECTION_HEADER_HEIGHT: f32 = 30.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[derive(Default)]
pub(crate) enum SortMode {
    #[default]
    Time,
    Alpha,
    Topic,
}


fn app_icon(name: IconName) -> Icon {
    Icon::new(name).size(px(16.))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ComposerKind {
    Note,
    Link,
    Image,
}

/// An interactive prompt the worker is waiting on during OAuth login.
#[derive(Clone)]
struct PendingAuthPrompt {
    prompt_id: String,
    provider_id: String,
    prompt: AuthPromptKind,
}

/// A non-interactive notice shown during OAuth login (URL / device code…).
struct AuthNotice {
    provider_id: String,
    notify: AuthNotifyKind,
}

pub struct WorktableView {
    pub(crate) service: Arc<WorktableService>,
    pub(crate) focus_handle: FocusHandle,

    pub(crate) mode: AppMode,

    // Entries — multi-select (Shift+click, Shift+Right drag)
    pub(crate) entries: Vec<WorktableEntry>,
    pub(crate) selected: HashSet<String>,
    selected_anchor: Option<String>,
    pub(crate) search_input: Entity<InputState>,
    pub(crate) query: String,

    // Composer
    pub(crate) composer: Option<ComposerKind>,
    composer_body: Entity<InputState>,
    assistant_input: Entity<InputState>,

    // Assistant
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) assistant_busy: bool,

    // Provider settings
    providers: Vec<ProviderInfo>,
    provider_models: HashMap<String, Arc<Vec<(String, String)>>>,
    active_provider: Option<String>,
    active_model: Option<String>,
    providers_loading: bool,
    /// Provider whose API key input is currently shown.
    api_key_provider: Option<String>,
    api_key_input: Entity<InputState>,
    logging_in: HashSet<String>,
    pending_prompt: Option<PendingAuthPrompt>,
    prompt_input: Entity<InputState>,
    auth_notice: Option<AuthNotice>,
    settings_status: Option<String>,

    // GitHub Stars
    github_input: Entity<InputState>,
    github_username: Option<String>,
    github_total_stars: Option<u64>,
    github_repos: Vec<crate::github::GithubRepo>,
    github_loading: bool,
    github_error: Option<String>,

    // Helix embedded graph
    helix_building: bool,
    helix_status: Option<String>,

    /// Set when a stored provider was found to be stale and auto-cleared;
    /// drives the assistant's "provider was removed — pick a new one" note.
    stale_provider_cleared: bool,

    // UI state
    pub(crate) library_menu_open: bool,
    pub(crate) sort_mode: SortMode,
    pub(crate) dark_mode: bool,
    settings_tab: SettingsTab,
    settings_scroll: VirtualListScrollHandle,
    provider_row_sizes: Rc<Vec<Size<Pixels>>>,
    entries_scroll: VirtualListScrollHandle,
    /// When the entries list last gained a new top entry — drives the
    /// "existing list glides down while the new card fades in" entrance.
    list_insert_at: Option<Instant>,
    recent_entry_id: Option<String>,
    github_importing: bool,
    github_import_status: Option<String>,
    splash_start: Option<Instant>,

    pub(crate) _subscriptions: Vec<Subscription>,
}

impl WorktableView {
    pub fn new(
        service: Arc<WorktableService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search")
                .clean_on_escape()
        });
        let composer_body = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Add a note or a prompt (development)")
        });
        let assistant_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Ask your notes…")
                .submit_on_enter(false)
        });
        let api_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste your API key…")
                .submit_on_enter(false)
        });
        let prompt_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste the code / value…")
                .submit_on_enter(false)
        });
        let github_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("GitHub username…")
                .submit_on_enter(false)
        });

        let mut view = Self {
            service,
            focus_handle,
            mode: AppMode::Entries,
            entries: Vec::new(),
            selected: HashSet::new(),
            selected_anchor: None,
            search_input,
            query: String::new(),
            composer: None,
            composer_body,
            assistant_input,
            messages: Vec::new(),
            assistant_busy: false,
            providers: Vec::new(),
            provider_models: HashMap::new(),
            active_provider: None,
            active_model: None,
            providers_loading: false,
            api_key_provider: None,
            api_key_input,
            logging_in: HashSet::new(),
            pending_prompt: None,
            prompt_input,
            auth_notice: None,
            settings_status: None,
            github_input,
            github_username: None,
            github_total_stars: None,
            github_repos: Vec::new(),
            github_loading: false,
            github_error: None,
            helix_building: false,
            stale_provider_cleared: false,
            helix_status: None,
            library_menu_open: false,
            sort_mode: SortMode::Time,
            dark_mode: false,
            settings_tab: SettingsTab::Ui,
            settings_scroll: VirtualListScrollHandle::new(),
            provider_row_sizes: Rc::new(Vec::new()),
            entries_scroll: VirtualListScrollHandle::new(),
            list_insert_at: None,
            recent_entry_id: None,
            github_importing: false,
            github_import_status: None,
            splash_start: Some(Instant::now()),

            _subscriptions: Vec::new(),
        };

        view.subscribe(window, cx);
        view.load_entries(cx);
        view.refresh_providers(cx);
        view.load_github_username(cx);
        view
    }

    fn save_ui_setting(&self, key: &str, value: &str, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        let key = key.to_owned();
        let value = value.to_owned();
        cx.spawn(async move |_, _| {
            let _ = service.set_config(&key, &value).await;
        })
        .detach();
    }

    fn subscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Route search input changes into `self.query`.
        let query_input = self.search_input.clone();
        let subscription = cx.subscribe(&self.search_input, move |this, _emitter, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = query_input.read(cx).value().to_string();
                let visible: HashSet<String> = this.visible_entry_ids().into_iter().collect();
                this.selected.retain(|id| visible.contains(id));
                if this
                    .selected_anchor
                    .as_ref()
                    .is_some_and(|id| !visible.contains(id))
                {
                    this.selected_anchor = None;
                }
                cx.notify();
            }
        });
        self._subscriptions.push(subscription);

        // Pressing Enter in the composer submits it.
        let composer_body = self.composer_body.clone();
        {
            let field = composer_body;
            let subscription = cx.subscribe(&field, move |this, _emitter, event, cx| {
                if matches!(
                    event,
                    InputEvent::PressEnter {
                        secondary: false,
                        shift: false
                    }
                ) {
                    this.submit_composer(cx);
                }
            });
            self._subscriptions.push(subscription);
        }

        // Pressing Enter in the assistant input sends the message.
        let assistant_input = self.assistant_input.clone();
        let subscription = cx.subscribe(&assistant_input, move |this, _emitter, event, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                this.send_assistant(cx);
            }
        });
        self._subscriptions.push(subscription);

        // Pressing Enter in the API-key field saves the key for its provider.
        let api_key_input = self.api_key_input.clone();
        let subscription = cx.subscribe(&api_key_input, move |this, _emitter, event, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                this.save_api_key(cx);
            }
        });
        self._subscriptions.push(subscription);

        // Pressing Enter in the login prompt field answers the pending prompt.
        let prompt_input = self.prompt_input.clone();
        let subscription = cx.subscribe(&prompt_input, move |this, _emitter, event, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                this.answer_prompt(cx);
            }
        });
        self._subscriptions.push(subscription);

        // Pressing Enter in the GitHub username field saves it and fetches stars.
        let github_input = self.github_input.clone();
        let subscription = cx.subscribe(&github_input, move |this, _emitter, event, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                this.save_github_username(cx);
            }
        });
        self._subscriptions.push(subscription);

        // Stream runtime events (AI deltas, provider snapshots, login prompts)
        // into this view.
        if let Some(mut events) = self.service.subscribe() {
            cx.spawn(async move |view, cx| {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let _ = view.update(cx, |this, cx| this.on_event(&event, cx));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
            .detach();
        }

        // Re-render whenever the window is resized. Layout-dependent styles
        // (the phone slide's pane widths, the wide/narrow sidebar switch) are
        // captured in `render()` from `viewport_size()`; without this the
        // stale styles persist through live resizes and the hidden pane peeks
        // out beside the active one.
        let subscription = cx.observe_window_bounds(window, |_, _, cx| cx.notify());
        self._subscriptions.push(subscription);

        // Focus the search field when the user opts in via Cmd+F.
        let _ = window;
    }

    fn load_entries(&mut self, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.list_entries().await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(entries) => this.entries = entries,
                    Err(error) => eprintln!("Worktable: failed to load entries: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_providers(&mut self, cx: &mut Context<Self>) {
        // Avoid stacking identical requests when navigation and a config change
        // happen before the worker has answered the previous request.
        if self.providers_loading {
            return;
        }
        self.providers_loading = true;
        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.list_providers().await;
            if let Err(error) = result {
                let _ = view.update(cx, |this, cx| {
                    this.providers_loading = false;
                    this.settings_status = Some(format!("Failed to load providers: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn load_github_username(&mut self, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        // Do not capture the github_input entity here: this detached task can
        // still be pending at App teardown (e.g. in tests), and the strong
        // entity handle kept alive past that trips GPUI's leak detection.
        cx.spawn(async move |view, cx| {
            let result = service.get_github_username().await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(Some(username)) if !username.trim().is_empty() => {
                        let username = username.trim().to_owned();
                        this.github_username = Some(username.clone());
                        let input = this.github_input.clone();
                        this.set_input(&input, &username, cx);
                        // Auto-fetch stars for the stored username (non-blocking, updates UI when done).
                        this.fetch_github_stars(cx);
                    }
                    Ok(Some(username)) => {
                        let username = username.trim().to_owned();
                        if !username.is_empty() {
                            this.github_username = Some(username.clone());
                            let input = this.github_input.clone();
                            this.set_input(&input, &username, cx);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("Worktable: failed to load github_username: {error}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn save_github_username(&mut self, cx: &mut Context<Self>) {
        let username = self.github_input.read(cx).value().to_string();
        let username = username.trim().to_owned();
        if username.is_empty() {
            self.github_error = Some("Enter a GitHub username.".to_owned());
            cx.notify();
            return;
        }
        // Basic validation mirrored from github::fetch_github_stars
        if username.contains('/') || username.contains(' ') || username.len() > 39 {
            self.github_error = Some(format!("Invalid GitHub username: '{username}'"));
            cx.notify();
            return;
        }
        let service = Arc::clone(&self.service);
        let username_clone = username.clone();
        cx.spawn(async move |view, cx| {
            let result = service.set_github_username(&username_clone).await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.github_username = Some(username_clone.clone());
                        this.github_error = None;
                        this.fetch_github_stars(cx);
                    }
                    Err(error) => {
                        this.github_error = Some(format!("Failed to save username: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Record that `id` was just added so the entries list plays its
    /// "existing rows glide down" entrance on the next renders.
    fn mark_entry_inserted(&mut self, id: &str, cx: &mut Context<Self>) {
        self.recent_entry_id = Some(id.to_owned());
        self.list_insert_at = Some(Instant::now());
        cx.notify();
    }

    pub fn show_github_stars(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::GithubStars;
        self.library_menu_open = false;
        if self.github_username.is_none() {
            self.load_github_username(cx);
        }
        cx.notify();
    }

    /// Import the starred repositories of the saved username as entries:
    /// one link entry per star, `created_at` = the star timestamp, content =
    /// the repo description (falling back to its URL). Already-imported
    /// stars are skipped.
    pub fn import_github_stars(&mut self, cx: &mut Context<Self>) {
        let Some(username) = self.github_username.clone().filter(|u| !u.is_empty()) else {
            self.github_import_status = Some("Save a GitHub username first.".to_owned());
            cx.notify();
            return;
        };
        if self.github_importing {
            return;
        }
        self.github_importing = true;
        self.github_import_status = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.import_starred_repos(&username, None).await;
            let _ = view.update(cx, |this, cx| {
                this.github_importing = false;
                match result {
                    Ok((imported, skipped)) => {
                        this.github_import_status = Some(if skipped > 0 {
                            format!("Imported {imported} stars ({skipped} already present).")
                        } else {
                            format!("Imported {imported} stars.")
                        });
                        if imported > 0 {
                            // Trigger the list entrance: the new stars land at
                            // their starred dates, so a plain refresh suffices.
                            this.load_entries(cx);
                        }
                    }
                    Err(error) => {
                        this.github_import_status =
                            Some(format!("Import failed: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }


    pub fn fetch_github_stars(&mut self, cx: &mut Context<Self>) {
        // Prefer the text currently in the input; fall back to the stored username.
        let username = {
            let input_val = self.github_input.read(cx).value().to_string();
            let trimmed = input_val.trim().to_owned();
            if !trimmed.is_empty() {
                trimmed
            } else if let Some(stored) = &self.github_username {
                stored.clone()
            } else {
                self.github_error = Some("Enter a GitHub username first.".to_owned());
                cx.notify();
                return;
            }
        };
        if self.github_loading {
            return;
        }
        self.github_loading = true;
        self.github_error = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.fetch_github_stars(&username).await;
            let _ = view.update(cx, |this, cx| {
                this.github_loading = false;
                match result {
                    Ok((total, repos)) => {
                        this.github_total_stars = Some(total);
                        this.github_repos = repos;
                        this.github_username = Some(username.clone());
                        this.github_error = None;
                    }
                    Err(error) => {
                        this.github_error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn build_helix(&mut self, cx: &mut Context<Self>) {
        if self.helix_building {
            return;
        }
        self.helix_building = true;
        self.helix_status = Some("Building Helix graph…".to_owned());
        cx.notify();
        let db_path = self.service.database_path().to_owned();
        let helix_path = worktable_helix::helix_path_for_sqlite(&db_path);
        cx.spawn(async move |view, cx| {
            // Run the blocking build on a dedicated thread. `tokio::task::spawn_blocking`
            // cannot be used here: this spawn runs on GPUI's executor, which has no Tokio
            // reactor, and calling it from there panics with "no reactor running".
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let client = worktable_helix::HelixClient::open_embedded(helix_path);
                let result = client.build_from_sqlite_blocking(&db_path);
                let _ = tx.send(result);
            });
            let result = rx.await;
            let _ = view.update(cx, |this, cx| {
                this.helix_building = false;
                match result {
                    Ok(Ok(synced)) => {
                        if synced == 0 {
                            this.helix_status =
                                Some("Helix up to date — no new entries to sync.".to_owned());
                        } else {
                            this.helix_status = Some(format!(
                                "Helix synced {synced} new entries. Topics + relations ready."
                            ));
                        }
                    }
                    Ok(Err(e)) => {
                        this.helix_status = Some(format!("Helix build failed: {e}"));
                    }
                    Err(e) => {
                        this.helix_status = Some(format!("Helix task failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn visible_entries(&self) -> Vec<&WorktableEntry> {
        let query = self.query.trim().to_lowercase();
        let mut filtered: Vec<&WorktableEntry> = self
            .entries
            .iter()
            .filter(|entry| {
                if query.is_empty() {
                    return true;
                }
                entry
                    .title
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query)
                    || entry.content.to_lowercase().contains(&query)
                    || entry.source.to_lowercase().contains(&query)
            })
            .collect();
        match self.sort_mode {
            SortMode::Time => {
                filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            }
            SortMode::Alpha => {
                filtered.sort_by(|a, b| {
                    let a_key = a
                        .title
                        .as_deref()
                        .unwrap_or(&a.content)
                        .to_lowercase();
                    let b_key = b
                        .title
                        .as_deref()
                        .unwrap_or(&b.content)
                        .to_lowercase();
                    a_key.cmp(&b_key)
                });
            }
            SortMode::Topic => {
                // Grouped view still needs a deterministic order: topics alphabetically,
                // then time within each topic. For the flat visible list used for selection,
                // sort by primary topic then time.
                filtered.sort_by(|a, b| {
                    let ta = helix_primary_topic(a);
                    let tb = helix_primary_topic(b);
                    ta.cmp(&tb).then_with(|| b.created_at.cmp(&a.created_at))
                });
            }
        }
        filtered
    }

    pub(crate) fn visible_entry_ids(&self) -> Vec<String> {
        self.visible_entries()
            .into_iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    pub(crate) fn set_sort_mode(&mut self, mode: SortMode, cx: &mut Context<Self>) {
        self.sort_mode = mode;
        cx.notify();
    }

    fn selected_entry(&self) -> Option<&WorktableEntry> {
        let anchor = self
            .selected_anchor
            .as_ref()
            .or_else(|| self.selected.iter().next())?;
        self.entries.iter().find(|entry| &entry.id == anchor)
    }

    // ---- Public action methods (called from app-level handlers) ------------

    pub fn open_composer_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(ComposerKind::Note, window, cx);
    }

    pub fn open_composer_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(ComposerKind::Link, window, cx);
    }

    pub fn open_composer_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(ComposerKind::Image, window, cx);
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search_input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn show_entries(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::Entries;
        self.library_menu_open = false;
        cx.notify();
    }

    pub fn show_assistant(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::Assistant;
        self.library_menu_open = false;
        cx.notify();
    }

    pub fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::Settings;
        self.library_menu_open = false;
        if self.providers.is_empty() {
            self.refresh_providers(cx);
        }
        cx.notify();
    }

    pub fn show_provider_config(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::ProviderConfig;
        self.library_menu_open = false;
        if self.providers.is_empty() {
            self.refresh_providers(cx);
        }
        cx.notify();
    }

    pub fn configure_provider(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
        else {
            return;
        };

        if provider.supports_api_key {
            self.api_key_provider = Some(provider_id.to_owned());
            let input = self.api_key_input.clone();
            self.set_input(&input, "", cx);
            self.settings_status = Some(format!(
                "Configuring {provider_id}: enter an API key or use OAuth below."
            ));
        } else {
            self.api_key_provider = None;
            self.settings_status = Some(format!("{provider_id} uses OAuth configuration below."));
        }
        self.update_provider_row_sizes();
        self.mode = AppMode::ProviderConfig;
        cx.notify();
    }

    pub fn toggle_library_menu(&mut self, cx: &mut Context<Self>) {
        self.library_menu_open = !self.library_menu_open;
        cx.notify();
    }

    pub fn cancel_composer(&mut self, cx: &mut Context<Self>) {
        self.composer = None;
        cx.notify();
    }

    pub fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        let mode = if self.dark_mode {
            gpui_component::ThemeMode::Dark
        } else {
            gpui_component::ThemeMode::Light
        };
        gpui_component::Theme::change(mode, None, cx);
        cx.notify();
    }

    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        let input = self.search_input.clone();
        self.set_input(&input, "", cx);
        self.query.clear();
        cx.notify();
    }

    pub fn move_selection(&mut self, direction: isize) {
        if self.mode != AppMode::Entries {
            return;
        }
        let visible = self.visible_entries();
        if visible.is_empty() {
            return;
        }
        let anchor = self
            .selected_anchor
            .as_ref()
            .or_else(|| self.selected.iter().next())
            .cloned();
        let current = anchor
            .as_ref()
            .and_then(|id| visible.iter().position(|entry| &entry.id == id));
        let next = match current {
            Some(index) => {
                ((index as isize + direction).rem_euclid(visible.len() as isize)) as usize
            }
            None => 0,
        };
        let id = visible[next].id.clone();
        self.selected.clear();
        self.selected.insert(id.clone());
        self.selected_anchor = Some(id);
    }

    pub fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        let ids: Vec<String> = self.selected.iter().cloned().collect();
        self.entries.retain(|entry| !ids.contains(&entry.id));
        self.selected.clear();
        self.selected_anchor = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        cx.spawn(async move |_view, _cx| {
            for id in ids {
                let _ = service.delete_entry(&id).await;
            }
        })
        .detach();
    }

    pub fn copy_selected(&mut self, cx: &mut Context<Self>) {
        if self.selected.len() > 1 {
            self.copy_entries_as_list(cx);
            return;
        }
        if let Some(entry) = self.selected_entry() {
            let text = entry
                .title
                .as_deref()
                .map(|title| format!("{title}\n{}", entry.content))
                .unwrap_or_else(|| entry.content.clone());
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    pub fn copy_selected_link(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.selected_entry()
            && entry.kind == "link" {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(entry.content.clone()));
            }
    }

    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.selected_entry()
            && entry.kind == "link" {
                cx.open_url(&entry.content);
            }
    }

    pub fn select_at(&mut self, id: String, extend: bool) {
        if extend {
            if self.selected.contains(&id) {
                self.selected.remove(&id);
                if self.selected_anchor.as_ref() == Some(&id) {
                    self.selected_anchor = self.selected.iter().next().cloned();
                }
            } else {
                self.selected.insert(id.clone());
                // Keep existing anchor for shift-contiguous (cmd should not move anchor)
                if self.selected_anchor.is_none() {
                    self.selected_anchor = Some(id);
                }
            }
        } else {
            self.selected.clear();
            self.selected.insert(id.clone());
            self.selected_anchor = Some(id);
        }
    }

    pub fn select_range(&mut self, id: String, _cx: &mut Context<Self>) {
        let visible_ids = self.visible_entry_ids();
        let anchor = self
            .selected_anchor
            .clone()
            .unwrap_or_else(|| id.clone());
        let anchor_idx = visible_ids.iter().position(|x| x == &anchor);
        let target_idx = visible_ids.iter().position(|x| x == &id);
        if let (Some(a), Some(b)) = (anchor_idx, target_idx) {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            self.selected.clear();
            for idx in start..=end {
                self.selected.insert(visible_ids[idx].clone());
            }
            // Keep the original anchor for subsequent shifts; update UI anchor to target
            self.selected_anchor = Some(anchor.clone());
            // Also ensure the target is considered last selected for next cmd ops
            // but keep anchor stable — do not overwrite anchor with id if we want sticky.
        } else {
            self.select_at(id, false);
        }
    }

    pub fn on_event(&mut self, event: &WorktableEvent, cx: &mut Context<Self>) {
        match event {
            WorktableEvent::AiRunStarted { .. } => {
                self.assistant_busy = true;
                cx.notify();
            }
            WorktableEvent::AiThoughtDelta { delta, .. } => {
                match self.messages.last_mut().filter(|m| m.streaming) {
                    Some(message) => message.thinking.push_str(delta),
                    None => self.messages.push(ChatMessage {
                        role: Role::Assistant,
                        text: String::new(),
                        thinking: delta.clone(),
                        streaming: true,
                    }),
                }
                cx.notify();
            }
            WorktableEvent::AiMessageDelta { delta, .. } => {
                match self.messages.last_mut().filter(|m| m.streaming) {
                    Some(message) => message.text.push_str(delta),
                    None => self.messages.push(ChatMessage {
                        role: Role::Assistant,
                        text: delta.clone(),
                        thinking: String::new(),
                        streaming: true,
                    }),
                }
                cx.notify();
            }
            WorktableEvent::AiToolStarted { name, .. } => {
                if !self
                    .messages
                    .iter()
                    .any(|m| m.text.contains("using a tool"))
                {
                    self.messages.push(ChatMessage {
                        role: Role::Assistant,
                        text: format!("> using **{name}**…"),
                        thinking: String::new(),
                        streaming: true,
                    });
                }
                cx.notify();
            }
            WorktableEvent::AiRunFinished { .. } => {
                self.assistant_busy = false;
                for message in self.messages.iter_mut().rev() {
                    if message.streaming {
                        message.streaming = false;
                        break;
                    }
                }
                cx.notify();
            }
            WorktableEvent::AiRunFailed { error, .. } | WorktableEvent::AiWorkerError { error } => {
                self.providers_loading = false;
                self.settings_status = Some(error.clone());
                self.assistant_busy = false;
                for message in self.messages.iter_mut().rev() {
                    if message.streaming {
                        message.streaming = false;
                        break;
                    }
                }
                if !self.messages.iter().any(|m| m.text.contains(error)) {
                    self.messages
                        .push(ChatMessage::assistant(format!("⚠ {error}")));
                }
                cx.notify();
            }
            WorktableEvent::AiProvidersSnapshot { snapshot } => {
                self.apply_snapshot(snapshot);
                cx.notify();
            }
            WorktableEvent::AiConfigChanged {
                active_provider,
                active_model,
            } => {
                if active_provider.is_some() {
                    self.active_provider = active_provider.clone();
                }
                if active_model.is_some() {
                    self.active_model = active_model.clone();
                }
                cx.notify();
            }
            WorktableEvent::AiAuthPrompt {
                prompt_id,
                provider_id,
                prompt,
            } => {
                self.pending_prompt = Some(PendingAuthPrompt {
                    prompt_id: prompt_id.clone(),
                    provider_id: provider_id.clone(),
                    prompt: prompt.clone(),
                });
                let input = self.prompt_input.clone();
                self.set_input(&input, "", cx);
                cx.notify();
            }
            WorktableEvent::AiAuthNotify {
                provider_id,
                notify,
            } => {
                self.auth_notice = Some(AuthNotice {
                    provider_id: provider_id.clone(),
                    notify: notify.clone(),
                });
                cx.notify();
            }
            WorktableEvent::AiLoginResult {
                provider_id,
                ok,
                error,
            } => {
                self.logging_in.remove(provider_id);
                self.pending_prompt = None;
                if *ok {
                    self.settings_status = Some(format!("Signed in to {provider_id}."));
                } else {
                    self.settings_status = Some(format!(
                        "Sign-in failed for {provider_id}: {}",
                        error.as_deref().unwrap_or("unknown error")
                    ));
                }
                self.refresh_providers(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn apply_snapshot(&mut self, snapshot: &ProvidersSnapshot) {
        self.providers_loading = false;
        self.providers = snapshot.providers.clone();
        self.provider_models = snapshot
            .providers
            .iter()
            .map(|provider| {
                let models = provider
                    .models
                    .iter()
                    .map(|model| (model.id.clone(), model.name.clone()))
                    .collect();
                (provider.id.clone(), Arc::new(models))
            })
            .collect();
        self.active_provider = snapshot.active_provider.clone();
        self.active_model = snapshot.active_model.clone();
        // Drop any api-key entry row that no longer exists.
        if let Some(provider) = &self.api_key_provider
            && !self.providers.iter().any(|p| &p.id == provider) {
            self.api_key_provider = None;
        }
        // A stored active provider the catalog no longer knows (e.g. saved by
        // an older build, then the provider was removed upstream) can never
        // serve a prompt. Clear it and say so — the assistant shows its setup
        // CTA until a valid provider is chosen again.
        if let Some(provider) = self.active_provider.clone()
            && !provider.is_empty()
            && !self.providers.iter().any(|p| p.id == provider) {
            self.active_provider = None;
            self.active_model = None;
            self.stale_provider_cleared = true;
            self.settings_status = Some(format!(
                "'{provider}' is no longer an available provider — pick a new one under Providers."
            ));
        }
        self.update_provider_row_sizes();
    }

    /// Rebuild the virtual list's row sizes: every provider row uses the
    /// compact height except the one whose API-key field is expanded.
    fn update_provider_row_sizes(&mut self) {
        self.provider_row_sizes = Rc::new(
            self.providers
                .iter()
                .map(|provider| {
                    let height = if self.api_key_provider.as_deref() == Some(provider.id.as_str()) {
                        PROVIDER_ROW_EXPANDED_HEIGHT
                    } else {
                        PROVIDER_ROW_HEIGHT
                    };
                    size(px(0.), px(height))
                })
                .collect(),
        );
    }

    /// The agent can serve prompts only when the worker runs AND a provider
    /// from the current catalog is active with a model selected. Anything
    /// less and the composer turns into a setup call-to-action instead of a
    /// dead input.
    fn agent_ready(&self) -> bool {
        self.service.has_ai_worker()
            && self
                .active_provider
                .as_deref()
                .is_some_and(|id| !id.is_empty() && self.providers.iter().any(|p| p.id == id))
            && self
                .active_model
                .as_deref()
                .is_some_and(|model| !model.is_empty())
    }

    pub fn send_assistant(&mut self, cx: &mut Context<Self>) {
        if !self.agent_ready() {
            self.messages.push(ChatMessage::assistant(
                "The AI assistant isn't configured yet — pick a provider, add an API key, and choose a model in Settings.",
            ));
            cx.notify();
            return;
        }
        if self.assistant_busy {
            return;
        }
        let text = self.assistant_input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let text = text.trim().to_owned();
        self.messages.push(ChatMessage::user(text.clone()));
        self.assistant_busy = true;
        let input = self.assistant_input.clone();
        self.set_input(&input, "", cx);
        cx.notify();

        let service = Arc::clone(&self.service);
        let request_id = crate::service::new_entry_id();
        cx.spawn(async move |view, cx| {
            let result = service
                .submit_prompt(&request_id, "wt-session", &text)
                .await;
            match result {
                Err(error) => {
                    let _ = view.update(cx, |this, cx| {
                        this.assistant_busy = false;
                        if !this.messages.iter().any(|m| m.text.contains(&error)) {
                            this.messages
                                .push(ChatMessage::assistant(format!("⚠ {error}")));
                        }
                        cx.notify();
                    });
                }
                // `None` = the session lease was not granted (another run is
                // still active). Without this arm the composer stays busy
                // forever with no feedback — the prompt silently vanishes.
                Ok(None) => {
                    let _ = view.update(cx, |this, cx| {
                        this.assistant_busy = false;
                        this.messages.push(ChatMessage::assistant(
                            "⚠ Another prompt is still running for this session. Please wait for it to finish and try again.",
                        ));
                        cx.notify();
                    });
                }
                Ok(Some(_)) => {}
            }
        })
        .detach();
    }

    // ---- Provider settings actions -------------------------------------------

    pub fn save_api_key(&mut self, cx: &mut Context<Self>) {
        let Some(provider_id) = self.api_key_provider.clone() else {
            return;
        };
        let api_key = self.api_key_input.read(cx).value().to_string();
        let api_key = api_key.trim().to_owned();
        if api_key.is_empty() {
            return;
        }

        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.set_api_key(&provider_id, &api_key).await;
            let _ = view.update(cx, |this, cx| {
                let input = this.api_key_input.clone();
                this.set_input(&input, "", cx);
                match result {
                    Ok(()) => {
                        this.settings_status = Some(format!("Saved API key for {provider_id}."));
                    }
                    Err(error) => {
                        this.settings_status = Some(format!("Failed to save API key: {error}"));
                    }
                }
                this.refresh_providers(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn select_model(&mut self, provider_id: &str, model_id: &str, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        let provider_id = provider_id.to_owned();
        let model_id = model_id.to_owned();
        cx.spawn(async move |view, cx| {
            let result = service.set_model(&provider_id, &model_id).await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.active_provider = Some(provider_id.clone());
                        this.active_model = Some(model_id.clone());
                        this.settings_status = Some(format!("Using {provider_id} / {model_id}."));
                    }
                    Err(error) => {
                        this.settings_status = Some(format!("Failed to select model: {error}"));
                    }
                }
                this.refresh_providers(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn login_oauth(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        self.logging_in.insert(provider_id.to_owned());
        self.settings_status = Some(format!("Starting sign-in for {provider_id}…"));
        self.auth_notice = None;
        self.pending_prompt = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        let provider_id = provider_id.to_owned();
        cx.spawn(async move |view, cx| {
            let result = service.login_oauth(&provider_id).await;
            if let Err(error) = result {
                let _ = view.update(cx, |this, cx| {
                    this.logging_in.remove(&provider_id);
                    this.settings_status = Some(format!("Failed to start sign-in: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn cancel_login(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        let provider_id = provider_id.to_owned();
        cx.spawn(async move |view, cx| {
            let _ = service.cancel_login(&provider_id).await;
            let _ = view.update(cx, |this, cx| {
                this.logging_in.remove(&provider_id);
                this.pending_prompt = None;
                this.settings_status = Some("Sign-in cancelled.".to_owned());
                cx.notify();
            });
        })
        .detach();
    }

    pub fn logout_provider(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        let provider_id = provider_id.to_owned();
        cx.spawn(async move |view, cx| {
            let result = service.logout_provider(&provider_id).await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.settings_status = Some(format!("Signed out of {provider_id}."));
                    }
                    Err(error) => {
                        this.settings_status = Some(format!("Failed to sign out: {error}"));
                    }
                }
                this.refresh_providers(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn answer_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_prompt.clone() else {
            return;
        };
        let answer = self.prompt_input.read(cx).value().to_string();
        if answer.trim().is_empty() {
            return;
        }
        self.submit_prompt_answer(&pending.prompt_id, answer.trim().to_owned(), cx);
    }

    pub fn answer_prompt_option(&mut self, option_id: &str, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_prompt.clone() else {
            return;
        };
        self.submit_prompt_answer(&pending.prompt_id, option_id.to_owned(), cx);
    }

    /// Open an OAuth URL in the user's default browser.
    pub fn open_auth_url(&mut self, url: &str, cx: &mut Context<Self>) {
        cx.open_url(url);
    }

    fn submit_prompt_answer(&mut self, prompt_id: &str, answer: String, cx: &mut Context<Self>) {
        self.pending_prompt = None;
        let input = self.prompt_input.clone();
        self.set_input(&input, "", cx);
        cx.notify();

        let service = Arc::clone(&self.service);
        let prompt_id = prompt_id.to_owned();
        cx.spawn(async move |view, cx| {
            let result = service.answer_auth_prompt(&prompt_id, &answer).await;
            if let Err(error) = result {
                let _ = view.update(cx, |this, cx| {
                    this.settings_status = Some(format!("Failed to answer prompt: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.composer else {
            return;
        };
        let body = trim_opt(self.composer_body.read(cx).value().as_ref());

        let (valid, content, entry_kind) = match kind {
            ComposerKind::Note => match body {
                Some(content) => (true, content, "text"),
                None => (false, String::new(), "text"),
            },
            ComposerKind::Link => match body {
                Some(url) => (true, url, "link"),
                None => (false, String::new(), "link"),
            },
            ComposerKind::Image => match body {
                Some(path) => (true, path, "image"),
                None => (false, String::new(), "image"),
            },
        };
        if !valid {
            return;
        }

        let entry = WorktableEntry {
            id: crate::service::new_entry_id(),
            kind: entry_kind.to_owned(),
            content,
            title: None,
            source: "Worktable".to_owned(),
            created_at: crate::service::unix_time_ms(),
        };

        self.composer = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        let entry_id_for_anim = entry.id.clone();
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.selected.clear();
                    this.selected.insert(entry.id.clone());
                    this.selected_anchor = Some(entry.id);
                    this.mode = AppMode::Entries;
                    this.mark_entry_inserted(&entry_id_for_anim, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_composer(&mut self, kind: ComposerKind, window: &mut Window, cx: &mut Context<Self>) {
        self.composer = Some(kind);
        self.composer_body.update(cx, |state, cx| {
            state.set_value("", window, cx);
            match kind {
                ComposerKind::Note => state.set_placeholder("Write your note…", window, cx),
                ComposerKind::Link => {
                    state.set_placeholder("https://…  (or a plain link)", window, cx)
                }
                ComposerKind::Image => state.set_placeholder("Image path or URL…", window, cx),
            }
        });
        cx.notify();

        // Focus the first field on the next frame so the composer is mounted.
        let view = cx.entity();
        window.defer(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                let handle = this.composer_body.read(cx).focus_handle(cx);
                handle.focus(window, cx);
            });
        });
    }

    /// Insert text captured from another application without opening the window.
    pub fn add_captured_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(content) = trim_opt(&text) else {
            return;
        };

        let entry = WorktableEntry {
            id: crate::service::new_entry_id(),
            kind: "text".to_owned(),
            content,
            title: None,
            source: "Selection".to_owned(),
            created_at: crate::service::unix_time_ms(),
        };
        let service = Arc::clone(&self.service);
        let entry_id_for_anim = entry.id.clone();
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.selected.clear();
                    this.selected.insert(entry.id.clone());
                    this.selected_anchor = Some(entry.id);
                    this.mode = AppMode::Entries;
                    this.mark_entry_inserted(&entry_id_for_anim, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Insert an image captured from another application.
    pub fn add_captured_image(&mut self, path: String, mime_type: String, cx: &mut Context<Self>) {
        let path = path.trim().to_owned();
        if path.is_empty() {
            return;
        }
        // `mime_type` is kept for display purposes but the DB stores the file path as content.
        let _ = mime_type;
        let entry = WorktableEntry {
            id: crate::service::new_entry_id(),
            kind: "image".to_owned(),
            content: path,
            title: None,
            source: "Selection".to_owned(),
            created_at: crate::service::unix_time_ms(),
        };
        let service = Arc::clone(&self.service);
        let entry_id_for_anim = entry.id.clone();
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.selected.clear();
                    this.selected.insert(entry.id.clone());
                    this.selected_anchor = Some(entry.id);
                    this.mode = AppMode::Entries;
                    this.mark_entry_inserted(&entry_id_for_anim, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy selected entries (when multi-selected) else all visible entries as a joined list.
    pub fn copy_entries_as_list(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<&WorktableEntry> = if self.selected.len() > 1 {
            let sel = &self.selected;
            self.visible_entries()
                .into_iter()
                .filter(|e| sel.contains(&e.id))
                .collect()
        } else {
            self.visible_entries()
        };
        if entries.is_empty() {
            return;
        }
        let text = entries
            .iter()
            .map(|entry| {
                if let Some(title) = &entry.title {
                    format!("{title}\n{}", entry.content)
                } else {
                    entry.content.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Clear/set an input's value via the window handle (inputs need a window).
    fn set_input(&mut self, input: &Entity<InputState>, value: &str, cx: &mut Context<Self>) {
        let input = input.clone();
        let value = value.to_owned();
        let Some(window) = cx.active_window() else {
            return;
        };
        let Some(window) = window.downcast::<gpui_component::Root>() else {
            return;
        };
        let _ = window.update(cx, |_root, window, cx| {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        });
    }
}

impl Focusable for WorktableView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorktableView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if hover_fades_active() {
            window.refresh();
        }
        if let Some(start) = self.splash_start
            && start.elapsed() > Duration::from_millis(650) {
                self.splash_start = None;
            }
        let theme = cx.theme().clone();
        let is_splash = self.splash_start.is_some();

        div()
            .id("worktable-root")
            .size_full()
            .flex()
            .relative()
            .track_focus(&self.focus_handle)
            .bg(theme.tokens.background)
            .text_color(theme.foreground)
            .text_size(theme.font_size)
            .key_context("worktable-list")
            .on_action(
                cx.listener(|this, _: &crate::actions::SelectPrevious, _, _| {
                    this.move_selection(-1)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::SelectNext, _, _| this.move_selection(1)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::DeleteEntry, _, cx| {
                    this.delete_selected(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenEntry, _, cx| this.open_selected(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CopyEntry, _, cx| this.copy_selected(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CopyLink, _, cx| {
                    this.copy_selected_link(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::NewNote, window, cx| {
                    this.open_composer_note(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::NewLink, window, cx| {
                    this.open_composer_link(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::FocusSearch, window, cx| {
                    this.focus_search(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ShowEntries, _, cx| this.show_entries(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ShowAssistant, _, cx| {
                    this.show_assistant(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ShowSettings, _, cx| this.show_settings(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ToggleTheme, _, cx| this.toggle_theme(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ClearSearch, _, cx| this.clear_search(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CancelComposer, _, cx| {
                    this.cancel_composer(cx)
                }),
            )
            .child(
                h_flex()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    // The transparent macOS titlebar lets the app background
                    // paint through it; leave room for the traffic lights.
                    .pt(px(30.))
                    // Cap the reading column on very wide windows so content
                    // stays comfortable; centered.
                    .justify_center()
                    .child(render_main(self, window, cx)),
            )
            .when(is_splash, |this| {
                this.child(splash_out(
                    "worktable-splash",
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(theme.tokens.background)
                        .child(
                            v_flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    Icon::new(IconName::GalleryVerticalEnd)
                                        .size(px(32.))
                                        .text_color(theme.primary),
                                )
                                .child(
                                    div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child("Worktable"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child("Your notes — pure Rust"),
                                ),
                        ),
                ))
            })
    }
}

fn render_main(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
    // One column, three shells:
    // - Entries/Assistant: header (search) + FIXED Entries|Agent button group,
    //   with only the page content sliding between them — the button group
    //   never moves.
    // - Settings: back button + tab group on top, no search bar.
    // - ProviderConfig / GithubStars: full-page replacements with a back
    //   button, reached from Settings.
    let theme = cx.theme().clone();
    let viewport_width = window.viewport_size().width;
    // Panes never exceed the content column, or the hidden pane would peek
    // out beside the active one.
    let pane_w = viewport_width.min(px(CONTENT_MAX_WIDTH));

    let header: gpui::AnyElement = match this.mode {
        AppMode::Entries | AppMode::Assistant => render_library_header(this, cx),
        AppMode::Settings => settings_header(this, "Settings", SettingsTab::Ui, cx),
        AppMode::ProviderConfig => settings_header(this, "AI providers", SettingsTab::Providers, cx),
        AppMode::GithubStars => settings_header(this, "GitHub Stars", SettingsTab::Data, cx),
    };

    let body: gpui::AnyElement = match this.mode {
        AppMode::Entries | AppMode::Assistant => {
            let is_assistant = this.mode == AppMode::Assistant;
            // Build both panes sequentially (separate mutable borrows).
            let entries_pane = render_entries_pane(this, window, cx).into_any_element();
            let assistant_pane = render_assistant(this, cx).into_any_element();
            v_flex()
                .id("slide-shell")
                .flex_1()
                .min_h_0()
                .w_full()
                // The button group is fixed; only this block slides.
                .child(library_tabs(this, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w(pane_w)
                        .max_w_full()
                        .overflow_hidden()
                        .relative()
                        .child(
                            h_flex()
                                .w(pane_w * 2.)
                                .h_full()
                                .relative()
                                .with_animation(
                                    if is_assistant {
                                        "slide-to-assistant"
                                    } else {
                                        "slide-to-entries"
                                    },
                                    worktable_ui::RESIZE.animation(),
                                    move |el, t| {
                                        let (from, to) = if is_assistant {
                                            (px(0.), -pane_w)
                                        } else {
                                            (-pane_w, px(0.))
                                        };
                                        el.left(from + (to - from) * t)
                                    },
                                )
                                .child(
                                    div()
                                        .w(pane_w)
                                        .h_full()
                                        .flex()
                                        .flex_col()
                                        .child(entries_pane),
                                )
                                .child(
                                    div()
                                        .w(pane_w)
                                        .h_full()
                                        .flex()
                                        .flex_col()
                                        .child(assistant_pane),
                                ),
                        ),
                )
                .into_any_element()
        }
        AppMode::Settings => render_settings(this, window, cx).into_any_element(),
        AppMode::ProviderConfig => render_provider_config(this, window, cx).into_any_element(),
        AppMode::GithubStars => render_github_stars_page(this, window, cx).into_any_element(),
    };

    v_flex()
        .relative()
        .flex_1()
        .min_w_0()
        .size_full()
        .max_w(px(CONTENT_MAX_WIDTH))
        .bg(theme.tokens.background)
        .child(header)
        .child(body)
        .when(this.library_menu_open, |this| {
            this.child(library_context_menu(cx))
        })
}

fn render_library_header(
    this: &mut WorktableView,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();

    // Search field + Ask Agent + hamburger as real flex siblings — the old
    // absolutely-positioned button overlapped the field's text.
    h_flex()
        .id("library-header")
        .w_full()
        .items_center()
        .gap_2()
        .px_4()
        .pt_2()
        .pb_2()
        .bg(theme.tokens.background)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(36.))
                .child(
                    Input::new(&this.search_input)
                        .h(px(36.))
                        .w_full()
                        .appearance(true)
                        .bordered(false)
                        .focus_bordered(false)
                        .bg(theme.muted)
                        .text_color(theme.foreground)
                        .prefix(
                            Icon::new(IconName::Search)
                                .size(px(18.))
                                .text_color(theme.muted_foreground),
                        ),
                ),
        )
        .child(
            Button::new("ask-agent")
                .label("Ask Agent")
                .primary()
                .shadow_sm()
                .h(px(32.))
                .text_xs()
                .on_click(cx.listener(|this, _, _, cx| this.show_assistant(cx))),
        )
        .child(
            div()
                .size(px(36.))
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .child(library_menu_button(this, cx)),
        )
        .into_any_element()
}

/// Shared header for the Settings-family pages: a back button at the left
/// corner (returns to Settings on the given tab) and — on the Settings page
/// itself — the UI/Data/Providers tab group.
fn settings_header(
    this: &mut WorktableView,
    title: &str,
    back_to: SettingsTab,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let is_settings = this.mode == AppMode::Settings;

    h_flex()
        .id("settings-header")
        .w_full()
        .items_center()
        .gap_3()
        .px_4()
        .pt_2()
        .pb_2()
        .bg(theme.tokens.background)
        .child(
            Button::new("settings-back")
                .icon(app_icon(IconName::ArrowLeft))
                .label("Back")
                .ghost()
                .h(px(32.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_tab = back_to;
                    this.show_settings(cx);
                })),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(div().flex_1())
        .when(is_settings, |el| {
            el.child(settings_tab_group(this, cx))
        })
        .into_any_element()
}

/// The UI / Data / Providers segmented control.
fn settings_tab_group(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let tabs = [
        ("settings-tab-ui", "UI", SettingsTab::Ui),
        ("settings-tab-data", "Data", SettingsTab::Data),
        ("settings-tab-providers", "Providers", SettingsTab::Providers),
    ];
    let mut group = h_flex()
        .gap_1()
        .p_1()
        .bg(theme.tokens.background)
        .rounded(px(RADIUS_CONTROL))
        .border_1()
        .border_color(theme.border.opacity(0.6));
    for (id, label, tab) in tabs {
        let active = this.settings_tab == tab;
        let mut btn = Button::new(id)
            .label(label)
            .h(px(26.))
            .text_xs()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings_tab = tab;
                cx.notify();
            }));
        btn = if active { btn.primary() } else { btn.ghost() };
        group = group.child(btn);
    }
    group.into_any_element()
}

fn library_menu_button(this: &WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();

    Button::new("library-menu")
        .icon(app_icon(IconName::Menu))
        .ghost()
        .size(px(40.))
        .rounded(px(59.))
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .selected(this.library_menu_open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_library_menu(cx)))
}

fn library_context_menu(cx: &mut Context<WorktableView>) -> gpui::AnyElement {
    let theme = cx.theme().clone();

    fn menu_row(
        id: &'static str,
        label: &'static str,
        icon: IconName,
        tall: bool,
        cx: &mut Context<WorktableView>,
        on_click: impl Fn(&mut WorktableView, &mut Window, &mut Context<WorktableView>) + 'static,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        h_flex()
            .id(id)
            .w_full()
            .h(px(if tall { 40.0 } else { 36.0 }))
            .items_center()
            .gap_3()
            .px_4()
            .rounded(px(RADIUS_ROW))
            .cursor_pointer()
            .hover(|this| this.bg(theme.muted))
            .on_click(cx.listener(move |this, _event, window, cx| on_click(this, window, cx)))
            .child(
                Icon::new(icon)
                    .size(px(16.))
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(label.to_owned()),
            )
            .into_any_element()
    }

    v_flex()
        .id("library-context-menu")
        .absolute()
        .top(px(52.))
        .right(px(8.))
        .w(px(220.))
        .p(px(4.))
        .gap_1()
        .rounded(px(RADIUS_MENU))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .child(menu_row(
            "library-menu-settings",
            "Settings",
            IconName::Settings,
            true,
            cx,
            |this, _window, cx| this.show_settings(cx),
        ))
        .child(menu_row(
            "library-menu-archive",
            "Archive",
            IconName::FolderOpen,
            true,
            cx,
            |this, _window, cx| {
                this.library_menu_open = false;
                cx.notify();
            },
        ))
        .child(div().mx(px(4.)).h(px(1.)).bg(theme.border))
        .child(menu_row(
            "library-menu-exit",
            "Exit Worktable",
            IconName::Close,
            true,
            cx,
            |_this, _window, cx| cx.quit(),
        ))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Settings panel
// ---------------------------------------------------------------------------

fn render_settings(
    this: &mut WorktableView,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let view = cx.entity();

    if this.providers.is_empty() {
        let message = this
            .settings_status
            .clone()
            .unwrap_or_else(|| "Loading providers…".to_owned());
        let loader = if this.providers_loading {
            let phase = pulse_delta(&ZERON_PULSE, cx.entity_id(), cx);
            let opacity = worktable_ui::pulse_opacity(phase);
            div()
                .size(px(12.))
                .rounded_full()
                .bg(theme.primary)
                .opacity(opacity)
                .into_any_element()
        } else {
            div().into_any_element()
        };
        return v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(loader)
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(message),
            )
            .child(
                Button::new("retry-providers")
                    .label("Retry")
                    .ghost()
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.refresh_providers(cx));
                    }),
            )
            .into_any_element();
    }

    let active = this
        .active_provider
        .as_deref()
        .map(|provider| {
            format!(
                "Active provider: {}{}",
                provider,
                this.active_model
                    .as_deref()
                    .map(|model| format!(" / {model}"))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_else(|| "No AI provider is active yet.".to_owned());

    let configure = Button::new("configure-providers")
        .label("Configure providers")
        .icon(app_icon(IconName::Settings))
        .primary()
        .on_click(cx.listener(|this, _, _, cx| this.show_provider_config(cx)));

    // The tab group lives in the settings header (`settings_tab_group`); the
    // body only switches on `this.settings_tab`.
    let _ = &theme;

    let _tab_bar = ();
    let ui_section = v_flex().gap_4().p_4().child(
        v_flex()
            .gap_2()
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("UI"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Appearance"),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().child("Dark theme"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Ayu Light / Ayu Dark"),
                            ),
                    )
                    .child(
                        Switch::new("toggle-theme")
                            .checked(this.dark_mode)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                if *checked != this.dark_mode {
                                    this.toggle_theme(cx);
                                }
                            })),
                    ),
            ),
    );

    // Data tab: one row per data provider, with a Configure button that opens
    // the provider's full-page setup.
    let data_section = v_flex()
        .gap_4()
        .p_4()
        .child(
            v_flex()
                .gap_2()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Data"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Import data sources as entries"),
                ),
        )
        .child(
            h_flex()
                .id("data-provider-github")
                .w_full()
                .items_center()
                .gap_3()
                .p_3()
                .rounded(px(RADIUS_CARD))
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_2xs()
                .child(
                    Icon::new(IconName::Star)
                        .size(px(18.))
                        .text_color(theme.primary),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child("GitHub Stars"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Starred repositories, with the date you starred them"),
                        ),
                )
                .child(
                    Button::new("configure-github-stars")
                        .label("Configure")
                        .ghost()
                        .h(px(28.))
                        .on_click(cx.listener(|this, _, _, cx| this.show_github_stars(cx))),
                ),
        );


    let providers_section = v_flex().gap_4().items_center().child(
        v_flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_2xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("AI providers"),
            )
            .child(
                div()
                    .max_w(px(480.))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        "Choose a provider, model, and authentication method for the assistant.",
                    ),
            )
            .child(div().text_sm().text_color(theme.primary).child(active))
            .child(configure),
    );

    let content = match this.settings_tab {
        SettingsTab::Ui => ui_section.into_any_element(),
        SettingsTab::Data => data_section.into_any_element(),
        SettingsTab::Providers => providers_section.into_any_element(),
    };

    let _ = _tab_bar;
    v_flex()
        .flex_1()
        .overflow_y_scrollbar()
        .p_4()
        .gap_4()
        .child(content)
        .into_any_element()
}

fn render_provider_config(
    this: &mut WorktableView,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
    let theme = cx.theme().clone();

    if this.providers.is_empty() {
        let message = this
            .settings_status
            .clone()
            .unwrap_or_else(|| "Loading providers…".to_owned());
        let view = cx.entity();
        return v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .max_w(px(520.))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(message),
            )
            .child(
                Button::new("retry-providers")
                    .label("Retry")
                    .ghost()
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.refresh_providers(cx));
                    }),
            )
            .into_any_element();
    }

    let view = cx.entity();
    let back_button = Button::new("back-to-settings")
        .label("Back to Settings")
        .ghost()
        .flex_shrink_0()
        .on_click(move |_, _, cx| {
            view.update(cx, |this, cx| this.show_settings(cx));
        });

    let provider_heading = h_flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_4()
        .pt_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("AI providers"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Choose a provider, model, and authentication method."),
                ),
        )
        .child(back_button);

    // Status + active line above the provider list.
    let mut status = v_flex().gap_1().px_4().pt_3();
    if let Some(status_text) = &this.settings_status {
        status = status.child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(status_text.clone()),
        );
    }
    if let Some(active) = &this.active_provider {
        status = status.child(div().text_sm().text_color(theme.primary).child(format!(
                    "Active: {}{}",
                    active,
                    this.active_model
                        .as_deref()
                        .map(|model| format!(" / {model}"))
                        .unwrap_or_default()
                )));
    }

    let login_panel = render_active_login(this, cx);

    // Provider rows are virtualized and their fixed sizes are retained between
    // renders. This keeps scroll geometry stable while streamed UI updates call
    // for normal GPUI repaints.
    let view = cx.entity();
    let item_sizes = this.provider_row_sizes.clone();
    let list = v_virtual_list(
        view.clone(),
        "providers-list",
        item_sizes.clone(),
        move |this, range, window, cx| {
            range
                .into_iter()
                .map(|index| render_provider_row(this, index, item_sizes[index].height, window, cx))
                .collect::<Vec<_>>()
        },
    )
    .track_scroll(&this.settings_scroll)
    .flex_1();

    let scrollbar = Scrollbar::vertical(&this.settings_scroll).axis(ScrollbarAxis::Vertical);

    let mut root = v_flex()
        .flex_1()
        .min_h_0()
        .size_full()
        .child(provider_heading)
        .child(status)
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(list)
                .child(scrollbar),
        );
    if let Some(panel) = login_panel {
        root = root.child(div().px_4().pt_3().child(panel));
    }

    root.into_any_element()
}

/// A single fixed-height provider row (name + auth badges | controls).
fn render_provider_row(
    this: &WorktableView,
    index: usize,
    height: Pixels,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let Some(provider) = this.providers.get(index) else {
        return div().h(height).w_full().into_any_element();
    };
    let theme = cx.theme().clone();
    let name = provider.name.clone();
    let is_active = this.active_provider.as_deref() == Some(&provider.id);
    let logging_in = this.logging_in.contains(&provider.id);
    let active_model = this.active_model.clone();

    let mut badges = Vec::new();
    if provider.supports_api_key {
        badges.push(if provider.api_key_set {
            "API key ✓"
        } else {
            "API key"
        });
    }
    if provider.supports_oauth {
        badges.push(if provider.oauth_set {
            "OAuth ✓"
        } else {
            "OAuth"
        });
    }
    if is_active {
        badges.push("active");
    }
    let badge_text = badges.join(" · ");

    let mut left = h_flex().gap_2().items_center().flex_1().min_w_0();
    left = left.child(
        div()
            .flex_shrink_0()
            .text_sm()
            .font_weight(gpui::FontWeight::BOLD)
            .child(name),
    );
    if !badge_text.is_empty() {
        left = left.child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .line_clamp(1)
                .child(badge_text),
        );
    }

    let model_options = this
        .provider_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();
    let controls = render_provider_field(
        &cx.entity(),
        provider,
        model_options,
        logging_in,
        active_model,
        this.api_key_provider.as_deref() == Some(&provider.id),
        this.api_key_input.clone(),
        _window,
        cx,
    );

    h_flex()
        .h(height)
        .w_full()
        .px_3()
        .items_center()
        .justify_between()
        .border_b_1()
        .border_color(theme.border.opacity(0.5))
        .child(left)
        .child(controls)
        .into_any_element()
}

/// The controls on the right side of a provider row: model picker, API key,
/// and OAuth sign-in.
fn render_provider_field(
    view: &gpui::Entity<WorktableView>,
    provider: &ProviderInfo,
    model_options: Arc<Vec<(String, String)>>,
    logging_in: bool,
    active_model: Option<String>,
    showing_key: bool,
    api_key_input: Entity<InputState>,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let view = view.clone();
    let mut row = h_flex().gap_2().items_center();

    let configure_view = view.clone();
    let configure_provider_id = provider.id.clone();
    row = row.child(
        Button::new(format!("configure:{}", provider.id))
            .label("Configure")
            .icon(app_icon(IconName::Settings))
            .ghost()
            .h(px(28.))
            .on_click(move |_, _, cx| {
                configure_view.update(cx, |this, cx| {
                    this.configure_provider(&configure_provider_id, cx)
                });
            }),
    );

    let usable = provider.api_key_set || provider.oauth_set;
    if usable && !model_options.is_empty() {
        row = row.child(model_picker_button(
            &view,
            provider,
            model_options,
            active_model,
            cx,
        ));
    }

    if provider.supports_api_key {
        if showing_key {
            row = row
                .child(Input::new(&api_key_input).h(px(30.)).w(px(220.)))
                .child(
                    Button::new(format!("save-key:{}", provider.id))
                        .label("Save")
                        .primary()
                        .h(px(28.))
                        .on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |this, cx| this.save_api_key(cx));
                            }
                        }),
                );
        } else {
            let label = if provider.api_key_set {
                "Change API key"
            } else {
                "Set API key"
            };
            let view = view.clone();
            let provider_id = provider.id.clone();
            row = row.child(
                Button::new(format!("set-key:{}", provider.id))
                    .label(label)
                    .ghost()
                    .h(px(28.))
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.api_key_provider = Some(provider_id.clone());
                            let input = this.api_key_input.clone();
                            this.set_input(&input, "", cx);
                            cx.notify();
                        });
                    }),
            );
        }
    }

    if provider.supports_oauth {
        if provider.oauth_set {
            let view = view.clone();
            let provider_id = provider.id.clone();
            row = row.child(
                Button::new(format!("logout:{}", provider.id))
                    .label("Sign out")
                    .ghost()
                    .h(px(28.))
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.logout_provider(&provider_id, cx));
                    }),
            );
        } else if logging_in {
            let view = view.clone();
            let provider_id = provider.id.clone();
            row = row.child(
                Button::new(format!("cancel-login:{}", provider.id))
                    .label("Cancel")
                    .ghost()
                    .h(px(28.))
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.cancel_login(&provider_id, cx));
                    }),
            );
        } else {
            let view = view.clone();
            let provider_id = provider.id.clone();
            row = row.child(
                Button::new(format!("login:{}", provider.id))
                    .label("Sign in with OAuth")
                    .icon(app_icon(IconName::ExternalLink))
                    .h(px(28.))
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.login_oauth(&provider_id, cx));
                    }),
            );
        }
    }

    row.into_any_element()
}

/// A lazy model dropdown for a provider row. The full model list only builds
/// when the menu is opened (scrollable), so the settings page stays light.
fn model_picker_button(
    view: &gpui::Entity<WorktableView>,
    provider: &ProviderInfo,
    models: Arc<Vec<(String, String)>>,
    active_model: Option<String>,
    _cx: &mut App,
) -> gpui::AnyElement {
    let view = view.clone();
    let provider_id = provider.id.clone();
    let label = match &active_model {
        Some(id) if models.iter().any(|(model_id, _)| model_id == id) => id.clone(),
        _ => "Select model…".to_owned(),
    };

    Button::new(format!("model-picker:{}", provider.id))
        .label(label)
        .icon(app_icon(IconName::ChevronDown))
        .ghost()
        .h(px(28.))
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu;
            for (model_id, model_name) in models.iter() {
                let is_active = active_model.as_deref() == Some(model_id.as_str());
                let model_id = model_id.clone();
                let model_name = model_name.clone();
                let view = view.clone();
                let provider_id = provider_id.clone();
                menu = menu.item(PopupMenuItem::new(model_name).checked(is_active).on_click(
                    move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.select_model(&provider_id, &model_id, cx)
                        });
                    },
                ));
            }
            menu.scrollable(true)
        })
        .into_any_element()
}

/// A panel showing the in-progress OAuth login (auth URL, device code, and any
/// prompt the user must answer).
fn render_active_login(
    this: &WorktableView,
    cx: &mut Context<WorktableView>,
) -> Option<gpui::AnyElement> {
    let provider_id = this.logging_in.iter().next()?.clone();
    let theme = cx.theme().clone();

    let mut column = v_flex().gap_2();

    let mut has_content = false;
    if let Some(notice) = &this.auth_notice
        && notice.provider_id == provider_id {
            has_content = true;
            match &notice.notify {
                AuthNotifyKind::AuthUrl { url, instructions } => {
                    let url = url.clone();
                    let instructions = instructions.clone();
                    let view = cx.entity();
                    column = column
                        .child(
                            div().text_sm().child(
                                instructions
                                    .clone()
                                    .unwrap_or_else(|| "Open this URL to authorize:".to_owned()),
                            ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(url.clone()),
                        )
                        .child(
                            Button::new(format!("open-url:{provider_id}"))
                                .label("Open in browser")
                                .icon(app_icon(IconName::ExternalLink))
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| this.open_auth_url(&url, cx));
                                }),
                        );
                }
                AuthNotifyKind::DeviceCode {
                    user_code,
                    verification_uri,
                    ..
                } => {
                    let verification_uri = verification_uri.clone();
                    let user_code = user_code.clone();
                    let view = cx.entity();
                    column = column
                        .child(
                            div()
                                .text_sm()
                                .child("Enter this code on the provider's website:"),
                        )
                        .child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(theme.primary)
                                .child(user_code),
                        )
                        .child(
                            Button::new(format!("open-device-url:{provider_id}"))
                                .label("Open verification page")
                                .icon(app_icon(IconName::ExternalLink))
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.open_auth_url(&verification_uri, cx)
                                    });
                                }),
                        );
                }
                _ => {}
            }
        }

    if let Some(pending) = &this.pending_prompt
        && pending.provider_id == provider_id {
            has_content = true;
            match &pending.prompt {
                AuthPromptKind::Text { message, .. }
                | AuthPromptKind::Secret { message, .. }
                | AuthPromptKind::ManualCode { message, .. } => {
                    let message = message.clone();
                    let view = cx.entity();
                    column = column.child(div().text_sm().child(message)).child(
                        h_flex()
                            .gap_2()
                            .child(Input::new(&this.prompt_input).h(px(32.)))
                            .child(
                                Button::new(format!("answer-prompt:{provider_id}"))
                                    .label("Submit")
                                    .primary()
                                    .on_click(move |_, _, cx| {
                                        view.update(cx, |this, cx| this.answer_prompt(cx));
                                    }),
                            ),
                    );
                }
                AuthPromptKind::Select { message, options } => {
                    let message = message.clone();
                    let view = cx.entity();
                    let mut options_column = v_flex().gap_1();
                    for option in options {
                        let option_id = option.id.clone();
                        let label = option.label.clone();
                        options_column = options_column.child(
                            Button::new(format!("prompt-option:{}", option.id))
                                .label(label)
                                .ghost()
                                .on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.answer_prompt_option(&option_id, cx)
                                        });
                                    }
                                }),
                        );
                    }
                    column = column
                        .child(div().text_sm().child(message))
                        .child(options_column);
                }
            }
        }

    if !has_content {
        return None;
    }

    Some(
        v_flex()
            .gap_2()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .child(column)
            .into_any_element(),
    )
}

fn render_github_stars_page(
    this: &mut WorktableView,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();

    let total_text = if this.github_loading {
        match this.github_total_stars {
            Some(total) => format!("★ {} total stars — updating…", total),
            None => "Fetching stars…".to_owned(),
        }
    } else if let Some(total) = this.github_total_stars {
        format!("★ {} total stars", total)
    } else if this.github_error.is_some() {
        String::new()
    } else {
        "No data yet — enter a username and fetch.".to_owned()
    };

    let mut column = v_flex()
        .gap_2()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover);

    column = column
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("GitHub Stars"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Total stars + per-repo breakdown"),
                ),
        )
        .child(div().text_xs().text_color(theme.muted_foreground).child(
            "Stored in wt_ai_config (github_username). Uses GITHUB_TOKEN if set for 5000 req/h.",
        ));

    // Input + Save + Fetch: input on its own row so the buttons never
    // overflow the 390pt column; Save + Fetch share a second row.
    let save_view = view.clone();
    let fetch_view = view.clone();
    let input_row = v_flex()
        .gap_2()
        .w_full()
        .child(
            div()
                .w_full()
                .child(Input::new(&this.github_input).h(px(32.)).w_full()),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("github-save")
                        .label("Save")
                        .icon(app_icon(IconName::Check))
                        .h(px(28.))
                        .flex_shrink_0()
                        .on_click(move |_, _, cx| {
                            save_view.update(cx, |this, cx| this.save_github_username(cx));
                        }),
                )
                .child(
                    Button::new("github-fetch")
                        .label(if this.github_loading {
                            "Fetching…"
                        } else {
                            "Fetch Stars"
                        })
                        .icon(app_icon(IconName::ExternalLink))
                        .primary()
                        .h(px(28.))
                        .flex_shrink_0()
                        .disabled(this.github_loading)
                        .on_click(move |_, _, cx| {
                            fetch_view.update(cx, |this, cx| this.fetch_github_stars(cx));
                        }),
                ),
        );

    column = column.child(input_row);

    // Import starred repositories (with starred-at timestamps and
    // descriptions) as entries.
    let import_view = view.clone();
    column = column.child(
        v_flex()
            .gap_1()
            .w_full()
            .items_start()
            .child(
                Button::new("github-import-stars")
                    .label(if this.github_importing {
                        "Importing…"
                    } else {
                        "Import stars to entries"
                    })
                    .icon(app_icon(IconName::ArrowDown))
                    .primary()
                    .shadow_sm()
                    .h(px(28.))
                    .flex_shrink_0()
                    .disabled(this.github_importing || this.github_loading)
                    .on_click(move |_, _, cx| {
                        import_view.update(cx, |this, cx| this.import_github_stars(cx));
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Each star becomes a link entry with its date and description"),
            ),
    );
    if let Some(status) = &this.github_import_status {
        column = column.child(
            div()
                .text_sm()
                .text_color(theme.primary)
                .child(status.clone()),
        );
    }

    if !total_text.is_empty() {
        let color = if this.github_loading {
            theme.muted_foreground
        } else {
            theme.primary
        };
        column = column.child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(color)
                .child(total_text),
        );
    }

    if let Some(error) = &this.github_error {
        column = column.child(
            div()
                .text_sm()
                .text_color(theme.danger)
                .child(error.clone()),
        );
    }

    if this.github_loading {
        let phase = pulse_delta(&ZERON_PULSE, cx.entity_id(), cx);
        let opacity = worktable_ui::pulse_opacity(phase);
        column = column.child(
            div()
                .h(px(2.))
                .w_full()
                .rounded(px(2.))
                .bg(theme.border.opacity(0.5))
                .child(
                    div()
                        .h_full()
                        .w(relative(opacity.clamp(0.2, 1.0)))
                        .bg(theme.primary)
                        .rounded(px(2.)),
                ),
        );
    }

    if !this.github_repos.is_empty() {
        let mut repos_list = v_flex()
            .gap_1()
            .max_h(px(260.))
            .overflow_y_scrollbar()
            .pr_2();
        // Show at most 50 repos to keep UI snappy; user likely cares about top starred.
        for repo in this.github_repos.iter().take(50) {
            let name = repo.name.clone();
            let url = repo.html_url.clone();
            let stars = repo.stars;
            // Clicking the row opens the repo URL.
            let view_for_click = view.clone();
            let url_for_click = url.clone();
            let row_id = format!("github-repo:{}", name);
            repos_list = repos_list.child(
                h_flex()
                    .id(row_id)
                    .justify_between()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .hover(|s| s.bg(theme.tokens.list_hover))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        if !url_for_click.is_empty() {
                            view_for_click.update(cx, |this, cx| {
                                cx.open_url(&url_for_click);
                                let _ = this;
                            });
                        }
                    })
                    .child(div().text_sm().child(name).text_color(theme.foreground))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!("★ {}", stars)),
                    ),
            );
        }
        if this.github_repos.len() > 50 {
            repos_list =
                repos_list.child(div().text_xs().text_color(theme.muted_foreground).child(
                    format!("… and {} more repositories", this.github_repos.len() - 50),
                ));
        }
        column = column
            .child(div().h(px(1.)).bg(theme.border.opacity(0.5)))
            .child(repos_list);
    } else if this.github_total_stars == Some(0)
        && !this.github_loading
        && this.github_error.is_none()
    {
        // User exists but has no public repos.
        column = column.child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No public repositories found."),
        );
    }

    // Outer wrapper ensures a sensible max width in Settings (centered),
    // with page padding so the card never touches the window edges.
    div()
        .w_full()
        .max_w(px(520.))
        .mx_auto()
        .p_4()
        .child(column)
        .into_any_element()
}

fn composer_bar(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let bar = if this.composer.is_some() {
        let is_note = this.composer == Some(ComposerKind::Note);
        let is_link = this.composer == Some(ComposerKind::Link);
        let is_image = this.composer == Some(ComposerKind::Image);
        v_flex()
            .id("composer-active")
            .w_full()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(px(16.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .child(
                h_flex()
                    .w_full()
                    .justify_center()
                    .child(
                        ButtonGroup::new("composer-kind")
                            .compact()
                            .outline()
                            .child(
                                Button::new("composer-note")
                                    .label("Note")
                                    .icon(Icon::new(IconName::File))
                                    .selected(is_note),
                            )
                            .child(
                                Button::new("composer-link")
                                    .label("Link")
                                    .icon(Icon::new(IconName::ExternalLink))
                                    .selected(is_link),
                            )
                            .child(
                                Button::new("composer-image")
                                    .label("Image")
                                    .icon(Icon::new(IconName::GalleryVerticalEnd))
                                    .selected(is_image),
                            )
                            .on_click(cx.listener(|this, selected: &Vec<usize>, window, cx| {
                                if selected.contains(&0) {
                                    this.open_composer_note(window, cx);
                                } else if selected.contains(&1) {
                                    this.open_composer_link(window, cx);
                                } else if selected.contains(&2) {
                                    this.open_composer_image(window, cx);
                                }
                            })),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        Input::new(&this.composer_body)
                            .h(px(36.))
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        Button::new("cancel-composer")
                            .label("Cancel")
                            .ghost()
                            .text_xs()
                            .flex_shrink_0()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_composer(cx))),
                    )
                    .child(
                        Button::new("submit-composer")
                            .label("Save")
                            .primary()
                            .icon(app_icon(IconName::Check))
                            .text_xs()
                            .flex_shrink_0()
                            .on_click(cx.listener(|this, _, _, cx| this.submit_composer(cx))),
                    ),
            )
            .into_any_element()
    } else {
        h_flex()
            .id("composer-prompt")
            .w_full()
            .h(px(52.))
            .items_center()
            .gap_3()
            .px_4()
            .rounded(px(16.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .cursor_pointer()
            .hover(|s| s.bg(theme.tokens.list_hover))
            .on_click(cx.listener(|this, _, window, cx| this.open_composer_note(window, cx)))
            .child(
                div()
                    .size_6()
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(theme.primary.opacity(0.12))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Plus).size(px(16.)).text_color(theme.primary)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("Add a note"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("⌘N note  •  ⌘L link  •  drag image"),
                    ),
            )
            .child(Icon::new(IconName::ChevronRight).size(px(14.)).text_color(theme.muted_foreground))
            .into_any_element()
    };

    div()
        .absolute()
        .left(px(16.))
        .right(px(16.))
        .bottom(px(16.))
        .child(bar)
        .into_any_element()
}

/// The Entries pane: sort bar + virtualized card list + composer. Slides as
/// a unit under the fixed Entries|Agent button group.
fn render_entries_pane(
    this: &mut WorktableView,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();
    let visible = this.visible_entries();

    // Text for "Copy as list" — when multiple are selected, copy those; otherwise copy all visible.
    let selected_for_clipboard = this.selected.clone();
    let clipboard_text = {
        let entries_to_copy: Vec<&WorktableEntry> = if selected_for_clipboard.len() > 1 {
            visible
                .iter()
                .filter(|e| selected_for_clipboard.contains(&e.id))
                .copied()
                .collect()
        } else {
            visible.to_vec()
        };
        entries_to_copy
            .iter()
            .map(|entry| {
                if let Some(title) = &entry.title {
                    format!("{title}\n{}", entry.content)
                } else {
                    entry.content.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    };

    // Flatten sections into virtual-list items with fixed row sizes.
    let mut sections: Vec<(String, Vec<&WorktableEntry>)> = Vec::new();
    for entry in visible.clone() {
        let label = if this.sort_mode == SortMode::Topic {
            helix_primary_topic(entry)
        } else if this.sort_mode == SortMode::Time {
            // Time sort groups by creation day: Today / Yesterday / weekday / date.
            crate::format::day_bucket(entry.created_at)
        } else {
            library_section_name(entry)
        };
        if let Some((_, entries)) = sections.iter_mut().find(|(name, _)| *name == label) {
            entries.push(entry);
        } else {
            sections.push((label, vec![entry]));
        }
    }
    if this.sort_mode == SortMode::Topic {
        sections.sort_by(|a, b| {
            if a.0 == "OTHER" {
                std::cmp::Ordering::Greater
            } else if b.0 == "OTHER" {
                std::cmp::Ordering::Less
            } else {
                a.0.cmp(&b.0)
            }
        });
    }

    enum Row {
        Header(String),
        Card(std::sync::Arc<WorktableEntry>),
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut sizes: Vec<Size<Pixels>> = Vec::new();
    for (label, entries) in &sections {
        rows.push(Row::Header(label.clone()));
        sizes.push(size(px(0.), px(SECTION_HEADER_HEIGHT)));
        for entry in entries {
            rows.push(Row::Card(std::sync::Arc::new((*entry).clone())));
            sizes.push(size(px(0.), px(ENTRY_CARD_HEIGHT)));
        }
    }
    let rows = std::rc::Rc::new(rows);
    let item_sizes = std::rc::Rc::new(sizes);

    let list = v_virtual_list(
        view.clone(),
        "entries-virtual-list",
        item_sizes.clone(),
        move |this, range, window, cx| {
            let clipboard_text = clipboard_text.clone();
            let rows = rows.clone();
            range
                .into_iter()
                .map(|index| {
                    let row = &rows[index];
                    match row {
                        Row::Header(label) => {
                            let theme = cx.theme().clone();
                            library_section_heading(label, &theme).into_any_element()
                        }
                        Row::Card(entry) => render_entry_card(
                            entry,
                            &this.selected,
                            &clipboard_text,
                            cx,
                        ),
                    }
                })
                .collect::<Vec<_>>()
        },
    )
    .track_scroll(&this.entries_scroll)
    .flex_1();

    let scrollbar = Scrollbar::vertical(&this.entries_scroll).axis(ScrollbarAxis::Vertical);

    // Empty state.
    let empty = visible.is_empty();
    let empty_el = {
        let message = if this.query.is_empty() {
            "No entries yet — press ⌘N to create one."
        } else {
            "No entries match your search."
        };
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_color(theme.muted_foreground)
            .child(div().text_sm().child(message))
            .into_any_element()
    };

    // Entrance: when the list last gained a new entry, the whole list starts
    // one card lower and glides up while the new card fades in — the existing
    // items appear to be pushed smoothly down by the newcomer.
    let inserting = this
        .list_insert_at
        .is_some_and(|at| at.elapsed() < Duration::from_millis(350));
    if !inserting && this.list_insert_at.is_some() {
        this.list_insert_at = None;
    }

    let sort = sort_bar(this, cx);

    let body = if empty {
        empty_el
    } else {
        let list_container = div()
            .flex_1()
            .min_h_0()
            .relative()
            .px_4()
            .pb(px(84.))
            .child(list)
            .child(scrollbar);
        let container = if inserting {
            let recent = this.recent_entry_id.clone();
            list_container
                .with_animation(
                    "entries-insert",
                    worktable_ui::RESIZE.animation(),
                    move |el, t| el.relative().top(px(ENTRY_CARD_HEIGHT * (1.0 - t))),
                )
                .into_any_element()
        } else {
            list_container.into_any_element()
        };
        v_flex().flex_1().min_h_0().size_full().child(container).into_any_element()
    };

    v_flex()
        .id("entries-pane")
        .flex_1()
        .min_h_0()
        .size_full()
        .child(
            v_flex()
                .pt_2()
                .pb_1()
                .px_4()
                .gap_2()
                .child(sort)
                .when(inserting, |el| {
                    // The new card fades in at the top while the list glides.
                    let _ = &el;
                    el
                }),
        )
        .child(body)
        .child(composer_bar(this, cx).into_any_element())
        .into_any_element()
}


fn library_tabs(this: &WorktableView, cx: &mut Context<WorktableView>) -> gpui::AnyElement {
    let is_entries = this.mode == AppMode::Entries;
    h_flex()
        .id("library-tabs")
        .w_full()
        .justify_center()
        .py_2()
        .child(
            ButtonGroup::new("library-tabs-group")
                .child(
                    Button::new("entries-tab")
                        .label("Entries")
                        .selected(is_entries),
                )
                .child(
                    Button::new("agent-tab")
                        .label("Agent")
                        .selected(!is_entries && this.mode == AppMode::Assistant),
                )
                .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                    if selected.contains(&0) {
                        this.show_entries(cx);
                    } else if selected.contains(&1) {
                        this.show_assistant(cx);
                    }
                })),
        )
        .into_any_element()
}

fn sort_bar(this: &WorktableView, cx: &mut Context<WorktableView>) -> gpui::AnyElement {
    let is_time = this.sort_mode == SortMode::Time;
    let is_alpha = this.sort_mode == SortMode::Alpha;
    let is_topic = this.sort_mode == SortMode::Topic;
    h_flex()
        .id("sort-bar")
        .w_full()
        .justify_center()
        .pb_2()
        .child(
            ButtonGroup::new("sort-mode-group")
                .compact()
                .outline()
                .child(
                    Button::new("sort-time")
                        .label("Time")
                        .icon(Icon::new(IconName::SortDescending))
                        .selected(is_time),
                )
                .child(
                    Button::new("sort-alpha")
                        .label("A–Z")
                        .icon(Icon::new(IconName::ALargeSmall))
                        .selected(is_alpha),
                )
                .child(
                    Button::new("sort-topic")
                        .label("Topic")
                        .icon(Icon::new(IconName::BookOpen))
                        .selected(is_topic),
                )
                .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                    if selected.contains(&0) {
                        this.set_sort_mode(SortMode::Time, cx);
                    } else if selected.contains(&1) {
                        this.set_sort_mode(SortMode::Alpha, cx);
                    } else if selected.contains(&2) {
                        this.set_sort_mode(SortMode::Topic, cx);
                    }
                })),
        )
        .into_any_element()
}

pub(crate) fn helix_primary_topic(entry: &WorktableEntry) -> String {
    // HelixDB topic extraction — mirrors worktable_helix::topic_for_entry.
    // We compute on the fly so the UI stays in sync even before the graph is built.
    let db_entry = worktable_db::Entry {
        id: entry.id.clone(),
        kind: entry.kind.clone(),
        content: entry.content.clone(),
        title: entry.title.clone(),
        source: entry.source.clone(),
        created_at: entry.created_at,
    };
    worktable_helix::topic_for_entry(&db_entry)
        .into_iter()
        .next()
        .map(|t| t.to_uppercase())
        .unwrap_or_else(|| "OTHER".to_owned())
}

fn library_section_name(entry: &WorktableEntry) -> String {
    let source = entry.source.trim();
    if !source.is_empty()
        && !source.eq_ignore_ascii_case("worktable")
        && !source.eq_ignore_ascii_case("selection")
    {
        return source.to_uppercase();
    }

    match entry.kind.as_str() {
        "link" => "CONFIGURATION FORMATS".to_owned(),
        "image" => "REFERENCES".to_owned(),
        _ => "RESEARCH".to_owned(),
    }
}

fn library_section_heading(label: &str, theme: &gpui_component::Theme) -> impl IntoElement {
    h_flex()
        .w_full()
        .h(px(16.))
        .px_4()
        .items_center()
        .child(
            div()
                .flex_shrink_0()
                .pr_3()
                .bg(theme.tokens.background)
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(label.to_owned()),
        )
        .child(div().flex_1().h(px(1.)).bg(theme.border))
}

/// One entry card: fixed-height row for the virtual list. Selection circle's
/// tick is centered; the whole card carries the click/context-menu wiring.
fn render_entry_card(
    entry: &WorktableEntry,
    selected: &HashSet<String>,
    clipboard_text: &str,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();
    let is_selected = selected.contains(&entry.id);
    let topic = helix_primary_topic(entry);
    let is_image = entry.kind == "image";

    let selection = div()
        .size_5()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .rounded_full()
        .border_1()
        .border_color(theme.primary.opacity(0.55))
        .when(is_selected, |this| {
            this.bg(theme.primary).child(
                Icon::new(IconName::Check)
                    .size(px(12.))
                    .text_color(theme.primary_foreground),
            )
        });

    let mut content = v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .text_sm()
        .text_color(theme.foreground)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .px_2()
                        .py(px(2.))
                        .rounded(px(RADIUS_CHIP))
                        .bg(theme.muted)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(topic),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(entry_kind(&entry.kind).to_owned()),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(crate::format::relative_time(entry.created_at)),
                ),
        );
    if let Some(title) = &entry.title {
        content = content.child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .line_clamp(1)
                .child(title.clone()),
        );
    }
    if is_image {
        // Compact photo row: icon + filename (kept whole) + path (truncates).
        let filename = entry
            .content
            .split('/')
            .next_back()
            .unwrap_or(&entry.content)
            .to_owned();
        content = content.child(
            h_flex()
                .gap_2()
                .items_center()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .p_1()
                .rounded(px(RADIUS_CHIP))
                .bg(theme.muted.opacity(0.5))
                .border_1()
                .border_color(theme.border.opacity(0.6))
                .child(
                    Icon::new(IconName::GalleryVerticalEnd)
                        .size(px(14.))
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(filename),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .line_clamp(1)
                        .child(entry.content.clone()),
                ),
        );
    } else {
        // Card preview: collapse hard line breaks so the body is one flowing
        // paragraph. `line_clamp` only clamps wrapped lines — raw `\n`s render
        // full-height, overflow the fixed 108px row, and paint over the
        // neighboring cards. Full content is still used for copy/open.
        let preview = entry.content.split_whitespace().collect::<Vec<_>>().join(" ");
        content = content.child(
            div()
                .line_clamp(if entry.title.is_some() { 2 } else { 3 })
                .text_color(if entry.title.is_some() {
                    theme.muted_foreground
                } else {
                    theme.foreground
                })
                .child(preview),
        );
    }

    let entry_id = entry.id.clone();
    let card = h_flex()
        .id(format!("entry:{}", entry.id))
        .w_full()
        .h(px(ENTRY_CARD_HEIGHT))
        .overflow_hidden()
        .items_center()
        .gap_4()
        .px(px(16.))
        .rounded(px(RADIUS_CARD))
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .shadow_2xs()
        .on_click(cx.listener({
            let entry_id = entry_id.clone();
            move |this, event: &ClickEvent, _window, cx| {
                let mods = event.modifiers();
                let shift = mods.shift;
                let cmd = mods.platform || mods.control;
                if shift {
                    this.select_range(entry_id.clone(), cx);
                } else if cmd {
                    this.select_at(entry_id.clone(), true);
                } else {
                    this.select_at(entry_id.clone(), false);
                }
                cx.notify();
            }
        }))
        .cursor_pointer()
        .child(selection)
        .child(content);

    let card = if is_selected {
        card.bg(theme.tokens.list_active)
    } else {
        card
    };

    let hover_key = format!("entry-hover:{}", entry.id);
    let card = card
        .on_hover(hover_listener(hover_key.clone()))
        .bg(hover_blend(
            &hover_key,
            if is_selected {
                *theme.tokens.list_active
            } else {
                theme.popover
            },
            *theme.tokens.list_hover,
        ));

    // Right-click selects the row (if not already selected); the menu offers
    // copy / open-link / delete like before.
    let id_for_right = entry.id.clone();
    let card = card.on_mouse_down(MouseButton::Right, {
        let view = view.clone();
        move |_event, _window, cx| {
            view.update(cx, |this, cx| {
                if !this.selected.contains(&id_for_right) {
                    this.select_at(id_for_right.clone(), false);
                    cx.notify();
                }
            });
        }
    });

    let entry_kind_menu = entry.kind.clone();
    let content_for_menu = entry.content.clone();
    let text_for_menu = clipboard_text.to_owned();
    let id_for_copy = entry.id.clone();
    let id_del = entry.id.clone();
    let card = card.context_menu(move |menu, _window, _cx| {
        let content_copy = content_for_menu.clone();
        let url_open = content_for_menu.clone();
        let is_link_menu = entry_kind_menu == "link";
        let text = text_for_menu.clone();
        let id_copy2 = id_for_copy.clone();
        let id_del2 = id_del.clone();
        let view_del = view.clone();
        menu.item(
            PopupMenuItem::new("Copy")
                .icon(Icon::new(IconName::Copy))
                .on_click(move |_event, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(content_copy.clone()));
                    let _ = id_copy2;
                }),
        )
        .item(
            PopupMenuItem::new("Copy as list")
                .icon(Icon::new(IconName::Copy))
                .on_click(move |_event, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                }),
        )
        .when(is_link_menu, |m| {
            m.item(
                PopupMenuItem::new("Open link")
                    .icon(Icon::new(IconName::ExternalLink))
                    .on_click(move |_event, _window, cx| {
                        cx.open_url(&url_open);
                    }),
            )
        })
        .separator()
        .item(
            PopupMenuItem::new("Delete")
                .icon(Icon::new(IconName::Delete))
                .on_click(move |_event, _window, cx| {
                    view_del.update(cx, |this, cx| {
                        this.selected.clear();
                        this.selected.insert(id_del2.clone());
                        this.selected_anchor = Some(id_del2.clone());
                        this.delete_selected(cx);
                    });
                }),
        )
    });

    card.into_any_element()
}


fn entry_kind(kind: &str) -> &'static str {
    match kind {
        "text" => "TEXT",
        "link" => "LINK",
        "image" => "IMAGE",
        _ => "ENTRY",
    }
}

/// Trim whitespace; empty strings become `None`.
fn trim_opt(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

// ---------------------------------------------------------------------------
// AI Assistant pane
// ---------------------------------------------------------------------------

fn render_assistant(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let configured = this.agent_ready();

    let mut messages = v_flex()
        .id("assistant-messages")
        .flex_1()
        .gap_3()
        .overflow_y_scroll()
        .p_4();

    // One shared pulse-clock phase for every animated loader in this render.
    let dots_delta = if this.assistant_busy || this.messages.iter().any(|m| m.is_thinking_only())
    {
        pulse_delta(&TEXT_DOTS, cx.entity_id(), cx)
    } else {
        0.0
    };

    if this.messages.is_empty() {
        messages = messages.child(fade_in(
            "assistant-welcome",
            div().child(welcome_panel(&theme, configured)).when(!configured, |el| {
                el.mt_2().child(
                    Button::new("assistant-setup-cta")
                        .label("Configure provider")
                        .icon(app_icon(IconName::Settings))
                        .primary()
                        .shadow_sm()
                        .on_click(cx.listener(|this, _, _, cx| this.show_provider_config(cx))),
                )
            }),
        ));
    } else {
        for (idx, message) in this.messages.iter().enumerate() {
            let id = SharedString::from(format!("msg-{}-{}", idx, message.text.len()));
            let bubble = render_message(&theme, message, dots_delta);
            let animated = if message.streaming {
                fade_quick(id, div().child(bubble))
            } else {
                fade_in(id, div().child(bubble))
            };
            messages = messages.child(animated);
        }
        if this.assistant_busy {
            // Waiting for the LLM: bobbing dots while no answer bubble is
            // streaming yet (the thinking block carries its own "Thinking" dots).
            let waiting = this
                .messages
                .last()
                .map(|m| m.is_thinking_only() || !m.streaming)
                .unwrap_or(true);
            if waiting {
                let bob_delta = pulse_delta(&BOBBING_DOTS, cx.entity_id(), cx);
                messages = messages.child(
                    h_flex()
                        .id("assistant-waiting")
                        .px_3()
                        .child(worktable_ui::bobbing_dots(
                            bob_delta,
                            theme.muted_foreground,
                            px(7.),
                        )),
                );
            }
        }
    }

    let helix_button = Button::new("build-helix")
        .icon(app_icon(IconName::HardDrive))
        .ghost()
        .tooltip("Build Helix knowledge graph (topics + relations) from entries — stored next to worktable.db as helix.json")
        .disabled(this.helix_building)
        .on_click(cx.listener(|this, _, _, cx| this.build_helix(cx)));

    let helix_status = this
        .helix_status
        .as_deref()
        .map(|s| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(s.to_owned())
                .into_any_element()
        })
        .unwrap_or_else(|| div().into_any_element());

    v_flex()
        .flex_1()
        .min_h_0()
        .child(messages)
        .child(
            v_flex()
                .gap_1()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_2()
                        .w_full()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    Input::new(&this.assistant_input)
                                        .disabled(!configured)
                                        .h(px(36.))
                                        .w_full(),
                                ),
                        )
                        .child(helix_button)
                        .child(
                            Button::new("send-assistant")
                                .label(if this.assistant_busy { "…" } else { "Send" })
                                .primary()
                                .icon(app_icon(IconName::ArrowRight))
                                .flex_shrink_0()
                                .disabled(!configured || this.assistant_busy)
                                .on_click(cx.listener(|this, _, _, cx| this.send_assistant(cx))),
                        ),
                )
                .child(helix_status),
        )
        .into_any_element()
}
