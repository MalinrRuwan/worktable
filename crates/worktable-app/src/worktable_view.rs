//! The main Worktable view: sidebar navigation, searchable entries list,
//! inline composer, the AI assistant pane, and the AI provider settings panel.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    AnimationExt as _, App, AppContext as _, Bounds, ClickEvent, ClipboardItem, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, ImageSource, InteractiveElement as _, IntoElement,
    Keystroke, MouseButton, MouseMoveEvent, MousePressureEvent, ObjectFit, ParentElement as _,
    Pixels, Point, PressureStage, Render, Role, ScrollHandle, SharedString, Size,
    StatefulInteractiveElement as _, Styled, StyledImage as _, Subscription, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, img,
    linear_color_stop, linear_gradient, point, rems, size, svg,
};
use gpui_component::button::{Button, ButtonGroup, ButtonVariants};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::kbd::Kbd;
use gpui_component::list::ListItem;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu, PopupMenuItem};
use gpui_component::scroll::{ScrollableElement as _, Scrollbar, ScrollbarAxis};
use gpui_component::switch::Switch;
use gpui_component::text::TextView;
use gpui_component::{
    ActiveTheme, Disableable, FocusTrapElement as _, Icon, IconName, Root, Selectable, Sizable,
    VirtualListScrollHandle, WindowExt as _, h_flex, v_flex, v_virtual_list,
};

use crate::design;
use worktable_ai::WorktableEntry;
use worktable_events::{
    AuthNotifyKind, AuthPromptKind, KnowledgeCitation, ProviderInfo, ProvidersSnapshot,
    WorktableEvent,
};
use worktable_ui::citations::CitationOpenHandler;
use worktable_ui::{
    CircleAction, CitationRef, Orb, OrbVariant, TEXT_DOTS, fade_in, fade_quick, pulse_delta,
    splash_out,
};

use crate::assistant::{
    ChatMessage, MessageOptions, Role as ChatRole, ThinkingToggle as ToggleThinking,
    render_message, welcome_panel,
};
use crate::service::WorktableService;

struct ChatRow {
    id: String,
    title: SharedString,
    updated_at: i64,
}

enum ChatListState {
    Loading,
    Ready,
    Failed(SharedString),
}

struct ChatSheet {
    opened_at: Instant,
    closing_at: Option<Instant>,
    return_focus: Option<FocusHandle>,
}

#[cfg(test)]
#[path = "worktable_view_tests.rs"]
mod worktable_view_tests;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppMode {
    Entries,
    Assistant,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SettingsTab {
    General,
    Appearance,
    Data,
    Providers,
}

impl SettingsTab {
    fn title(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Data => "Data",
            SettingsTab::Providers => "Providers",
        }
    }

    fn description(self) -> &'static str {
        match self {
            SettingsTab::General => "Window and background behavior",
            SettingsTab::Appearance => "Theme and assistant display",
            SettingsTab::Data => "GitHub stars and imports",
            SettingsTab::Providers => "AI providers, models, and API keys",
        }
    }
}

/// Theme selection: light, dark, or follow the system appearance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AppThemeMode {
    Light,
    Dark,
    System,
}

impl AppThemeMode {
    fn as_config(self) -> &'static str {
        match self {
            AppThemeMode::Light => "light",
            AppThemeMode::Dark => "dark",
            AppThemeMode::System => "system",
        }
    }

    fn from_config(value: &str) -> Option<Self> {
        match value {
            "light" => Some(AppThemeMode::Light),
            "dark" => Some(AppThemeMode::Dark),
            "system" => Some(AppThemeMode::System),
            _ => None,
        }
    }
}

/// Full-content entry view: a morph from the card's rect to the middle of the
/// UI (transitions.dev-style; GPUI has no scale, so it tweens the rect and
/// fades the content in).
pub(crate) struct EntryModalState {
    pub(crate) entry_id: String,
    pub(crate) origin: Bounds<Pixels>,
    pub(crate) opened_at: Instant,
    pub(crate) closing_at: Option<Instant>,
}

/// Fixed heights for the virtualized lists live in [`crate::design`] as rems
/// so they zoom with the interface; [`crate::design::to_pixels`] resolves them
/// at render time.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SortMode {
    #[default]
    Time,
    Alpha,
    Topic,
}

impl SortMode {
    /// The direction a mode starts in when the user switches to it: recency
    /// reads newest first, while the alphabetical orders read A to Z. Clicking
    /// the already-active mode flips from there.
    pub(crate) const fn default_ascending(self) -> bool {
        match self {
            SortMode::Time => false,
            SortMode::Alpha | SortMode::Topic => true,
        }
    }
}

fn app_icon(name: IconName) -> Icon {
    Icon::new(name).size_4()
}

/// macOS Accessibility trust, stubbed off-platform so the view compiles
/// everywhere.
fn accessibility_trusted() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::status_item::accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Open the Accessibility pane in System Settings, stubbed off-platform.
fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    crate::status_item::open_accessibility_settings();
}

/// One keycap for the onboarding shortcut rows. The keystroke comes from the
/// keymap when the action is bound (single source of truth) and falls back to
/// the literal while the app is still starting up. Styled with the app's chip
/// treatment: `sm` radius, muted surface, foreground text.
fn onboarding_kbd(
    action: &dyn gpui::Action,
    fallback: &'static str,
    window: &Window,
    theme: &gpui_component::theme::Theme,
) -> gpui::AnyElement {
    let kbd = Kbd::binding_for_action(action, None, window)
        .unwrap_or_else(|| Kbd::new(Keystroke::parse(fallback).expect("valid shortcut")));
    let selector = format!("onboarding-kbd-{fallback}");
    div()
        .debug_selector(move || selector.clone())
        .flex_shrink_0()
        .child(
            kbd.outline()
                .rounded(theme.radius_tokens().sm)
                .min_w(rems(1.5))
                .px_1p5()
                .text_color(theme.foreground)
                .bg(theme.muted)
                .border_color(theme.border),
        )
        .into_any_element()
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

/// One page of the first-run tour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OnboardingStep {
    /// What the app is for and the shortcuts that drive it.
    Welcome,
    /// macOS Accessibility permission for global capture (skippable).
    Accessibility,
    /// Connect an AI provider (skippable).
    Provider,
}

/// Live first-run tour state.
pub(crate) struct OnboardingState {
    pub(crate) step: OnboardingStep,
    /// Cached `AXIsProcessTrusted`, refreshed when the step is shown and when
    /// the user asks to check again.
    pub(crate) accessibility_trusted: bool,
}

impl OnboardingState {
    fn new() -> Self {
        Self {
            step: OnboardingStep::Welcome,
            accessibility_trusted: accessibility_trusted(),
        }
    }
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
    /// The full-content entry view, when open (or playing its close morph).
    pub(crate) entry_modal: Option<EntryModalState>,
    /// Draft editor for the entry view's Edit mode.
    entry_edit_input: Entity<TextareaState>,
    /// Whether the entry view is showing the markdown editor.
    pub(crate) entry_editing: bool,
    entry_edit_status: Option<String>,
    /// Live prepainted bounds per entry card, keyed by entry id, in window
    /// coordinates. The morph starts and ends on the actual card.
    /// `Rc<RefCell<..>>` because prepaint writers must not mutate view state
    /// (or notify) mid-render.
    pub(crate) card_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// The content column's top-left in window coordinates. The column is
    /// centered on wide windows, so card/click positions must be converted to
    /// the column's local space before positioning the morph overlay.
    content_origin: Rc<Cell<Point<Pixels>>>,
    /// Last pointer position inside the library; the morph origin fallback
    /// for triggers that carry no position (context menu, keyboard).
    pointer_position: Point<Pixels>,
    /// When the Entries ⇄ Agent page transition started; `None` at rest.
    page_anim_at: Option<Instant>,

    // Composer
    composer_body: Entity<InputState>,
    assistant_input: Entity<InputState>,

    // Assistant
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) assistant_busy: bool,
    /// A chat id is allocated on the first prompt, not for an empty draft.
    chat_id: Option<String>,
    chat_revision: i64,
    chat_save_pending: bool,
    chat_save_error: Option<SharedString>,
    chats: Vec<ChatRow>,
    chats_state: ChatListState,
    chat_loading: Option<String>,
    chats_sheet: Option<ChatSheet>,
    chats_focus: FocusHandle,
    chats_scroll: VirtualListScrollHandle,
    /// Reject list/transcript loads after dismissal or a newer selection.
    chats_generation: u64,
    /// Scroll position of the assistant transcript (explicit scrollbar).
    assistant_scroll: ScrollHandle,
    /// Name of the tool currently executing, rendered as an orb status row.
    pub(crate) active_tool: Option<String>,
    /// `search_knowledge` hits waiting to be attached to the answer.
    pending_citations: Vec<KnowledgeCitation>,
    /// Whether the assistant's reasoning is rendered (Settings → UI toggle).
    pub(crate) show_thinking: bool,
    /// Keep running in the menu bar when the window closes (General).
    pub(crate) background_on_close: bool,

    // Provider settings
    pub(crate) providers: Vec<ProviderInfo>,
    provider_models: HashMap<String, Arc<Vec<(String, String)>>>,
    active_provider: Option<String>,
    active_model: Option<String>,
    providers_loading: bool,
    /// Provider whose configuration dialog is open, if any.
    api_key_provider: Option<String>,
    api_key_input: Entity<InputState>,
    logging_in: HashSet<String>,
    pending_prompt: Option<PendingAuthPrompt>,
    prompt_input: Entity<InputState>,
    auth_notice: Option<AuthNotice>,
    settings_status: Option<String>,
    /// Feedback from actions inside the provider configuration dialog.
    provider_dialog_status: Option<String>,

    // GitHub Stars
    github_input: Entity<InputState>,
    github_username: Option<String>,
    github_total_stars: Option<u64>,
    github_repos: Vec<crate::github::GithubRepo>,
    github_loading: bool,
    github_error: Option<String>,

    // Helix embedded graph
    pub(crate) knowledge_building: bool,
    pub(crate) knowledge_status: Option<String>,
    /// Set to stop a running knowledge build (button clicked again).
    knowledge_cancel: Arc<std::sync::atomic::AtomicBool>,

    /// Set when a stored provider was found to be stale and auto-cleared;
    /// drives the assistant's "provider was removed — pick a new one" note.
    stale_provider_cleared: bool,

    /// Locally extracted primary topic per entry id, rebuilt whenever entries
    /// change. Topic grouping reads this instead of re-extracting per frame.
    topic_cache: HashMap<String, String>,
    /// Topics from the knowledge graph (AI-named once a build enriched them).
    /// These win over the local cache, so grouping and search use the graph's
    /// vocabulary.
    graph_topics: HashMap<String, String>,

    // UI state
    pub(crate) sort_mode: SortMode,
    /// Sort direction. Clicking the already-active sort mode flips this.
    pub(crate) sort_ascending: bool,
    pub(crate) theme_mode: AppThemeMode,
    /// `None` shows the Settings category list; `Some` a category page.
    settings_tab: Option<SettingsTab>,
    pub(crate) entries_scroll: VirtualListScrollHandle,
    /// When the entries list last gained a new top entry — drives the
    /// "existing list glides down while the new card fades in" entrance.
    list_insert_at: Option<Instant>,
    recent_entry_id: Option<String>,
    github_importing: bool,
    github_import_status: Option<String>,
    splash_start: Option<Instant>,
    /// First-run tour; `None` once completed or skipped.
    pub(crate) onboarding: Option<OnboardingState>,

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
                .placeholder("Search entries")
                .clean_on_escape()
        });
        let composer_body = cx.new(|cx| InputState::new(window, cx).placeholder("Add a note"));
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
        let entry_edit_input =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("Write markdown…"));

        let mut view = Self {
            service,
            focus_handle,
            mode: AppMode::Entries,
            entries: Vec::new(),
            selected: HashSet::new(),
            selected_anchor: None,
            search_input,
            query: String::new(),
            entry_modal: None,
            entry_edit_input,
            entry_editing: false,
            entry_edit_status: None,
            card_bounds: Rc::new(RefCell::new(HashMap::new())),
            content_origin: Rc::new(Cell::new(point(Pixels::ZERO, Pixels::ZERO))),
            pointer_position: point(Pixels::ZERO, Pixels::ZERO),
            page_anim_at: None,
            composer_body,
            assistant_input,
            messages: Vec::new(),
            assistant_busy: false,
            chat_id: None,
            chat_revision: 0,
            chat_save_pending: false,
            chat_save_error: None,
            chats: Vec::new(),
            chats_state: ChatListState::Ready,
            chat_loading: None,
            chats_sheet: None,
            chats_focus: cx.focus_handle(),
            chats_scroll: VirtualListScrollHandle::new(),
            chats_generation: 0,
            assistant_scroll: ScrollHandle::new(),
            active_tool: None,
            pending_citations: Vec::new(),
            show_thinking: false,
            // Seeded from the process mirror; the async config read refines it.
            background_on_close: crate::preferences::background_on_close(),
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
            provider_dialog_status: None,
            github_input,
            github_username: None,
            github_total_stars: None,
            github_repos: Vec::new(),
            github_loading: false,
            github_error: None,
            knowledge_building: false,
            knowledge_status: None,
            knowledge_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stale_provider_cleared: false,
            topic_cache: HashMap::new(),
            graph_topics: HashMap::new(),
            sort_mode: SortMode::Time,
            sort_ascending: SortMode::Time.default_ascending(),
            theme_mode: AppThemeMode::System,
            settings_tab: None,
            entries_scroll: VirtualListScrollHandle::new(),
            list_insert_at: None,
            recent_entry_id: None,
            github_importing: false,
            github_import_status: None,
            splash_start: Some(Instant::now()),
            onboarding: None,

            _subscriptions: Vec::new(),
        };

        // The view owns the keyboard until a text field takes it: list
        // navigation (arrows, Enter, Backspace) then works before the first
        // click, which is what makes the app feel keyboard-first.
        view.focus_handle.focus(window, cx);
        view.subscribe(window, cx);
        view.load_entries(cx);
        view.refresh_providers(cx);
        view.load_github_username(cx);
        view.load_preferences(window, cx);
        view.load_knowledge_topics(cx);
        view
    }

    fn subscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Route search input changes into `self.query`.
        let query_input = self.search_input.clone();
        let subscription = cx.subscribe(&self.search_input, move |this, _emitter, event, cx| {
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                // Enter in the search field asks the agent.
                this.ask_agent_from_search(cx);
            }
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
                    let input = this.composer_body.clone();
                    if this.queue_composer_entry(cx) {
                        this.set_input(&input, "", cx);
                    }
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
                this.rebuild_topic_cache();
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

    /// Open the GitHub stars dialog: account, fetch, and import controls in
    /// one modal instead of a full settings page.
    pub fn open_github_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.github_username.is_none() {
            self.load_github_username(cx);
        }
        let main = cx.entity();
        let dialog_view = cx.new(|_| GithubStarsDialog {
            main: main.downgrade(),
        });
        window.open_dialog(cx, move |dialog, window, _cx| {
            dialog
                .title("GitHub stars")
                .width(dialog_width(window, design::GITHUB_DIALOG_WIDTH))
                .child(dialog_view.clone())
                .footer(
                    DialogFooter::new().child(
                        Button::new("github-dialog-close")
                            .label("Close")
                            .ghost()
                            .debug_selector(|| "github-dialog-close".into())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
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
                        this.github_import_status = Some(format!("Import failed: {error}"));
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

    /// Build the knowledge graph: mirror entries locally (topic extraction +
    /// semantic links), then let the active provider's model name the topics
    /// so the graph links entries by meaning rather than wording.
    /// Abort a running knowledge build; the pass stops between phases/batches.
    pub fn cancel_knowledge(&mut self, cx: &mut Context<Self>) {
        if self.knowledge_building {
            self.knowledge_cancel
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.knowledge_status = Some("Stopping knowledge build…".to_owned());
            cx.notify();
        }
    }

    pub fn build_knowledge(&mut self, cx: &mut Context<Self>) {
        if self.knowledge_building {
            self.cancel_knowledge(cx);
            return;
        }
        self.knowledge_building = true;
        self.knowledge_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = Arc::clone(&self.knowledge_cancel);
        self.knowledge_status = Some("Building knowledge…".to_owned());
        cx.notify();

        let db_path = self.service.database_path().to_owned();
        let graph_path = worktable_helix::helix_path_for_sqlite(&db_path);
        let service = Arc::clone(&self.service);
        let can_enrich = self.agent_ready();
        cx.spawn(async move |view, cx| {
            // Phase 1 — mirror entries into the graph on a dedicated thread.
            // `tokio::task::spawn_blocking` cannot be used here: this spawn
            // runs on GPUI's executor, which has no Tokio reactor.
            let local = {
                let local_path = graph_path.clone();
                let local_db = db_path.clone();
                run_blocking(move || {
                    let client = worktable_helix::HelixClient::open_embedded(local_path);
                    client.build_from_sqlite_blocking(&local_db)
                })
                .await
            };
            let synced = match local {
                Ok(Ok(synced)) => synced,
                Ok(Err(error)) => {
                    report_knowledge_status(view, cx, format!("Knowledge build failed: {error}"));
                    return;
                }
                Err(error) => {
                    report_knowledge_status(view, cx, format!("Knowledge build failed: {error}"));
                    return;
                }
            };

            // Phase 2 — AI topics, when a provider is configured.
            let mut enriched = 0usize;
            let mut enrichment_note: Option<String> = None;
            let mut stopped = false;
            if can_enrich {
                loop {
                    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        stopped = true;
                        break;
                    }
                    let batch_path = graph_path.clone();
                    let batch_db = db_path.clone();
                    let batch = run_blocking(move || {
                        worktable_helix::HelixClient::open_embedded(batch_path)
                            .unenriched_entries_blocking(&batch_db, 8)
                    })
                    .await;
                    let batch = match batch {
                        Ok(Ok(batch)) => batch,
                        Ok(Err(error)) => {
                            enrichment_note = Some(format!("AI topics unavailable: {error}"));
                            break;
                        }
                        Err(error) => {
                            enrichment_note = Some(format!("AI topics unavailable: {error}"));
                            break;
                        }
                    };
                    if batch.is_empty() {
                        break;
                    }
                    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        stopped = true;
                        break;
                    }
                    let batch_len = batch.len();
                    let topics = match service.enrich_topics(batch).await {
                        Ok(topics) if !topics.is_empty() => topics,
                        Ok(_) => {
                            enrichment_note = Some(
                                "AI topics returned nothing; keeping keyword topics.".to_owned(),
                            );
                            break;
                        }
                        Err(error) => {
                            enrichment_note = Some(format!("AI topics unavailable: {error}"));
                            break;
                        }
                    };
                    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        stopped = true;
                        break;
                    }
                    let apply_path = graph_path.clone();
                    let _ = run_blocking(move || {
                        let client = worktable_helix::HelixClient::open_embedded(apply_path);
                        for (id, topics) in topics {
                            let _ = client.apply_ai_topics_blocking(&id, topics);
                        }
                    })
                    .await;
                    enriched += batch_len;
                    let _ = view.update(cx, |this, cx| {
                        this.knowledge_status =
                            Some(format!("Naming topics with AI… {enriched} entries so far"));
                        cx.notify();
                    });
                }
            } else {
                enrichment_note = Some("Configure a provider to name topics with AI.".to_owned());
            }

            // Export the graph's topics into the view's cache: grouping and
            // search then use the graph's vocabulary (AI when available).
            let export_path = graph_path.clone();
            let topics = run_blocking(move || {
                worktable_helix::HelixClient::open_embedded(export_path).knowledge_topics_blocking()
            })
            .await
            .unwrap_or_default();
            let _ = view.update(cx, |this, cx| {
                this.apply_knowledge_topics(topics);
                this.knowledge_building = false;
                let summary = if stopped {
                    format!("Knowledge build stopped after {enriched} AI-named entries.")
                } else if enriched > 0 {
                    format!(
                        "Knowledge ready — {synced} new entries synced, {enriched} named by AI."
                    )
                } else if synced == 0 {
                    "Knowledge is up to date.".to_owned()
                } else {
                    format!("Knowledge ready — {synced} new entries linked.")
                };
                this.knowledge_status = Some(match enrichment_note {
                    Some(note) if enriched == 0 && !stopped => format!("{summary} {note}"),
                    _ => summary,
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Load the graph's topics into the view cache (startup and after builds).
    fn load_knowledge_topics(&mut self, cx: &mut Context<Self>) {
        let db_path = self.service.database_path().to_owned();
        cx.spawn(async move |view, cx| {
            let graph_path = worktable_helix::helix_path_for_sqlite(&db_path);
            let topics = run_blocking(move || {
                worktable_helix::HelixClient::open_embedded(graph_path).knowledge_topics_blocking()
            })
            .await
            .unwrap_or_default();
            if !topics.is_empty() {
                let _ = view.update(cx, |this, cx| {
                    this.apply_knowledge_topics(topics);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Record the graph's topics. They layer over the local extraction rather
    /// than replacing the cache, so a late entry-load cannot drop them.
    fn apply_knowledge_topics(&mut self, topics: HashMap<String, String>) {
        for (id, topic) in topics {
            self.graph_topics.insert(id, topic);
        }
    }

    /// Recompute the cached primary topic for every entry. Topic extraction
    /// tokenizes title + content, so running it per entry per frame made the
    /// list stutter while scrolling; grouping reads the cache instead.
    fn rebuild_topic_cache(&mut self) {
        self.topic_cache = self
            .entries
            .iter()
            .map(|entry| (entry.id.clone(), helix_primary_topic(entry)))
            .collect();
    }

    /// Primary topic for `entry`: the knowledge graph's (AI-named when the
    /// graph was built with a provider), else the local extraction, else a
    /// fresh extraction.
    pub(crate) fn primary_topic(&self, entry: &WorktableEntry) -> String {
        self.graph_topics
            .get(&entry.id)
            .or_else(|| self.topic_cache.get(&entry.id))
            .cloned()
            .unwrap_or_else(|| helix_primary_topic(entry))
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
                    // The graph's topic vocabulary (AI-named when available)
                    // is part of search, so a topic finds its entries even
                    // when their wording differs.
                    || self.primary_topic(entry).to_lowercase().contains(&query)
            })
            .collect();
        let ascending = self.sort_ascending;
        match self.sort_mode {
            SortMode::Time => {
                if ascending {
                    filtered.sort_by_key(|entry| entry.created_at);
                } else {
                    filtered.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
                }
            }
            SortMode::Alpha => {
                filtered.sort_by(|a, b| {
                    let a_key = a.title.as_deref().unwrap_or(&a.content).to_lowercase();
                    let b_key = b.title.as_deref().unwrap_or(&b.content).to_lowercase();
                    if ascending {
                        a_key.cmp(&b_key)
                    } else {
                        b_key.cmp(&a_key)
                    }
                });
            }
            SortMode::Topic => {
                // Grouped view still needs a deterministic order: topics
                // alphabetically, then time within each topic. For the flat
                // visible list used for selection, sort by primary topic then
                // time.
                filtered.sort_by(|a, b| {
                    let ta = self.primary_topic(a);
                    let tb = self.primary_topic(b);
                    let topic = if ascending { ta.cmp(&tb) } else { tb.cmp(&ta) };
                    let time = if ascending {
                        a.created_at.cmp(&b.created_at)
                    } else {
                        b.created_at.cmp(&a.created_at)
                    };
                    topic.then_with(|| time)
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
        if self.sort_mode == mode {
            // Clicking the active sort flips its direction.
            self.sort_ascending = !self.sort_ascending;
        } else {
            self.sort_mode = mode;
            self.sort_ascending = mode.default_ascending();
        }
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

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The field only exists on the Entries page; ⌘F from the agent or
        // settings morphs back to it.
        if self.mode != AppMode::Entries {
            self.show_entries(cx);
        }
        let handle = self.search_input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn show_entries(&mut self, cx: &mut Context<Self>) {
        self.show_library_page(AppMode::Entries, cx);
    }

    pub fn show_assistant(&mut self, cx: &mut Context<Self>) {
        self.show_library_page(AppMode::Assistant, cx);
    }

    /// Whether the Entries ⇄ Agent slide is still running (test helper;
    /// `render_library_pages` derives the same window inline for the tween).
    #[cfg(test)]
    pub(crate) fn page_transition_active(&self) -> bool {
        self.page_anim_at
            .is_some_and(|started| started.elapsed() < worktable_ui::PAGE_SLIDE.total())
    }

    /// Switch between the Entries and Agent pages with the transitions.dev
    /// slide + fade (see `render`). Other mode changes are full-page swaps.
    fn show_library_page(&mut self, mode: AppMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.chats_sheet = None;
        self.chat_loading = None;
        self.chats_generation += 1;
        self.page_anim_at = if matches!(self.mode, AppMode::Entries | AppMode::Assistant)
            && matches!(mode, AppMode::Entries | AppMode::Assistant)
        {
            Some(Instant::now())
        } else {
            None
        };
        self.mode = mode;
        cx.notify();
    }

    pub fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.chats_sheet = None;
        self.chat_loading = None;
        self.chats_generation += 1;
        self.mode = AppMode::Settings;
        self.settings_tab = None;
        if self.providers.is_empty() {
            self.refresh_providers(cx);
        }
        cx.notify();
    }

    /// Show Settings opened on one of its category pages.
    pub fn show_settings_at(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        self.chats_sheet = None;
        self.chat_loading = None;
        self.chats_generation += 1;
        self.mode = AppMode::Settings;
        self.settings_tab = Some(tab);
        if self.providers.is_empty() {
            self.refresh_providers(cx);
        }
        cx.notify();
    }

    /// Open the modal configuration dialog for one provider. The dialog is the
    /// only place keys and OAuth sign-ins are entered, so the provider list
    /// stays scannable.
    pub fn open_provider_dialog(
        &mut self,
        provider_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
        else {
            return;
        };

        self.api_key_provider = Some(provider_id.to_owned());
        self.provider_dialog_status = None;
        let input = self.api_key_input.clone();
        self.set_input(&input, "", cx);

        let main = cx.entity();
        let dialog_view = cx.new(|_| ProviderDialogView {
            main: main.downgrade(),
            provider_id: provider_id.to_owned(),
            api_key_input: self.api_key_input.clone(),
            opened_at: Instant::now(),
        });
        let title = provider.name.clone();
        window.open_dialog(cx, move |dialog, window, _cx| {
            dialog
                .title(format!("Configure {title}"))
                .width(dialog_width(window, design::PROVIDER_DIALOG_WIDTH))
                .child(dialog_view.clone())
                .footer(
                    DialogFooter::new().child(
                        Button::new("provider-dialog-close")
                            .label("Close")
                            .ghost()
                            .debug_selector(|| "provider-dialog-close".into())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
        cx.notify();
    }

    pub fn cancel_composer(&mut self, cx: &mut Context<Self>) {
        if self.entry_modal.is_some() {
            self.close_entry_modal(cx);
        }
    }

    /// Show the first-run tour (first launch, or Settings → Appearance).
    pub fn start_onboarding(&mut self, cx: &mut Context<Self>) {
        self.onboarding = Some(OnboardingState::new());
        cx.notify();
    }

    /// Advance the tour: Welcome → Accessibility → Provider → done.
    pub fn advance_onboarding(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.onboarding.as_mut() else {
            return;
        };
        match state.step {
            OnboardingStep::Welcome => {
                state.step = OnboardingStep::Accessibility;
                state.accessibility_trusted = accessibility_trusted();
            }
            OnboardingStep::Accessibility => {
                state.step = OnboardingStep::Provider;
            }
            OnboardingStep::Provider => {
                self.complete_onboarding(cx);
                return;
            }
        }
        cx.notify();
    }

    /// Step back one page.
    pub fn back_onboarding(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.onboarding.as_mut() else {
            return;
        };
        state.step = match state.step {
            OnboardingStep::Welcome => OnboardingStep::Welcome,
            OnboardingStep::Accessibility => OnboardingStep::Welcome,
            OnboardingStep::Provider => OnboardingStep::Accessibility,
        };
        state.accessibility_trusted = accessibility_trusted();
        cx.notify();
    }

    /// Dismiss the tour and remember the choice.
    pub fn skip_onboarding(&mut self, cx: &mut Context<Self>) {
        self.complete_onboarding(cx);
    }

    /// Re-read the Accessibility permission (the user grants it in System
    /// Settings, outside the app).
    pub fn refresh_accessibility(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.onboarding.as_mut() {
            state.accessibility_trusted = accessibility_trusted();
        }
        cx.notify();
    }

    /// Jump straight into the provider settings from the tour.
    pub fn setup_provider_from_onboarding(&mut self, cx: &mut Context<Self>) {
        self.complete_onboarding(cx);
        self.show_settings_at(SettingsTab::Providers, cx);
    }

    fn complete_onboarding(&mut self, cx: &mut Context<Self>) {
        self.onboarding = None;
        let service = Arc::clone(&self.service);
        cx.spawn(async move |_view, _cx| {
            let _ = service.set_config("onboarding_completed", "1").await;
        })
        .detach();
        cx.notify();
    }

    /// Load UI preferences persisted in config.
    fn load_preferences(&mut self, window: &Window, cx: &mut Context<Self>) {
        // System is the default; a stored preference overrides it.
        self.apply_theme(window, cx);
        let service = Arc::clone(&self.service);
        let appearance = window.appearance();
        cx.spawn(async move |view, cx| {
            let show_thinking = service.get_config("show_thinking").await;
            let theme_mode = service.get_config("theme_mode").await;
            let onboarding_done = service.get_config("onboarding_completed").await;
            let background_on_close = service
                .get_config(crate::preferences::BACKGROUND_ON_CLOSE_KEY)
                .await;
            let _ = view.update(cx, |this, cx| {
                if let Ok(Some(value)) = show_thinking {
                    this.show_thinking = value == "1";
                }
                if let Ok(Some(value)) = background_on_close {
                    this.background_on_close = value != "0";
                    crate::preferences::set_background_on_close(this.background_on_close);
                }
                // First launch: show the tour. Tests and the visual runner
                // seed this as "1" so they open straight into the app.
                if !matches!(onboarding_done, Ok(Some(ref value)) if value == "1") {
                    this.onboarding = Some(OnboardingState::new());
                }
                if let Ok(Some(value)) = theme_mode
                    && let Some(mode) = AppThemeMode::from_config(&value)
                {
                    this.theme_mode = mode;
                    match mode {
                        AppThemeMode::Light => {
                            gpui_component::Theme::change(
                                gpui_component::ThemeMode::Light,
                                None,
                                cx,
                            );
                        }
                        AppThemeMode::Dark => {
                            gpui_component::Theme::change(
                                gpui_component::ThemeMode::Dark,
                                None,
                                cx,
                            );
                        }
                        AppThemeMode::System => {
                            let mode = if matches!(
                                appearance,
                                gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
                            ) {
                                gpui_component::ThemeMode::Dark
                            } else {
                                gpui_component::ThemeMode::Light
                            };
                            gpui_component::Theme::change(mode, None, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Resolve and apply the current theme mode. `System` follows the
    /// window's platform appearance at apply time.
    fn apply_theme(&self, window: &Window, cx: &mut Context<Self>) {
        let mode = match self.theme_mode {
            AppThemeMode::Light => gpui_component::ThemeMode::Light,
            AppThemeMode::Dark => gpui_component::ThemeMode::Dark,
            AppThemeMode::System => {
                if matches!(
                    window.appearance(),
                    gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
                ) {
                    gpui_component::ThemeMode::Dark
                } else {
                    gpui_component::ThemeMode::Light
                }
            }
        };
        gpui_component::Theme::change(mode, None, cx);
    }

    /// Select a theme mode, apply it, and persist it.
    pub fn set_theme_mode(
        &mut self,
        mode: AppThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.theme_mode = mode;
        self.apply_theme(window, cx);
        let value = mode.as_config().to_owned();
        let service = Arc::clone(&self.service);
        cx.spawn(async move |_view, _cx| {
            let _ = service.set_config("theme_mode", &value).await;
        })
        .detach();
        cx.notify();
    }

    /// Flip the background-on-close preference and persist it. The window
    /// close handler reads the mirrored flag synchronously.
    pub fn toggle_background_on_close(&mut self, cx: &mut Context<Self>) {
        self.background_on_close = !self.background_on_close;
        crate::preferences::set_background_on_close(self.background_on_close);
        let value = if self.background_on_close { "1" } else { "0" }.to_owned();
        let service = Arc::clone(&self.service);
        cx.spawn(async move |_view, _cx| {
            let _ = service
                .set_config(crate::preferences::BACKGROUND_ON_CLOSE_KEY, &value)
                .await;
        })
        .detach();
        cx.notify();
    }

    /// Flip the "show thinking" preference and persist it.
    pub fn toggle_show_thinking(&mut self, cx: &mut Context<Self>) {
        self.show_thinking = !self.show_thinking;
        let value = if self.show_thinking { "1" } else { "0" }.to_owned();
        let service = Arc::clone(&self.service);
        cx.spawn(async move |_view, _cx| {
            let _ = service.set_config("show_thinking", &value).await;
        })
        .detach();
        cx.notify();
    }

    pub fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.theme_mode = match self.theme_mode {
            AppThemeMode::Dark => AppThemeMode::Light,
            _ => AppThemeMode::Dark,
        };
        let mode = match self.theme_mode {
            AppThemeMode::Dark => gpui_component::ThemeMode::Dark,
            _ => gpui_component::ThemeMode::Light,
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

    /// List keyboard commands only apply on the Library page with no detail
    /// overlay; otherwise Enter while prompting the agent (or a detail view)
    /// could open or delete an entry the user cannot even see.
    fn list_actions_allowed(&self) -> bool {
        self.mode == AppMode::Entries && self.entry_modal.is_none() && self.onboarding.is_none()
    }

    /// Whether one of the view's text fields currently owns the keyboard.
    /// While typing, Enter adds the note (or sends the prompt) and Backspace
    /// edits the field — neither may reach the list commands behind it.
    fn text_input_focused(&self, window: &Window, cx: &App) -> bool {
        let Some(focused) = window.focused(cx) else {
            return false;
        };
        [
            self.search_input.read(cx).focus_handle(cx),
            self.composer_body.read(cx).focus_handle(cx),
            self.assistant_input.read(cx).focus_handle(cx),
        ]
        .into_iter()
        .any(|handle| handle == focused)
    }

    pub fn move_selection(&mut self, direction: isize) {
        if !self.list_actions_allowed() {
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

    /// Delete the entry the context menu was opened on. When it belongs to a
    /// multi-selection, the whole selection goes; otherwise the clicked entry
    /// becomes the selection and is deleted alone.
    pub(crate) fn delete_context_target(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.selected.contains(&id) {
            self.selected.clear();
            self.selected.insert(id.clone());
            self.selected_anchor = Some(id);
        }
        self.delete_selected(cx);
    }

    pub fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if !self.list_actions_allowed() || self.selected.is_empty() {
            return;
        }
        let ids: Vec<String> = self.selected.iter().cloned().collect();
        self.entries.retain(|entry| !ids.contains(&entry.id));
        self.rebuild_topic_cache();
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
        if !self.list_actions_allowed() {
            return;
        }
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
        if !self.list_actions_allowed() {
            return;
        }
        if let Some(entry) = self.selected_entry()
            && content_is_link(&entry.content)
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(entry.content.clone()));
        }
    }

    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        // The detail view is the single destination for Enter/Open; links
        // inside it get their own chip.
        if !self.list_actions_allowed() {
            return;
        }
        if let Some(id) = self.selected_entry().map(|entry| entry.id.clone()) {
            let origin = self.entry_origin(&id);
            self.open_entry_modal(&id, origin, cx);
        }
    }

    /// Convert a window-space point/rect into the content column's local
    /// space (the overlay's coordinate system).
    fn local_point(&self, position: Point<Pixels>) -> Point<Pixels> {
        position - self.content_origin.get()
    }

    fn local_bounds(&self, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
        Bounds::new(self.local_point(bounds.origin), bounds.size)
    }

    /// A small morph origin centered on a window-space pointer position.
    pub(crate) fn pointer_origin(&self, position: Point<Pixels>) -> Bounds<Pixels> {
        morph_origin_at(self.local_point(position))
    }

    /// The morph's starting rect: the entry's card when it is mounted (its
    /// live prepainted rect, converted to column space), else a small square
    /// at the last pointer position.
    pub(crate) fn entry_origin(&self, id: &str) -> Bounds<Pixels> {
        if let Some(bounds) = self.card_bounds.borrow().get(id) {
            return self.local_bounds(*bounds);
        }
        morph_origin_at(self.local_point(self.pointer_position))
    }

    /// Open the full-content view for `id`, morphing from `origin`.
    pub fn open_entry_modal(&mut self, id: &str, origin: Bounds<Pixels>, cx: &mut Context<Self>) {
        // One detail at a time: a force touch on a card outside the open
        // panel (the overlay does not cover the whole list for pressure
        // events) must not restart the morph in a loop.
        if self.entry_modal.is_some() {
            return;
        }
        if self.entries.iter().all(|entry| entry.id != id) {
            return;
        }
        self.entry_editing = false;
        self.entry_edit_status = None;
        self.entry_modal = Some(EntryModalState {
            entry_id: id.to_owned(),
            origin,
            opened_at: Instant::now(),
            closing_at: None,
        });
        cx.notify();
    }

    /// Switch the detail view into the markdown editor.
    pub fn start_entry_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .entry_modal
            .as_ref()
            .map(|modal| modal.entry_id.clone())
        else {
            return;
        };
        let Some(content) = self
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.content.clone())
        else {
            return;
        };
        let input = self.entry_edit_input.clone();
        input.update(cx, |state, cx| state.set_value(&content, window, cx));
        let handle = input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.entry_editing = true;
        self.entry_edit_status = None;
        cx.notify();
    }

    /// Leave the editor without saving.
    pub fn cancel_entry_edit(&mut self, cx: &mut Context<Self>) {
        self.entry_editing = false;
        self.entry_edit_status = None;
        cx.notify();
    }

    /// Persist the editor's markdown and keep the detail view open.
    pub fn save_entry_edit(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .entry_modal
            .as_ref()
            .map(|modal| modal.entry_id.clone())
        else {
            return;
        };
        let Some(mut updated) = self.entries.iter().find(|entry| entry.id == id).cloned() else {
            return;
        };
        let content = self.entry_edit_input.read(cx).value().to_string();
        if content == updated.content {
            self.entry_editing = false;
            self.entry_edit_status = None;
            cx.notify();
            return;
        }
        updated.content = content;

        let service = Arc::clone(&self.service);
        let saved = updated.clone();
        cx.spawn(async move |view, cx| {
            let result = service.update_entry(saved.clone()).await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if let Some(entry) =
                            this.entries.iter_mut().find(|entry| entry.id == saved.id)
                        {
                            *entry = saved.clone();
                        }
                        // The graph topic for this entry is stale; let the
                        // local extraction (or the next build) take over.
                        this.graph_topics.remove(&saved.id);
                        this.rebuild_topic_cache();
                        this.entry_editing = false;
                        this.entry_edit_status = None;
                    }
                    Err(error) => {
                        this.entry_edit_status = Some(format!("Couldn't save the entry: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Play the close morph, then unmount the panel.
    pub fn close_entry_modal(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = self.entry_modal.as_mut() else {
            return;
        };
        if modal.closing_at.is_some() {
            return;
        }
        modal.closing_at = Some(Instant::now());
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(worktable_ui::MORPH_CLOSE.total())
                .await;
            let _ = view.update(cx, |this, cx| {
                this.entry_modal = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
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
        let anchor = self.selected_anchor.clone().unwrap_or_else(|| id.clone());
        let anchor_idx = visible_ids.iter().position(|x| x == &anchor);
        let target_idx = visible_ids.iter().position(|x| x == &id);
        if let (Some(a), Some(b)) = (anchor_idx, target_idx) {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            self.selected.clear();
            for id in visible_ids.iter().take(end + 1).skip(start) {
                self.selected.insert(id.clone());
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
        let session_id = match event {
            WorktableEvent::AiRunStarted { session_id, .. }
            | WorktableEvent::AiThoughtDelta { session_id, .. }
            | WorktableEvent::AiMessageDelta { session_id, .. }
            | WorktableEvent::AiToolStarted { session_id, .. }
            | WorktableEvent::AiToolFinished { session_id, .. }
            | WorktableEvent::AiCitations { session_id, .. }
            | WorktableEvent::AiRunFinished { session_id, .. }
            | WorktableEvent::AiRunFailed { session_id, .. } => Some(session_id),
            _ => None,
        };
        if self
            .chat_id
            .as_ref()
            .zip(session_id)
            .is_some_and(|(active, incoming)| active != incoming)
        {
            return;
        }
        match event {
            WorktableEvent::AiRunStarted { .. } => {
                self.assistant_busy = true;
                cx.notify();
            }
            WorktableEvent::AiThoughtDelta { delta, .. } => {
                self.assistant_scroll.scroll_to_bottom();
                match self.messages.last_mut().filter(|m| m.streaming) {
                    Some(message) => message.thinking.push_str(delta),
                    None => self.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        text: String::new(),
                        thinking: delta.clone(),
                        streaming: true,
                        citations: Vec::new(),
                        thinking_collapsed: false,
                    }),
                }
                cx.notify();
            }
            WorktableEvent::AiMessageDelta { delta, .. } => {
                self.assistant_scroll.scroll_to_bottom();
                match self.messages.last_mut().filter(|m| m.streaming) {
                    Some(message) => {
                        message.text.push_str(delta);
                        // Text is arriving: the tool row has done its job.
                        self.active_tool = None;
                    }
                    None => {
                        let citations = citation_refs(std::mem::take(&mut self.pending_citations));
                        self.messages.push(ChatMessage {
                            role: ChatRole::Assistant,
                            text: delta.clone(),
                            thinking: String::new(),
                            streaming: true,
                            citations,
                            thinking_collapsed: false,
                        });
                        self.active_tool = None;
                    }
                }
                cx.notify();
            }
            WorktableEvent::AiToolStarted { name, .. } => {
                self.active_tool = Some(name.clone());
                cx.notify();
            }
            WorktableEvent::AiToolFinished { .. } => {
                self.active_tool = None;
                cx.notify();
            }
            WorktableEvent::AiCitations { citations, .. } => {
                // The tool's citations are emitted when the run's stream ends,
                // after the answer deltas — so the streaming message already
                // exists. Attach them there; without this the markers render
                // as literal `[n]` brackets and the sources are lost.
                let refs = citation_refs(citations.clone());
                match self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.streaming && message.role == ChatRole::Assistant)
                {
                    Some(message) => {
                        message.citations = refs;
                        self.pending_citations.clear();
                    }
                    None => {
                        // No answer yet (a tool-only turn): hold them for the
                        // first delta.
                        self.pending_citations = citations.to_vec();
                    }
                }
                cx.notify();
            }
            WorktableEvent::AiRunFinished { .. } => {
                self.assistant_busy = false;
                self.active_tool = None;
                for message in self.messages.iter_mut().rev() {
                    if message.streaming {
                        message.streaming = false;
                        break;
                    }
                }
                // The answer is in: fold the reasoning away by default.
                for message in self.messages.iter_mut() {
                    if !message.thinking.is_empty() {
                        message.thinking_collapsed = true;
                    }
                }
                // Defensive: if citations arrived before any answer message
                // existed, land them on the last answer rather than dropping.
                let pending = std::mem::take(&mut self.pending_citations);
                if !pending.is_empty() {
                    let refs = citation_refs(pending);
                    if let Some(message) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|message| message.role == ChatRole::Assistant)
                        && message.citations.is_empty()
                    {
                        message.citations = refs;
                    }
                }
                self.save_current_chat(cx);
                cx.notify();
            }
            WorktableEvent::AiRunFailed { error, .. } | WorktableEvent::AiWorkerError { error } => {
                self.providers_loading = false;
                self.settings_status = Some(error.clone());
                self.assistant_busy = false;
                self.active_tool = None;
                self.pending_citations.clear();
                for message in self.messages.iter_mut().rev() {
                    if message.streaming {
                        message.streaming = false;
                        break;
                    }
                }
                for message in self.messages.iter_mut() {
                    if !message.thinking.is_empty() {
                        message.thinking_collapsed = true;
                    }
                }
                if !self.messages.iter().any(|m| m.text.contains(error)) {
                    self.messages
                        .push(ChatMessage::assistant(format!("⚠ {error}")));
                }
                self.save_current_chat(cx);
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
                    self.provider_dialog_status = Some(format!("Signed in to {provider_id}."));
                } else {
                    self.provider_dialog_status = Some(format!(
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
            && !self.providers.iter().any(|p| &p.id == provider)
        {
            self.api_key_provider = None;
        }
        // A stored active provider the catalog no longer knows (e.g. saved by
        // an older build, then the provider was removed upstream) can never
        // serve a prompt. Clear it and say so — the assistant shows its setup
        // CTA until a valid provider is chosen again.
        if let Some(provider) = self.active_provider.clone()
            && !provider.is_empty()
            && !self.providers.iter().any(|p| p.id == provider)
        {
            self.active_provider = None;
            self.active_model = None;
            self.stale_provider_cleared = true;
            self.settings_status = Some(format!(
                "'{provider}' is no longer an available provider — pick a new one under Providers."
            ));
        }
    }

    /// The agent can serve prompts only when the worker runs AND a provider
    /// from the current catalog is active with a model selected. Anything
    /// less and the composer turns into a setup call-to-action instead of a
    /// dead input.
    fn agent_ready(&self) -> bool {
        self.service.has_ai_runtime()
            && self
                .active_provider
                .as_deref()
                .is_some_and(|id| !id.is_empty() && self.providers.iter().any(|p| p.id == id))
            && self
                .active_model
                .as_deref()
                .is_some_and(|model| !model.is_empty())
    }

    fn chat_switch_allowed(&self) -> bool {
        !self.assistant_busy
            && !self.chat_save_pending
            && self.chat_save_error.is_none()
            && self.chat_loading.is_none()
    }

    pub(crate) fn open_chats(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .chats_sheet
            .as_ref()
            .is_some_and(|sheet| sheet.closing_at.is_none())
        {
            self.close_chats(window, cx);
            return;
        }
        self.chats_sheet = Some(ChatSheet {
            opened_at: Instant::now(),
            closing_at: None,
            return_focus: window.focused(cx),
        });
        self.chats_focus.focus(window, cx);
        self.refresh_chats(cx);
        cx.notify();
    }

    fn close_chats(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = self.chats_sheet.as_mut() else {
            return;
        };
        if sheet.closing_at.is_some() {
            return;
        }
        sheet.closing_at = Some(Instant::now());
        if let Some(focus) = sheet.return_focus.as_ref() {
            focus.focus(window, cx);
        }
        self.chats_generation += 1;
        self.chat_loading = None;
        let generation = self.chats_generation;
        let duration = if worktable_ui::reduced_motion(cx) {
            Duration::ZERO
        } else {
            worktable_ui::MODAL_CLOSE.total()
        };
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(duration).await;
            let _ = view.update(cx, |this, cx| {
                if this.chats_generation == generation {
                    this.chats_sheet = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh_chats(&mut self, cx: &mut Context<Self>) {
        self.chats_generation += 1;
        let generation = self.chats_generation;
        self.chats_state = ChatListState::Loading;
        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.list_chats().await;
            let _ = view.update(cx, |this, cx| {
                if this.chats_generation != generation {
                    return;
                }
                match result {
                    Ok(chats) => {
                        this.chats = chats
                            .into_iter()
                            .map(|chat| ChatRow {
                                id: chat.id,
                                title: chat.title.into(),
                                updated_at: chat.updated_at,
                            })
                            .collect();
                        this.chats_state = ChatListState::Ready;
                    }
                    Err(error) => {
                        this.chats_state = ChatListState::Failed(
                            format!("Couldn't load chats: {error}. Try again.").into(),
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_current_chat(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.chat_id.clone() else {
            return;
        };
        let Some(first_prompt) = self
            .messages
            .iter()
            .find(|message| message.role == ChatRole::User)
        else {
            return;
        };
        let title = first_prompt
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let title = if title.chars().count() > 64 {
            format!("{}…", title.chars().take(64).collect::<String>())
        } else {
            title
        };
        let messages_json = match serde_json::to_string(&self.messages) {
            Ok(json) => json,
            Err(error) => {
                self.chat_save_error =
                    Some(format!("Couldn't save this chat: {error}. Try again.").into());
                cx.notify();
                return;
            }
        };
        self.chat_revision += 1;
        let revision = self.chat_revision;
        self.chat_save_pending = true;
        self.chat_save_error = None;
        let chat = worktable_db::StoredChat::new(
            worktable_db::ChatSummary::new(
                id.clone(),
                title,
                crate::service::unix_time_ms(),
                revision,
            ),
            messages_json,
        );
        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let result = service.save_chat(chat).await;
            let _ = view.update(cx, |this, cx| {
                if this.chat_id.as_ref() != Some(&id) || this.chat_revision != revision {
                    return;
                }
                this.chat_save_pending = false;
                if let Err(error) = result {
                    this.chat_save_error = Some(
                        format!(
                            "Couldn't save this chat: {error}. Try again before switching chats."
                        )
                        .into(),
                    );
                } else if this
                    .chats_sheet
                    .as_ref()
                    .is_some_and(|sheet| sheet.closing_at.is_none())
                {
                    this.refresh_chats(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat_switch_allowed() {
            return;
        }
        self.close_chats(window, cx);
        self.chat_id = None;
        self.chat_revision = 0;
        self.messages.clear();
        self.active_tool = None;
        self.pending_citations.clear();
        let input = self.assistant_input.clone();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.assistant_scroll = ScrollHandle::new();
        cx.notify();
    }

    fn load_chat(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chat_switch_allowed() {
            return;
        }
        self.chats_generation += 1;
        let generation = self.chats_generation;
        self.chat_loading = Some(id.clone());
        let service = Arc::clone(&self.service);
        cx.spawn_in(window, async move |view, cx| {
            let result = service.load_chat(&id).await.and_then(|chat| {
                let chat = chat.ok_or_else(|| "This chat is no longer available.".to_owned())?;
                let messages = serde_json::from_str::<Vec<ChatMessage>>(&chat.messages_json)
                    .map_err(|error| format!("Couldn't read this chat: {error}. Try again."))?;
                Ok((chat.summary, messages))
            });
            let _ = view.update_in(cx, |this, window, cx| {
                if this.chats_generation != generation {
                    return;
                }
                this.chat_loading = None;
                match result {
                    Ok((summary, mut messages)) => {
                        for message in &mut messages {
                            message.thinking_collapsed = true;
                        }
                        this.chat_id = Some(summary.id);
                        this.chat_revision = summary.revision;
                        this.messages = messages;
                        this.active_tool = None;
                        this.pending_citations.clear();
                        this.assistant_scroll = ScrollHandle::new();
                        this.assistant_scroll.scroll_to_bottom();
                        this.close_chats(window, cx);
                        let input = this.assistant_input.clone();
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                        input.read(cx).focus_handle(cx).focus(window, cx);
                    }
                    Err(error) => this.chats_state = ChatListState::Failed(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Abort the in-flight assistant run (the orb button while busy).
    pub fn cancel_assistant(&mut self, cx: &mut Context<Self>) {
        if !self.assistant_busy {
            return;
        }
        let service = Arc::clone(&self.service);
        let session_id = self.chat_id.clone().unwrap_or_default();
        cx.spawn(async move |_view, _cx| {
            let _ = service.cancel_prompt(&session_id).await;
        })
        .detach();
    }

    pub fn send_assistant(&mut self, cx: &mut Context<Self>) {
        if self.assistant_busy || self.chat_loading.is_some() {
            return;
        }
        let text = self.assistant_input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let input = self.assistant_input.clone();
        self.set_input(&input, "", cx);
        self.submit_assistant_prompt(text, cx);
    }

    /// Send the library search query to the agent. Tab from the search field
    /// reaches the Ask agent button; Enter switches to Agent mode and runs
    /// the query as the prompt.
    pub fn ask_agent_from_search(&mut self, cx: &mut Context<Self>) {
        let query = self.query.trim().to_owned();
        self.show_assistant(cx);
        if query.is_empty() {
            return;
        }
        self.clear_search(cx);
        self.submit_assistant_prompt(query, cx);
    }

    /// Submit one prompt: always shows the user's message, then either runs it
    /// or explains the (missing) provider setup.
    pub fn submit_assistant_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        if self.assistant_busy || self.chat_loading.is_some() {
            return;
        }
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let session_id = self
            .chat_id
            .get_or_insert_with(crate::service::new_entry_id)
            .clone();
        self.messages.push(ChatMessage::user(text.clone()));
        if !self.agent_ready() {
            self.messages.push(ChatMessage::assistant(
                "The AI assistant isn't configured yet — pick a provider, add an API key, and choose a model in Settings.",
            ));
            self.save_current_chat(cx);
            cx.notify();
            return;
        }
        self.assistant_busy = true;
        self.active_tool = None;
        self.pending_citations.clear();
        self.save_current_chat(cx);
        cx.notify();

        let service = Arc::clone(&self.service);
        let request_id = crate::service::new_entry_id();
        cx.spawn(async move |view, cx| {
            let result = service
                .submit_prompt(&request_id, &session_id, &text)
                .await;
            match result {
                Err(error) => {
                    let _ = view.update(cx, |this, cx| {
                        if this.chat_id.as_ref() != Some(&session_id) { return; }
                        this.assistant_busy = false;
                        if !this.messages.iter().any(|m| m.text.contains(&error)) {
                            this.messages
                                .push(ChatMessage::assistant(format!("⚠ {error}")));
                        }
                        this.save_current_chat(cx);
                        cx.notify();
                    });
                }
                // `None` = the session lease was not granted (another run is
                // still active). Without this arm the composer stays busy
                // forever with no feedback — the prompt silently vanishes.
                Ok(None) => {
                    let _ = view.update(cx, |this, cx| {
                        if this.chat_id.as_ref() != Some(&session_id) { return; }
                        this.assistant_busy = false;
                        this.messages.push(ChatMessage::assistant(
                            "⚠ Another prompt is still running for this session. Please wait for it to finish and try again.",
                        ));
                        this.save_current_chat(cx);
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
                        this.provider_dialog_status =
                            Some(format!("Saved API key for {provider_id}."));
                    }
                    Err(error) => {
                        this.provider_dialog_status =
                            Some(format!("Failed to save API key: {error}"));
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
        self.provider_dialog_status = Some(format!("Starting sign-in for {provider_id}…"));
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
                    this.provider_dialog_status = Some(format!("Failed to start sign-in: {error}"));
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
                this.provider_dialog_status = Some("Sign-in cancelled.".to_owned());
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
                        this.provider_dialog_status = Some(format!("Signed out of {provider_id}."));
                    }
                    Err(error) => {
                        this.provider_dialog_status = Some(format!("Failed to sign out: {error}"));
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
                    this.provider_dialog_status = Some(format!("Failed to answer prompt: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.queue_composer_entry(cx) {
            return;
        }
        let input = self.composer_body.clone();
        input.update(cx, |state, cx| state.set_value("", window, cx));
    }

    /// Queue the note in the bar for insertion. Returns false when it is empty.
    fn queue_composer_entry(&mut self, cx: &mut Context<Self>) -> bool {
        let body = trim_opt(self.composer_body.read(cx).value().as_ref());
        let Some(content) = body else {
            return false;
        };

        let entry = WorktableEntry {
            id: crate::service::new_entry_id(),
            content,
            title: None,
            source: "Worktable".to_owned(),
            created_at: crate::service::unix_time_ms(),
        };

        cx.notify();

        let service = Arc::clone(&self.service);
        let entry_id_for_anim = entry.id.clone();
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.rebuild_topic_cache();
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
        true
    }

    /// Focus the always-visible note input. Test/visual-runner helper: the
    /// bar is focused by clicking it in the running app, so this only exists
    /// for the hermetic test and capture targets.
    #[cfg(any(test, feature = "visual-tests"))]
    #[allow(dead_code)] // used by the visual runner target, not the main bin
    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.composer_body.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        cx.notify();
    }

    /// Open the system image picker; chosen images become image entries.
    pub fn pick_image(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose an image".into()),
        });
        cx.spawn(async move |view, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            for path in paths {
                let Some(mime) = image_mime_for(&path) else {
                    continue;
                };
                let mime = mime.to_owned();
                let path = path.to_string_lossy().into_owned();
                let _ = view.update(cx, |this, cx| {
                    this.add_captured_image(path.clone(), mime.clone(), cx);
                });
            }
        })
        .detach();
    }

    /// Save an image entry through the system save panel. Local files are
    /// copied; remote images are fetched first.
    pub fn save_image(&mut self, content: String, cx: &mut Context<Self>) {
        let trimmed = content.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        let is_remote = trimmed.starts_with("http://") || trimmed.starts_with("https://");
        let local = std::path::PathBuf::from(&trimmed);
        let (directory, suggested) = if is_remote {
            (std::env::temp_dir(), None)
        } else {
            (
                local
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(std::env::temp_dir),
                local
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
            )
        };
        let receiver = cx.prompt_for_new_path(&directory, suggested.as_deref());
        cx.spawn(async move |view, cx| {
            let Ok(Ok(Some(target))) = receiver.await else {
                return;
            };
            let result = if is_remote {
                match reqwest::get(&trimmed).await {
                    Ok(response) => match response.bytes().await {
                        Ok(bytes) => std::fs::write(&target, &bytes),
                        Err(error) => Err(std::io::Error::other(error)),
                    },
                    Err(error) => Err(std::io::Error::other(error)),
                }
            } else {
                std::fs::copy(&trimmed, &target).map(|_| ())
            };
            let _ = view.update(cx, |this, cx| {
                this.entry_edit_status = Some(match result {
                    Ok(()) => "Image saved".to_owned(),
                    Err(error) => format!("Couldn't save the image: {error}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Open an image in a borderless fullscreen popup above every other app,
    /// fading it in with the modal motion curve.
    pub fn open_image_viewer(
        &mut self,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = image_source_for(&content) else {
            return;
        };
        let bounds = window
            .display(cx)
            .map(|display| display.bounds())
            .unwrap_or_else(|| {
                Bounds::new(
                    point(Pixels::ZERO, Pixels::ZERO),
                    size(
                        design::to_pixels(rems(80.0), window),
                        design::to_pixels(rems(50.0), window),
                    ),
                )
            });
        let label: SharedString = content
            .rsplit('/')
            .next()
            .unwrap_or(&content)
            .to_owned()
            .into();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            focus: true,
            show: true,
            ..WindowOptions::default()
        };
        if let Err(error) = cx.open_window(options, move |window, cx| {
            let viewer = cx.new(|cx| ImageViewer::new(source, label, cx));
            let handle = viewer.read(cx).focus_handle.clone();
            handle.focus(window, cx);
            viewer
        }) {
            self.entry_edit_status = Some(format!("Couldn't open the image: {error}"));
            cx.notify();
        }
    }

    /// Insert text captured from another application without opening the window.
    pub fn add_captured_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(content) = trim_opt(&text) else {
            return;
        };

        let entry = WorktableEntry {
            id: crate::service::new_entry_id(),
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
                    this.rebuild_topic_cache();
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

    /// Insert an image captured from another application. The file is
    /// hard-linked into the internal media library first, so the entry keeps
    /// working when the original moves away.
    pub fn add_captured_image(&mut self, path: String, mime_type: String, cx: &mut Context<Self>) {
        let path = path.trim().to_owned();
        if path.is_empty() {
            return;
        }
        // `mime_type` is kept for display purposes but the DB stores the file path as content.
        let _ = mime_type;
        let content = self
            .service
            .import_image(std::path::Path::new(&path))
            .map(|stored| stored.to_string_lossy().into_owned())
            .unwrap_or(path);
        let entry = WorktableEntry {
            id: crate::service::new_entry_id(),
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
                    this.rebuild_topic_cache();
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
        if let Some(start) = self.splash_start
            && start.elapsed() > Duration::from_millis(650)
        {
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
            .key_context(if self.list_actions_allowed() {
                "worktable-list"
            } else {
                "worktable"
            })
            .on_action(
                cx.listener(|this, _: &crate::actions::SelectPrevious, window, cx| {
                    if !this.text_input_focused(window, cx) {
                        this.move_selection(-1)
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::SelectNext, window, cx| {
                    if !this.text_input_focused(window, cx) {
                        this.move_selection(1)
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::DeleteEntry, window, cx| {
                    if !this.text_input_focused(window, cx) {
                        this.delete_selected(cx)
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::OpenEntry, window, cx| {
                    if !this.text_input_focused(window, cx) {
                        this.open_selected(cx)
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CopyEntry, window, cx| {
                    if !this.text_input_focused(window, cx) {
                        this.copy_selected(cx)
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CopyLink, window, cx| {
                    if !this.text_input_focused(window, cx) {
                        this.copy_selected_link(cx)
                    }
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
                cx.listener(|this, _: &crate::actions::CancelComposer, window, cx| {
                    if this.chats_sheet.is_some() {
                        this.close_chats(window, cx);
                    } else {
                        this.cancel_composer(cx);
                        this.focus_handle.focus(window, cx);
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::SubmitComposer, window, cx| {
                    this.submit_composer(window, cx)
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
                    .pt(design::TITLEBAR_INSET)
                    // Cap the reading column on very wide windows so content
                    // stays comfortable; centered.
                    .justify_center()
                    .child(render_main(self, window, cx)),
            )
            .children(render_chats_sheet(self, window, cx))
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
                                    img(crate::assets::LOGO_PATH)
                                        .size(rems(4.0))
                                        .rounded(theme.radius_tokens().lg),
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
    // - GithubStars: a full-page replacement with a back button, reached from
    //   Settings. AI providers configure through a modal dialog instead.
    let theme = cx.theme().clone();
    // Panes never exceed the content column, or the hidden pane would peek
    // out beside the active one. The column is responsive: full width on
    // narrow windows, wider on large displays.
    let pane_w = design::content_column_width(window);

    let header: gpui::AnyElement = match this.mode {
        AppMode::Entries | AppMode::Assistant => render_library_header(this, pane_w, window, cx),
        AppMode::Settings => settings_header(this, cx),
    };

    let body: gpui::AnyElement = match this.mode {
        AppMode::Entries | AppMode::Assistant => {
            // Build both panes sequentially (separate mutable borrows).
            let entries_pane = render_entries_pane(this, window, cx).into_any_element();
            let assistant_pane = render_assistant(this, cx).into_any_element();
            v_flex()
                .id("slide-shell")
                .flex_1()
                .min_h_0()
                .w_full()
                // The button group is fixed; only the pages below switch.
                .child(render_library_pages(
                    this,
                    pane_w,
                    entries_pane,
                    assistant_pane,
                    window,
                    cx,
                ))
                .into_any_element()
        }
        AppMode::Settings => render_settings(this, window, cx).into_any_element(),
    };

    v_flex()
        .relative()
        .flex_1()
        .min_w_0()
        .size_full()
        .max_w(pane_w)
        .bg(theme.tokens.background)
        // Dropping image files from Finder adds them as entries, matching the
        // composer's "drag image" hint.
        .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
            for path in paths.paths() {
                if let Some(mime) = image_mime_for(path) {
                    this.add_captured_image(
                        path.to_string_lossy().into_owned(),
                        mime.to_owned(),
                        cx,
                    );
                }
            }
        }))
        // The column is centered on wide windows; the header's origin is the
        // column's window-space origin, which morph positions subtract.
        .on_children_prepainted({
            let origin = this.content_origin.clone();
            move |children: Vec<Bounds<Pixels>>, _, _| {
                // Every non-overlay child starts at the column's origin; take
                // the minimum so child ordering can never skew the morph.
                if let Some(min) = children
                    .iter()
                    .map(|bounds| bounds.origin)
                    .reduce(|a, b| point(a.x.min(b.x), a.y.min(b.y)))
                {
                    origin.set(min);
                }
            }
        })
        .child(header)
        .child(body)
        // The entry detail morph lives above the pages; the first-run tour
        // covers it, and dialogs stack above everything.
        .children(render_entry_modal(this, window, cx))
        .children(render_onboarding(this, window, cx))
        // Modal dialogs live in the window's dialog layer, which must be
        // rendered by the view tree; Root only stores the active dialog.
        .children(Root::render_dialog_layer(window, cx))
}

/// The morphing entry view: the panel's rect tweens from the trigger's rect
/// (card/click) to the middle of the UI, the content laid out at its target
/// size and revealed by the growing panel. Port of beui.dev's morphing modal
/// (rect + fade here; GPUI has no scale/filter).
/// Fullscreen image viewer: a fade-in panel on top of every other window.
/// Click anywhere or press ⎋ to close.
struct ImageViewer {
    source: ImageSource,
    label: SharedString,
    focus_handle: FocusHandle,
    started_at: Instant,
}

impl ImageViewer {
    fn new(source: ImageSource, label: SharedString, cx: &mut Context<Self>) -> Self {
        Self {
            source,
            label,
            focus_handle: cx.focus_handle(),
            started_at: Instant::now(),
        }
    }
}

impl Focusable for ImageViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let spec = worktable_ui::MODAL_OPEN;
        let elapsed = self.started_at.elapsed();
        let t = if elapsed < spec.total() {
            // Keep frames coming while the fade runs.
            let _ = worktable_ui::activity_now(cx.entity_id(), cx);
            spec.progress((elapsed.as_secs_f32() / spec.total().as_secs_f32()).min(1.0))
        } else {
            1.0
        };
        div()
            .id("image-viewer")
            .debug_selector(|| "image-viewer".into())
            .key_context("ImageViewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.tokens.background)
            .opacity(t)
            .on_click(|_, window, _| window.remove_window())
            .on_action(
                |_: &crate::actions::CloseImageViewer, window: &mut Window, _: &mut App| {
                    window.remove_window()
                },
            )
            .child(
                img(self.source.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .debug_selector(|| "image-viewer-image".into()),
            )
            .child(
                div().absolute().top_3().right_3().child(
                    CircleAction::new("image-viewer-close")
                        .ghost()
                        .icon(app_icon(IconName::Close))
                        .tooltip("Close")
                        .debug_selector("image-viewer-close")
                        .on_click(|_, window, _| window.remove_window()),
                ),
            )
            .child(
                div()
                    .absolute()
                    .bottom_3()
                    .left_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .absolute()
                    .bottom_3()
                    .right_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Click anywhere or press ⎋ to close"),
            )
    }
}

/// The first-run tour. Welcome shows the keymap as [`Kbd`] keycaps (styled as
/// our chips), then the Accessibility permission, then a skippable provider
/// step. Every step can be skipped; completing it is persisted in config.
fn render_onboarding(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> Option<gpui::AnyElement> {
    let state = this.onboarding.as_ref()?;
    let step = state.step;
    let trusted = state.accessibility_trusted;
    let theme = cx.theme().clone();

    let mut step_body = v_flex().gap_3().w_full().min_w_0();
    let (title, body): (&str, &str) = match step {
        OnboardingStep::Welcome => {
            use crate::actions::*;
            let rows: Vec<(&'static str, Vec<gpui::AnyElement>)> = vec![
                (
                    "Search or ask",
                    vec![onboarding_kbd(&FocusSearch, "cmd-f", window, &theme)],
                ),
                (
                    "Library / Agent",
                    vec![
                        onboarding_kbd(&ShowEntries, "cmd-1", window, &theme),
                        onboarding_kbd(&ShowAssistant, "cmd-2", window, &theme),
                    ],
                ),
                (
                    "Settings",
                    vec![onboarding_kbd(&ShowSettings, "cmd-,", window, &theme)],
                ),
                (
                    "Move selection",
                    vec![
                        onboarding_kbd(&SelectPrevious, "up", window, &theme),
                        onboarding_kbd(&SelectNext, "down", window, &theme),
                    ],
                ),
                (
                    "Open / Delete",
                    vec![
                        onboarding_kbd(&OpenEntry, "enter", window, &theme),
                        onboarding_kbd(&DeleteEntry, "backspace", window, &theme),
                    ],
                ),
            ];
            for (label, keys) in rows {
                step_body = step_body.child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(div().text_sm().text_color(theme.foreground).child(label))
                        .child(h_flex().gap_1().children(keys)),
                );
            }
            (
                "Welcome to Worktable",
                "Capture a note, ask your library, and find anything later. These shortcuts work anywhere in the app.",
            )
        }
        OnboardingStep::Accessibility => {
            step_body = step_body
                .child(
                    div()
                        .id("onboarding-accessibility-status")
                        .debug_selector(|| "onboarding-accessibility-status".into())
                        .role(Role::Status)
                        .text_xs()
                        .text_color(if trusted {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        })
                        .child(if trusted {
                            "Accessibility access is on."
                        } else {
                            "Not granted yet."
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("onboarding-open-accessibility")
                                .label("Open System Settings")
                                .secondary()
                                .small()
                                .debug_selector(|| "onboarding-open-accessibility".into())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    open_accessibility_settings();
                                    this.refresh_accessibility(cx);
                                })),
                        )
                        .child(
                            Button::new("onboarding-check-accessibility")
                                .label("Check again")
                                .ghost()
                                .small()
                                .debug_selector(|| "onboarding-check-accessibility".into())
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.refresh_accessibility(cx)),
                                ),
                        ),
                );
            (
                "Allow global capture",
                "Worktable lives in the menu bar and can capture text or images from other apps. macOS asks for Accessibility permission the first time. You can grant it now, or later in System Settings → Privacy & Security → Accessibility.",
            )
        }
        OnboardingStep::Provider => (
            "Connect an AI provider",
            "Ask questions about your notes with the model of your choice. Add an API key or sign in — or skip this and set it up later in Settings → Providers.",
        ),
    };

    let page_label = match step {
        OnboardingStep::Welcome => "Step 1 of 3",
        OnboardingStep::Accessibility => "Step 2 of 3",
        OnboardingStep::Provider => "Step 3 of 3",
    };
    let is_first = step == OnboardingStep::Welcome;
    let is_last = step == OnboardingStep::Provider;

    // The actions wrap instead of overflowing the card on narrow windows (a
    // clipped primary would be unclickable). The step badge above the title
    // already carries the page number.
    let mut footer = v_flex().w_full().gap_2();
    let mut actions = h_flex().w_full().flex_wrap().justify_end().gap_2();
    if !is_first {
        actions = actions.child(
            Button::new("onboarding-back")
                .label("Back")
                .ghost()
                .small()
                .debug_selector(|| "onboarding-back".into())
                .on_click(cx.listener(|this, _, _, cx| this.back_onboarding(cx))),
        );
    }
    actions = actions.child(
        Button::new("onboarding-skip")
            .label(if is_last { "Skip for now" } else { "Skip" })
            .ghost()
            .small()
            .debug_selector(|| "onboarding-skip".into())
            .on_click(cx.listener(|this, _, _, cx| this.skip_onboarding(cx))),
    );
    actions = actions.child(if is_last {
        Button::new("onboarding-provider")
            .label("Set up provider")
            .primary()
            .small()
            .debug_selector(|| "onboarding-provider".into())
            .on_click(cx.listener(|this, _, _, cx| this.setup_provider_from_onboarding(cx)))
    } else {
        Button::new("onboarding-next")
            .label("Continue")
            .primary()
            .small()
            .debug_selector(|| "onboarding-next".into())
            .on_click(cx.listener(|this, _, _, cx| this.advance_onboarding(cx)))
    });
    footer = footer.child(actions);

    Some(
        div()
            .id("onboarding")
            .debug_selector(|| "onboarding".into())
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.tokens.background.opacity(0.72))
            .child(
                v_flex()
                    .w(design::to_pixels(design::ONBOARDING_CARD_WIDTH, window))
                    .max_w(gpui::relative(0.92))
                    .max_h(gpui::relative(0.9))
                    .min_w_0()
                    .overflow_hidden()
                    .rounded(theme.radius_tokens().lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_lg()
                    .child(
                        v_flex()
                            .id("onboarding-content")
                            .gap_3()
                            .p_6()
                            .w_full()
                            .min_w_0()
                            .overflow_y_scroll()
                            .child(
                                img(crate::assets::LOGO_PATH)
                                    .size(rems(3.0))
                                    .rounded(theme.radius_tokens().md),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.primary)
                                    .child(page_label),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(body),
                            )
                            .child(step_body),
                    )
                    .child(footer.px_6().py_4().border_t_1().border_color(theme.border)),
            )
            .into_any_element(),
    )
}

fn render_entry_modal(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> Option<gpui::AnyElement> {
    let modal = this.entry_modal.as_ref()?;
    let entry = this
        .entries
        .iter()
        .find(|entry| entry.id == modal.entry_id)?
        .clone();
    let theme = cx.theme().clone();
    let viewport = window.viewport_size();
    let pane_w = design::content_column_width(window);
    let edge = design::to_pixels(rems(0.75), window);
    let panel_w = (pane_w - edge * 2.0).max(design::to_pixels(rems(16.0), window));
    let panel_h = viewport.height * 0.72;
    let target = Bounds::new(
        point((pane_w - panel_w) / 2.0, (viewport.height - panel_h) / 2.0),
        size(panel_w, panel_h),
    );

    let (raw, spec, opening) = match modal.closing_at {
        Some(started) => (started.elapsed(), worktable_ui::MORPH_CLOSE, false),
        None => (modal.opened_at.elapsed(), worktable_ui::MORPH_OPEN, true),
    };
    // The unmount timer clears the state in production, but never rely on a
    // clock ticking to stop painting: once the close span is over the panel
    // is done regardless.
    if !opening && raw >= spec.total() {
        return None;
    }
    if raw < spec.total() {
        // Keep frames coming for the morph.
        let _ = worktable_ui::activity_now(cx.entity_id(), cx);
    }
    let eased = spec.progress((raw.as_secs_f32() / spec.total().as_secs_f32()).min(1.0));
    let t = if opening { eased } else { 1.0 - eased };
    // Closing shrank back into the card it came from (its live rect, so a
    // scrolled list still matches); opening grows out of the stored rect.
    let from = if opening {
        modal.origin
    } else {
        this.card_bounds
            .borrow()
            .get(&modal.entry_id)
            .map(|bounds| this.local_bounds(*bounds))
            .unwrap_or(modal.origin)
    };
    let lerp = |from: Pixels, to: Pixels| from + (to - from) * t;
    let panel = Bounds::new(
        point(
            lerp(from.origin.x, target.origin.x),
            lerp(from.origin.y, target.origin.y),
        ),
        size(
            lerp(from.size.width, target.size.width),
            lerp(from.size.height, target.size.height),
        ),
    );
    // Content cross-fades in once the panel is meaningfully large.
    let content_t = ((t - 0.3) / 0.7).clamp(0.0, 1.0);

    let meta = format!(
        "{} · {}",
        entry.source,
        crate::format::relative_time(entry.created_at)
    );
    let close_view = cx.entity();
    let copy_text = entry
        .title
        .as_deref()
        .map(|title| format!("{title}\n{}", entry.content))
        .unwrap_or_else(|| entry.content.clone());
    let actions = crate::entry_actions::detect_actions(&entry.content);
    let editing = this.entry_editing;

    // Header: metadata + close.
    let header = h_flex()
        .items_center()
        .gap_2()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(meta),
        )
        .child(div().flex_1())
        .child(
            CircleAction::new("entry-modal-close")
                .ghost()
                .icon(app_icon(IconName::Close))
                .tooltip("Close")
                .debug_selector("entry-modal-close")
                .on_click({
                    let view = close_view.clone();
                    move |_, window, cx| {
                        view.update(cx, |this, cx| {
                            this.close_entry_modal(cx);
                            this.focus_handle.focus(window, cx);
                        });
                    }
                }),
        );

    let mut body = v_flex().size_full().min_h_0().child(header);
    if let Some(title) = entry.title.clone() {
        // The title is selectable text (copyable via selection) like the body.
        // `TextView`'s root is height-100%, so it lives in a fixed one-line
        // wrapper; an unconstrained child would take the whole panel. The
        // wrapper's height is its content box — the top padding sits outside.
        body = body.child(
            div().px_4().pt_3().child(
                div()
                    .h(design::to_pixels(rems(1.5), window))
                    .min_h_0()
                    .overflow_hidden()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .debug_selector(|| "entry-modal-title".into())
                    .child(
                        TextView::markdown(
                            SharedString::from(format!("entry-modal-title:{}", entry.id)),
                            title,
                        )
                        .selectable(true),
                    ),
            ),
        );
    }

    if editing {
        body = body.child(
            div()
                .flex_1()
                .min_h_0()
                .p_3()
                .debug_selector(|| "entry-modal-editor".into())
                .child(
                    Textarea::new(&this.entry_edit_input)
                        .h(gpui::relative(1.0))
                        .appearance(true)
                        .bordered(true),
                ),
        );
    } else if let Some(source) = image_source_for(&entry.content) {
        body = body.child(
            div()
                .flex_1()
                .min_h_0()
                .p_4()
                .debug_selector(|| "entry-modal-image".into())
                .child(render_entry_image(
                    source,
                    entry.content.clone(),
                    &theme,
                    cx,
                )),
        );
    } else {
        body = body.child(
            // The rich text component owns scrolling: its root takes the
            // container's height, so an outer scroll container cannot measure
            // the overflow. `scrollable(true)` virtualizes the content and
            // draws its own scrollbar.
            div()
                .flex_1()
                .min_h_0()
                .px_4()
                .pt_3()
                .pb_2()
                .debug_selector(|| "entry-modal-scroll".into())
                .child(
                    TextView::markdown(
                        SharedString::from(format!("entry-modal-body:{}", entry.id)),
                        entry.content.clone(),
                    )
                    .selectable(true)
                    .scrollable(true),
                ),
        );
    }

    let mut footer = h_flex()
        .flex_wrap()
        .gap_2()
        .items_center()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border)
        .child(modal_chip(
            "entry-modal-copy",
            "Copy",
            Some(IconName::Copy),
            move |_window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
            },
        ))
        .children(actions.into_iter().map(|action| match action {
            crate::entry_actions::EntryAction::Link(url) => modal_chip(
                "entry-modal-open-link",
                "Open link",
                Some(IconName::ExternalLink),
                move |_window, cx| cx.open_url(&url),
            ),
            crate::entry_actions::EntryAction::Email(email) => {
                modal_chip("entry-modal-email", "Email", None, move |_window, cx| {
                    cx.open_url(&format!("mailto:{email}"))
                })
            }
            crate::entry_actions::EntryAction::Phone(phone) => {
                modal_chip("entry-modal-call", "Call", None, move |_window, cx| {
                    cx.open_url(&format!("tel:{}", phone.replace([' ', '(', ')'], "")))
                })
            }
        }));
    footer = footer.child(div().flex_1());
    if editing {
        let cancel_view = cx.entity();
        footer = footer.child(modal_chip(
            "entry-modal-cancel",
            "Cancel",
            None,
            move |_window, cx| {
                cancel_view.update(cx, |this, cx| this.cancel_entry_edit(cx));
            },
        ));
        let save_view = cx.entity();
        footer = footer.child(
            Button::new("entry-modal-save")
                .label("Save")
                .primary()
                .small()
                .rounded_full()
                .debug_selector(|| "entry-modal-save".into())
                .on_click(move |_, _, cx| {
                    save_view.update(cx, |this, cx| this.save_entry_edit(cx));
                }),
        );
    } else {
        let edit_view = cx.entity();
        footer = footer.child(modal_chip(
            "entry-modal-edit",
            "Edit",
            None,
            move |window, cx| {
                edit_view.update(cx, |this, cx| this.start_entry_edit(window, cx));
            },
        ));
    }
    if let Some(status) = this.entry_edit_status.clone() {
        footer = footer.child(
            div()
                .id("entry-modal-edit-status")
                .role(Role::Status)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(status),
        );
    }
    body = body.child(footer);

    Some(
        div()
            .absolute()
            .inset_0()
            .child(
                div()
                    .id("entry-modal-backdrop")
                    .absolute()
                    .inset_0()
                    .debug_selector(|| "entry-modal-backdrop".into())
                    .bg(theme.tokens.background.opacity(0.62 * t))
                    .on_mouse_pressure(|_, _, _| {})
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_entry_modal(cx);
                        this.focus_handle.focus(window, cx);
                    })),
            )
            .child(
                div()
                    .id("entry-modal")
                    .debug_selector(|| "entry-modal".into())
                    .occlude()
                    .absolute()
                    .left(panel.origin.x)
                    .top(panel.origin.y)
                    .w(panel.size.width)
                    .h(panel.size.height)
                    .min_w_0()
                    .overflow_hidden()
                    .rounded(theme.radius_tokens().lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.tokens.popover)
                    // The whole panel fades with the tween so the morph reads
                    // as a materialization, not a pop.
                    .opacity(t.clamp(0.0, 1.0))
                    .shadow_lg()
                    // Lay the content out at the target size; the growing
                    // panel clips it, so text never reflows mid-morph.
                    .child(
                        div()
                            .w(target.size.width)
                            .h(target.size.height)
                            .opacity(content_t)
                            .child(body),
                    ),
            )
            .into_any_element(),
    )
}

fn render_library_header(
    this: &mut WorktableView,
    pane_w: Pixels,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let is_assistant = this.mode == AppMode::Assistant;

    // The leading slot morphs between the full search field (Entries) and a
    // circular search button (Agent, where the field has nothing to filter).
    // Progress rides the page-slide clock so both transitions match.
    let circle = design::to_pixels(rems(2.5), window);
    let reserved = design::to_pixels(rems(2.0), window)
        + design::to_pixels(rems(1.0), window)
        + design::to_pixels(rems(5.0), window);
    let full = (pane_w - reserved).max(circle);
    let eased = |raw: f32| worktable_ui::PAGE_SLIDE.progress(raw);
    let progress = match this.page_anim_at {
        Some(started) => {
            let elapsed = started.elapsed();
            if elapsed < worktable_ui::PAGE_SLIDE.total() {
                let raw = elapsed.as_secs_f32() / worktable_ui::PAGE_SLIDE.total().as_secs_f32();
                let eased = eased(raw);
                if is_assistant { eased } else { 1.0 - eased }
            } else if is_assistant {
                1.0
            } else {
                0.0
            }
        }
        None => {
            if is_assistant {
                1.0
            } else {
                0.0
            }
        }
    };
    let width = full + (circle - full) * progress;
    // The slot dissolves between its two states instead of swapping at one
    // instant: the field fades out over the first half of the morph (while it
    // is still shrinking), the circle fades in over the second. Each side is
    // invisible at the hand-off, so nothing pops.
    let show_input = progress < 0.5;
    let field_opacity = (1.0 - progress * 2.0).clamp(0.0, 1.0);
    let circle_opacity = (progress * 2.0 - 1.0).clamp(0.0, 1.0);
    let slot: gpui::AnyElement = if show_input {
        div()
            .w(width)
            .min_w_0()
            .opacity(field_opacity)
            .debug_selector(|| "library-search-input".into())
            .child(
                Input::new(&this.search_input)
                    .w_full()
                    .appearance(true)
                    .bordered(false)
                    .focus_bordered(false)
                    .bg(theme.muted)
                    .text_color(theme.foreground)
                    .prefix(
                        Icon::new(IconName::Search)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .into_any_element()
    } else {
        div()
            .w(width)
            .min_w_0()
            .opacity(circle_opacity)
            .debug_selector(|| "agent-search-slot".into())
            .child(
                CircleAction::new("agent-search")
                    .secondary()
                    // Same glyph and colour as the field's prefix icon: the
                    // field collapses into its own search icon, so the swap
                    // must not change the glyph's tone.
                    .icon(
                        Icon::new(IconName::Search)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    )
                    .tooltip("Search entries")
                    .debug_selector("agent-search-button")
                    .on_click(cx.listener(|this, _, window, cx| {
                        // Morph back into the field and land on the list.
                        this.show_entries(cx);
                        this.focus_search(window, cx);
                    })),
            )
            .into_any_element()
    };

    // Search slot + page toggle + library menu as real flex siblings. The
    // search field doubles as the prompt box: Enter sends the query to the
    // agent; Tab moves focus on to the page toggle like every other control.
    h_flex()
        .id("library-header")
        .debug_selector(|| "library-header".into())
        .w_full()
        .items_center()
        .gap_2()
        .px_4()
        .py_2()
        .bg(theme.tokens.background)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .debug_selector(|| "library-search".into())
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .child(slot)
                        .child(div().flex_1()),
                ),
        )
        .child(page_toggle_button(this, cx))
        .child(library_menu_button(this, cx))
        .into_any_element()
}

/// Shared header for the Settings-family pages: a back button at the left
/// corner (returns to Settings on the given tab) and — on the Settings page
/// itself — the UI/Data/Providers tab group.
fn settings_header(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let page = this.settings_tab;
    let title = page.map(SettingsTab::title).unwrap_or("Settings");

    h_flex()
        .id("settings-header")
        .debug_selector(|| "settings-header".into())
        .w_full()
        .items_center()
        .gap_3()
        .px_4()
        .py_2()
        .bg(theme.tokens.background)
        .child(
            Button::new("settings-back")
                .icon(app_icon(IconName::ArrowLeft))
                .label("Back")
                .ghost()
                .small()
                .debug_selector(|| "settings-back".into())
                .tooltip(if page.is_some() {
                    "Back to settings"
                } else {
                    "Back to entries"
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.settings_tab.is_some() {
                        // Category pages return to the Settings list; the
                        // list's Back returns to the main UI.
                        this.settings_tab = None;
                        cx.notify();
                    } else {
                        this.show_entries(cx);
                    }
                })),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(div().flex_1())
        .into_any_element()
}

/// The library menu: a visible dropdown trigger owning its own open state,
/// dismissal and keyboard navigation. The `secondary` variant keeps a filled
/// rest state and switches to its active background while the popup is open.
/// Entries ⇄ Agent toggle: an icon-only circular button left of the menu that
/// shows the current page's icon and switches to the other page.
fn page_toggle_button(this: &WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let is_assistant = this.mode == AppMode::Assistant;
    // The icon shows where a click takes you: a sparkle pair for the agent, a
    // dashboard for the entries list.
    let mut toggle = CircleAction::new("page-toggle")
        .secondary()
        .large()
        .debug_selector("page-toggle")
        .tooltip(if is_assistant {
            "Showing agent — switch to entries"
        } else {
            "Showing entries — switch to agent"
        })
        .on_click(cx.listener(|this, _, _, cx| {
            if this.mode == AppMode::Assistant {
                this.show_entries(cx);
            } else {
                this.show_assistant(cx);
            }
        }));
    if is_assistant {
        toggle = toggle.icon(app_icon(IconName::LayoutDashboard));
    } else {
        toggle = toggle.child(sparkle_icon(theme.secondary_foreground));
    }
    toggle
}

/// The library menu: Settings, the entry sort modes (the button group they
/// replaced sat under the header), and Quit.
fn library_menu_button(this: &WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let view = cx.entity();
    let (is_time, is_alpha, is_topic) = (
        this.sort_mode == SortMode::Time,
        this.sort_mode == SortMode::Alpha,
        this.sort_mode == SortMode::Topic,
    );
    let ascending = this.sort_ascending;

    Button::new("library-menu")
        .icon(app_icon(IconName::Menu))
        .secondary()
        .size_10()
        .rounded_full()
        .tooltip("Library menu")
        .debug_selector(|| "library-menu".into())
        .dropdown_menu(move |menu, _, _| {
            let settings_view = view.clone();
            let mut menu = menu
                .item(
                    PopupMenuItem::new("Settings")
                        .icon(Icon::new(IconName::Settings))
                        .on_click(move |_, _, cx| {
                            settings_view.update(cx, |this, cx| this.show_settings(cx));
                        }),
                )
                .separator();
            for (mode, checked, label) in [
                (
                    SortMode::Time,
                    is_time,
                    if is_time && ascending {
                        "Oldest first"
                    } else {
                        "Newest first"
                    },
                ),
                (
                    SortMode::Alpha,
                    is_alpha,
                    if is_alpha && ascending {
                        "A to Z"
                    } else {
                        "Z to A"
                    },
                ),
                (
                    SortMode::Topic,
                    is_topic,
                    if is_topic && ascending {
                        "Topics A to Z"
                    } else {
                        "Topics Z to A"
                    },
                ),
            ] {
                let view = view.clone();
                menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                    move |_, _, cx| {
                        view.update(cx, |this, cx| this.set_sort_mode(mode, cx));
                    },
                ));
            }
            menu.separator().item(
                PopupMenuItem::new("Quit Worktable")
                    .icon(Icon::new(IconName::Close))
                    .on_click(|_, _, cx| cx.quit()),
            )
        })
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

    if this.settings_tab == Some(SettingsTab::Providers) && this.providers.is_empty() {
        let message = this
            .settings_status
            .clone()
            .unwrap_or_else(|| "Loading providers…".to_owned());
        let loader = if this.providers_loading {
            div()
                .debug_selector(|| "providers-loading".into())
                .child(
                    Orb::new("providers-loading", OrbVariant::G2)
                        .view(cx.entity_id())
                        .size(rems(1.75))
                        .color(theme.primary),
                )
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
                    .small()
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

    // General page: window/background behavior. The toggle mirrors into a
    // process-wide flag the AppKit close handler reads synchronously.
    let general_section = v_flex()
        .gap_5()
        .p_4()
        .child(
            v_flex()
                .gap_2()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Window"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("What the close button does"),
                ),
        )
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child("Keep running in the menu bar"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Closing the window hides Worktable, removes it from the Dock, and keeps the menu bar icon and capture shortcut active. Choose Open Window from the menu bar to bring it back."),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .debug_selector(|| "background-on-close-switch".into())
                        .child(
                            Switch::new("toggle-background-on-close")
                                .checked(this.background_on_close)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if *checked != this.background_on_close {
                                        this.toggle_background_on_close(cx);
                                    }
                                })),
                        ),
                ),
        );

    // Appearance page: theme mode group + the thinking toggle.
    let is_light = this.theme_mode == AppThemeMode::Light;
    let is_dark = this.theme_mode == AppThemeMode::Dark;
    let is_system = this.theme_mode == AppThemeMode::System;
    let appearance_section = v_flex()
        .gap_5()
        .p_4()
        .child(
            v_flex()
                .gap_2()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Theme"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Light, dark, or follow the system appearance"),
                )
                .child(
                    h_flex().w_full().justify_end().child(
                        ButtonGroup::new("theme-mode-group")
                            .compact()
                            .outline()
                            .child(
                                Button::new("theme-light")
                                    .icon(app_icon(IconName::Sun))
                                    .tooltip("Light")
                                    .debug_selector(|| "theme-light".into())
                                    .selected(is_light),
                            )
                            .child(
                                Button::new("theme-dark")
                                    .icon(app_icon(IconName::Moon))
                                    .tooltip("Dark")
                                    .debug_selector(|| "theme-dark".into())
                                    .selected(is_dark),
                            )
                            .child(
                                Button::new("theme-system")
                                    .icon(app_icon(IconName::Cpu))
                                    .tooltip("System")
                                    .debug_selector(|| "theme-system".into())
                                    .selected(is_system),
                            )
                            .on_click(cx.listener(|this, selected: &Vec<usize>, window, cx| {
                                let mode = match selected.first() {
                                    Some(0) => AppThemeMode::Light,
                                    Some(1) => AppThemeMode::Dark,
                                    _ => AppThemeMode::System,
                                };
                                if mode != this.theme_mode {
                                    this.set_theme_mode(mode, window, cx);
                                }
                            })),
                    ),
                ),
        )
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child("Show thinking"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Display the assistant's reasoning while it works"),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .debug_selector(|| "thinking-switch".into())
                        .child(
                            Switch::new("toggle-thinking")
                                .checked(this.show_thinking)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if *checked != this.show_thinking {
                                        this.toggle_show_thinking(cx);
                                    }
                                })),
                        ),
                ),
        )
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child("Welcome tour"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Replay the setup steps and keyboard shortcuts"),
                        ),
                )
                .child(
                    Button::new("show-onboarding")
                        .label("Replay")
                        .ghost()
                        .small()
                        .debug_selector(|| "show-onboarding".into())
                        .on_click(cx.listener(|this, _, _, cx| this.start_onboarding(cx))),
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
                .rounded(theme.radius_tokens().md)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .child(Icon::new(IconName::Star).size_5().text_color(theme.primary))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child("GitHub stars"))
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
                        .small()
                        .debug_selector(|| "configure-github-stars".into())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_github_dialog(window, cx)),
                        ),
                ),
        );

    // One card per provider; Configure opens the modal auth dialog. The model
    // picker stays on the row so switching a model is one click.
    let provider_rows: Vec<gpui::AnyElement> = this
        .providers
        .iter()
        .map(|provider| render_settings_provider_row(this, provider, &view, cx))
        .collect();
    let login_panel = this
        .logging_in
        .iter()
        .next()
        .cloned()
        .and_then(|provider_id| render_active_login(&view, &provider_id, cx));
    let mut providers_section = v_flex()
        .gap_2()
        .p_4()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("AI providers"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Choose a provider, model, and authentication method for the assistant."),
        )
        .child(div().text_sm().text_color(theme.primary).child(active))
        .children(provider_rows);
    if let Some(status) = &this.settings_status {
        providers_section = providers_section.child(
            div()
                .id("providers-status")
                .role(Role::Status)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(status.clone()),
        );
    }
    if let Some(panel) = login_panel {
        providers_section = providers_section.child(panel);
    }

    let content = match this.settings_tab {
        None => settings_categories(&theme, cx).into_any_element(),
        Some(SettingsTab::General) => general_section.into_any_element(),
        Some(SettingsTab::Appearance) => appearance_section.into_any_element(),
        Some(SettingsTab::Data) => data_section.into_any_element(),
        Some(SettingsTab::Providers) => providers_section.into_any_element(),
    };

    v_flex()
        .flex_1()
        .w_full()
        .overflow_y_scrollbar()
        .child(content)
        .into_any_element()
}

/// The Settings landing list: one line-separated row per category.
fn settings_categories(
    theme: &gpui_component::Theme,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
    let mut list = v_flex()
        .w_full()
        .debug_selector(|| "settings-categories".into());
    for tab in [
        SettingsTab::General,
        SettingsTab::Appearance,
        SettingsTab::Data,
        SettingsTab::Providers,
    ] {
        let id = match tab {
            SettingsTab::General => "settings-category-general",
            SettingsTab::Appearance => "settings-category-appearance",
            SettingsTab::Data => "settings-category-data",
            SettingsTab::Providers => "settings-category-providers",
        };
        let selector = id.to_owned();
        let row = ListItem::new(id)
            .debug_selector(move || selector.clone())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings_tab = Some(tab);
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(tab.title()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(tab.description()),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    ),
            );
        list = list.child(
            div()
                .w_full()
                .border_b_1()
                .border_color(theme.border.opacity(0.6))
                .child(row),
        );
    }
    list
}

/// One provider card in Settings → Providers: identity and auth badges,
/// the model picker when the provider is usable, and the Configure button
/// that opens the auth dialog.
fn render_settings_provider_row(
    this: &WorktableView,
    provider: &ProviderInfo,
    view: &gpui::Entity<WorktableView>,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let is_active = this.active_provider.as_deref() == Some(&provider.id);
    let active_model = this.active_model.clone();

    let mut badges = Vec::new();
    if provider.supports_api_key {
        badges.push(if provider.api_key_set {
            "API key ✓"
        } else {
            "API key not set"
        });
    }
    if provider.supports_oauth {
        badges.push(if provider.oauth_set {
            "OAuth ✓"
        } else {
            "OAuth not connected"
        });
    }
    if is_active {
        badges.push("Active");
    }
    let badge_text = badges.join(" · ");

    let mut row = h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .p_3()
        .rounded(theme.radius_tokens().md)
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(provider.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(badge_text),
                ),
        );

    let usable = provider.api_key_set || provider.oauth_set;
    let model_options = this
        .provider_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();
    if usable && !model_options.is_empty() {
        row = row.child(model_picker_button(
            view,
            provider,
            model_options,
            active_model,
            cx,
        ));
    }

    let configure_view = view.clone();
    let provider_id = provider.id.clone();
    row = row.child(
        Button::new(format!("configure:{}", provider.id))
            .label("Configure")
            .icon(app_icon(IconName::Settings))
            .ghost()
            .small()
            .debug_selector({
                let selector = format!("configure:{}", provider.id);
                move || selector.clone()
            })
            .on_click(move |_, window, cx| {
                configure_view.update(cx, |this, cx| {
                    this.open_provider_dialog(&provider_id, window, cx)
                });
            }),
    );

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
        .small()
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
/// prompt the user must answer) for one provider.
fn render_active_login(
    view: &gpui::Entity<WorktableView>,
    provider_id: &str,
    cx: &mut App,
) -> Option<gpui::AnyElement> {
    let this = view.read(cx);
    if !this.logging_in.contains(provider_id) {
        return None;
    }
    let provider_id = provider_id.to_owned();
    let theme = cx.theme().clone();

    let mut column = v_flex().gap_2();

    let mut has_content = false;
    if let Some(notice) = &this.auth_notice
        && notice.provider_id == provider_id
    {
        has_content = true;
        match &notice.notify {
            AuthNotifyKind::AuthUrl { url, instructions } => {
                let url = url.clone();
                let instructions = instructions.clone();
                let view = view.clone();
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
                let view = view.clone();
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
        && pending.provider_id == provider_id
    {
        has_content = true;
        match &pending.prompt {
            AuthPromptKind::Text { message, .. }
            | AuthPromptKind::Secret { message, .. }
            | AuthPromptKind::ManualCode { message, .. } => {
                let message = message.clone();
                let view = view.clone();
                column = column.child(div().text_sm().child(message)).child(
                    h_flex()
                        .gap_2()
                        .child(Input::new(&this.prompt_input))
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
                let view = view.clone();
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
            .rounded(theme.radius_tokens().md)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .child(column)
            .into_any_element(),
    )
}

fn composer_bar(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    h_flex()
        .id("composer-bar")
        .debug_selector(|| "composer-bar".into())
        .w_full()
        .items_center()
        .gap_2()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border)
        .child(
            CircleAction::new("add-image")
                .ghost()
                .icon(app_icon(IconName::Plus))
                .tooltip("Add image…")
                .debug_selector("add-image")
                .on_click(cx.listener(|this, _, _, cx| this.pick_image(cx))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .debug_selector(|| "composer-input".into())
                .child(
                    Input::new(&this.composer_body)
                        .w_full()
                        .appearance(true)
                        .bordered(false)
                        .focus_bordered(false)
                        .bg(theme.muted)
                        .text_color(theme.foreground),
                ),
        )
        .child(
            CircleAction::new("add-note")
                .primary()
                .icon(app_icon(IconName::Check))
                .tooltip("Add note")
                .debug_selector("add-note")
                .on_click(cx.listener(|this, _, window, cx| this.submit_composer(window, cx))),
        )
        .into_any_element()
}

/// The Entries pane: sort bar + virtualized card list + composer. Slides as
/// a unit under the fixed Entries|Agent button group.
fn render_entries_pane(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();
    let visible = this.visible_entries();

    // Flatten sections into virtual-list items with fixed row sizes. Rows keep
    // only the entry id; the card looks the entry up when it renders, so the
    // view never clones the whole entry set on every frame.
    let mut sections: Vec<(String, Vec<&WorktableEntry>)> = Vec::new();
    for entry in &visible {
        let entry = *entry;
        let label = if this.sort_mode == SortMode::Topic {
            this.primary_topic(entry)
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
        let ascending = this.sort_ascending;
        sections.sort_by(|a, b| {
            if a.0 == "OTHER" {
                std::cmp::Ordering::Greater
            } else if b.0 == "OTHER" {
                std::cmp::Ordering::Less
            } else if ascending {
                a.0.cmp(&b.0)
            } else {
                b.0.cmp(&a.0)
            }
        });
    }

    enum Row {
        Header(String),
        Card {
            id: String,
            height: Pixels,
            body_lines: usize,
        },
    }
    // Row metrics resolve from the current rem size so the list zooms with the
    // rest of the interface.
    let section_header_height = design::to_pixels(design::SECTION_HEADER_HEIGHT, window);
    let min_card_height = design::to_pixels(design::ENTRY_CARD_MIN_HEIGHT, window);
    let max_card_height = design::to_pixels(design::ENTRY_CARD_MAX_HEIGHT, window);
    let row_gap = design::to_pixels(design::ENTRY_ROW_GAP, window);
    // Width available to a card's text: the reading column minus the card's
    // horizontal padding, the selection circle, and the row gap.
    let text_width = (design::content_column_width(window)
        - design::to_pixels(rems(2.0), window)
        - design::to_pixels(rems(1.25), window)
        - design::to_pixels(rems(1.0), window)
        - design::to_pixels(rems(0.75), window))
    .max(design::to_pixels(rems(8.0), window));
    let mut rows: Vec<Row> = Vec::new();
    let mut sizes: Vec<Size<Pixels>> = Vec::new();
    for (label, entries) in &sections {
        rows.push(Row::Header(label.clone()));
        sizes.push(size(Pixels::ZERO, section_header_height));
        for entry in entries {
            let (height, body_lines) =
                entry_card_metrics(entry, text_width, min_card_height, max_card_height, window);
            rows.push(Row::Card {
                id: entry.id.clone(),
                height,
                body_lines,
            });
            sizes.push(size(Pixels::ZERO, height + row_gap));
        }
    }
    let rows = std::rc::Rc::new(rows);
    let item_sizes = std::rc::Rc::new(sizes);

    // Entrance: when the list last gained a new entry, the existing rows start
    // one card higher (their pre-insert places) and glide down while the new
    // card fades in — the newcomer pushes the list down into its slot.
    let inserting = this
        .list_insert_at
        .is_some_and(|at| at.elapsed() < worktable_ui::FADE_IN.total());

    let list = v_virtual_list(
        view.clone(),
        "entries-virtual-list",
        item_sizes.clone(),
        move |this, range, _window, cx| {
            let rows = rows.clone();
            range
                .into_iter()
                .map(|index| match &rows[index] {
                    Row::Header(label) => {
                        let theme = cx.theme().clone();
                        library_section_heading(label, &theme).into_any_element()
                    }
                    Row::Card {
                        id,
                        height,
                        body_lines,
                    } => {
                        let show_topic = this.sort_mode != SortMode::Topic;
                        this.entries
                            .iter()
                            .find(|entry| &entry.id == id)
                            .map(|entry| {
                                let topic = this.primary_topic(entry);
                                let bounds_map = this.card_bounds.clone();
                                let entry_id = entry.id.clone();
                                let card = render_entry_card(
                                    entry,
                                    &topic,
                                    &this.selected,
                                    show_topic,
                                    *height,
                                    *body_lines,
                                    cx,
                                );
                                let card = if inserting
                                    && this.recent_entry_id.as_deref() == Some(id.as_str())
                                {
                                    fade_in(
                                        SharedString::from(format!("entry-insert-{id}")),
                                        div().child(card),
                                    )
                                    .into_any_element()
                                } else {
                                    card
                                };
                                div()
                                    .px_4()
                                    .pb(design::ENTRY_ROW_GAP)
                                    // Record the card's real rect for the
                                    // morph (the first child is the card).
                                    .on_children_prepainted(move |child_bounds, _, _| {
                                        if let Some(bounds) = child_bounds.first() {
                                            bounds_map
                                                .borrow_mut()
                                                .insert(entry_id.clone(), *bounds);
                                        }
                                    })
                                    .child(card)
                                    .into_any_element()
                            })
                            .unwrap_or_else(|| div().into_any_element())
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
    // Past the entrance window: stop animating future renders.
    if !inserting && this.list_insert_at.is_some() {
        this.list_insert_at = None;
    }
    let empty_el = {
        let message = if this.query.is_empty() {
            "No entries yet — add a note with the bar below."
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

    let body = if empty {
        empty_el
    } else {
        // Boundary fades only appear when there is content beyond the edge;
        // reaching the top or bottom hides the respective fade entirely.
        let scroll = this.entries_scroll.base_handle();
        let scroll_offset = scroll.offset();
        let max_offset = scroll.max_offset();
        let epsilon = Pixels::from(0.5);
        let at_top = scroll_offset.y >= -epsilon;
        let at_bottom = max_offset.y <= epsilon || scroll_offset.y <= -(max_offset.y) + epsilon;

        // No horizontal padding here: the scrollbar belongs to the region's
        // trailing edge. Rows and headings apply their own content inset.
        let list_container = div()
            .id("entries-list")
            .debug_selector(|| "entries-list".into())
            .role(Role::List)
            .aria_label("Entries")
            .flex_1()
            .min_h_0()
            .relative()
            // Track the pointer so position-less triggers (context menu,
            // keyboard) can still morph from where the user is.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                this.pointer_position = event.position;
            }))
            .on_scroll_wheel(cx.listener(|_this, _, _, cx| cx.notify()))
            .child(list)
            // Soften the top and bottom cut: the content fades into the
            // surface instead of clipping hard at the scroll viewport. The
            // bottom fade sits at the list's viewport edge (above the
            // composer clearance), not under the padding.
            .when(!at_top, |container| {
                container.child(
                    div()
                        .debug_selector(|| "entries-fade-top".into())
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(rems(0.75))
                        .bg(linear_gradient(
                            180.0,
                            linear_color_stop(theme.tokens.background, 0.0),
                            linear_color_stop(theme.tokens.background.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(!at_bottom, |container| {
                container.child(
                    div()
                        .debug_selector(|| "entries-fade-bottom".into())
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(rems(0.75))
                        .bg(linear_gradient(
                            0.0,
                            linear_color_stop(theme.tokens.background, 0.0),
                            linear_color_stop(theme.tokens.background.opacity(0.0), 1.0),
                        )),
                )
            })
            .child(scrollbar);
        let container = if inserting {
            // Start one card higher (the pre-insert layout) and glide down to
            // the new layout: existing rows are pushed down by the newcomer.
            let slide = min_card_height;
            list_container
                .with_animation(
                    "entries-insert",
                    worktable_ui::RESIZE.animation(),
                    move |el, t| el.relative().top(-slide * (1.0 - t)),
                )
                .into_any_element()
        } else {
            list_container.into_any_element()
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .child(container)
            .into_any_element()
    };

    v_flex()
        .id("entries-pane")
        .debug_selector(|| "entries-pane".into())
        .flex_1()
        .min_h_0()
        .size_full()
        .child(body)
        .child(composer_bar(this, cx).into_any_element())
        .into_any_element()
}

/// The Entries ⇄ Agent pages, stacked so both can animate at once.
///
/// Port of transitions.dev's page slide: the exiting page fades toward its own
/// 8px offset while the entering page fades in from the opposite side, over
/// 250ms `cubic-bezier(0.22,1,0.36,1)`. The tween is computed from wall time
/// and the shared frame clock is leased while it runs — element-animation
/// wrappers would remount the page subtrees (and replay every message's
/// entrance) on each switch. GPUI cannot blur element content, so the spec's
/// 3px blur is carried by the fade (see DESIGN.md).
fn render_library_pages(
    this: &mut WorktableView,
    pane_w: Pixels,
    entries_pane: gpui::AnyElement,
    assistant_pane: gpui::AnyElement,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let is_assistant = this.mode == AppMode::Assistant;
    let slide = design::to_pixels(design::PAGE_SLIDE_DISTANCE, window);
    let transition = this.page_anim_at.and_then(|started| {
        let elapsed = started.elapsed();
        (elapsed < worktable_ui::PAGE_SLIDE.total()).then_some(elapsed)
    });
    if transition.is_some() {
        // Keep frames coming for the duration of the slide.
        let _ = worktable_ui::activity_now(cx.entity_id(), cx);
    }
    let eased = |elapsed: std::time::Duration| {
        let raw = elapsed.as_secs_f32() / worktable_ui::PAGE_SLIDE.total().as_secs_f32();
        worktable_ui::PAGE_SLIDE.progress(raw)
    };

    let page = |assistant: bool, body: gpui::AnyElement| -> gpui::AnyElement {
        let active = assistant == is_assistant;
        let visible = active || transition.is_some();
        // `progress` runs rest → exit offset. The entering page reverses it,
        // so it fades in from the offset; the exiting page runs it forward.
        let progress = match transition {
            Some(elapsed) if active => 1.0 - eased(elapsed),
            Some(elapsed) => eased(elapsed),
            None => 0.0,
        };
        let offset = if assistant { slide } else { -slide };
        let selector = if assistant {
            "assistant-page"
        } else {
            "entries-page"
        };
        div()
            .absolute()
            .inset_0()
            .w(pane_w)
            .h_full()
            .flex()
            .flex_col()
            .debug_selector(move || selector.to_owned())
            .when(!visible, |el| el.hidden())
            .left(offset * progress)
            .opacity(1.0 - progress)
            .child(body)
            .into_any_element()
    };

    let entries = page(false, entries_pane);
    let assistant = page(true, assistant_pane);
    div()
        .flex_1()
        .min_h_0()
        .w(pane_w)
        .max_w_full()
        .overflow_hidden()
        .relative()
        .child(entries)
        .child(assistant)
        .into_any_element()
}

/// Whether the entry's content is a bare URL.
fn content_is_link(content: &str) -> bool {
    let candidate = content.trim();
    candidate.starts_with("http://") || candidate.starts_with("https://")
}

/// Whether the entry's content points at an image file (path or URL).
fn content_is_image(content: &str) -> bool {
    image_source_for(content).is_some()
}

/// Turn image content into a renderable source. Local paths must exist;
/// http(s) URLs are loaded by the image element's HTTP client. A URL only
/// counts as an image when its path carries an image extension — otherwise
/// every link entry would render as a broken thumbnail.
fn image_source_for(content: &str) -> Option<ImageSource> {
    let candidate = content.split_whitespace().next().unwrap_or(content).trim();
    // Query and fragment are not part of the file name.
    let path_part = candidate.split(['?', '#']).next().unwrap_or(candidate);
    let path = std::path::Path::new(path_part);
    image_mime_for(path)?;
    if candidate.starts_with("http://") || candidate.starts_with("https://") {
        return Some(candidate.to_owned().into());
    }
    if path.is_file() {
        Some(path.into())
    } else {
        None
    }
}

/// The image inside the entry detail: contained, with a hover download
/// button and double-click fullscreen.
fn render_entry_image(
    source: ImageSource,
    content: String,
    theme: &gpui_component::theme::Theme,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let view = cx.entity();
    let save_content = content.clone();
    let fullscreen_content = content.clone();
    div()
        .id("entry-image-frame")
        .debug_selector(|| "entry-image-frame".into())
        .group("entry-image")
        .relative()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .rounded(theme.radius_tokens().md)
        .bg(theme.muted.opacity(0.35))
        .child(
            img(source)
                .w_full()
                .h_full()
                .object_fit(ObjectFit::Contain)
                .on_click(move |event, window, cx| {
                    if event.click_count() >= 2 {
                        view.update(cx, |this, cx| {
                            this.open_image_viewer(fullscreen_content.clone(), window, cx);
                        });
                    }
                }),
        )
        .child(
            div()
                .absolute()
                .top_2()
                .right_2()
                .opacity(0.0)
                .group_hover("entry-image", |style| style.opacity(1.0))
                .child(
                    CircleAction::new("entry-image-save")
                        .secondary()
                        .icon(app_icon(IconName::ArrowDown))
                        .tooltip("Save image…")
                        .debug_selector("entry-image-save")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.save_image(save_content.clone(), cx)
                        })),
                ),
        )
        .into_any_element()
}

/// Mime for a dropped image file, when the extension is one we accept.
fn image_mime_for(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "heic" | "heif" => Some("image/heic"),
        "tif" | "tiff" => Some("image/tiff"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Resolve a dialog's design width, clamped to the window so narrow windows
/// never clip the modal.
fn dialog_width(window: &Window, width: gpui::Rems) -> Pixels {
    let available = window.viewport_size().width - Pixels::from(32.0);
    design::to_pixels(width, window).min(available)
}

/// A Gemini-like sparkle: a large star with a small one at its shoulder.
/// GPUI's icon set has stars but no sparkle glyph, so compose two.
fn sparkle_icon(color: gpui::Hsla) -> gpui::AnyElement {
    // Lucide `sparkles` served by the app asset source; the SVG's
    // `currentColor` resolves to the element's text color.
    svg()
        .path(crate::assets::SPARKLES_PATH)
        .size_4()
        .text_color(color)
        .into_any_element()
}

/// A compact action chip for the entry view's footer.
fn modal_chip(
    id: &'static str,
    label: &'static str,
    icon: Option<IconName>,
    handler: impl Fn(&mut Window, &mut App) + 'static,
) -> Button {
    let selector = id.to_owned();
    let button = Button::new(id)
        .label(label)
        .small()
        .rounded_full()
        .ghost()
        .debug_selector(move || selector.clone())
        .on_click(move |_, window, cx| handler(window, cx));
    match icon {
        Some(icon) => button.icon(app_icon(icon)),
        None => button,
    }
}

/// A 44×44 morph origin centered on a pointer position: the panel appears to
/// grow out of the card under the cursor.
fn morph_origin_at(position: Point<Pixels>) -> Bounds<Pixels> {
    let side = Pixels::from(44.0);
    Bounds::new(
        point(position.x - side / 2.0, position.y - side / 2.0),
        size(side, side),
    )
}

/// Run a blocking task on a dedicated thread and await it, so GPUI's
/// executor (no Tokio reactor) never touches blocking I/O.
async fn run_blocking<T: Send + 'static>(
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(task());
    });
    rx.await.map_err(|error| error.to_string())
}

/// Report a knowledge-build failure. Progress lives on the button's orb, so
/// the only text a failed pass owes the user is the failure itself — it lands
/// in the transcript with the rest of the run feedback.
fn report_knowledge_status(
    view: gpui::WeakEntity<WorktableView>,
    cx: &mut gpui::AsyncApp,
    status: String,
) {
    let _ = view.update(cx, |this, cx| {
        this.knowledge_building = false;
        this.knowledge_status = Some(status.clone());
        if !this
            .messages
            .iter()
            .any(|message| message.text.contains(&status))
        {
            this.messages
                .push(ChatMessage::assistant(format!("⚠ {status}")));
        }
        cx.notify();
    });
}

/// Convert tool citations into UI references. Every `search_knowledge` hit is
/// a library entry, so the click target is always the in-app
/// `worktable-entry:` scheme — the app opens the morphing entry view, whose
/// own chips carry the external URL. A citation without an entry falls back to
/// opening its URL directly.
fn citation_refs(citations: Vec<KnowledgeCitation>) -> Vec<CitationRef> {
    citations
        .into_iter()
        .map(|citation| CitationRef {
            n: citation.n,
            label: citation.label.into(),
            snippet: citation.snippet.into(),
            host: citation.host.into(),
            url: if citation.entry_id.is_empty() {
                citation.url.into()
            } else {
                format!("worktable-entry:{}", citation.entry_id).into()
            },
        })
        .collect()
}

pub(crate) fn helix_primary_topic(entry: &WorktableEntry) -> String {
    // HelixDB topic extraction — mirrors worktable_helix::topic_for_entry.
    // We compute on the fly so the UI stays in sync even before the graph is built.
    let db_entry = worktable_db::Entry {
        id: entry.id.clone(),
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

    "RESEARCH".to_owned()
}

fn library_section_heading(label: &str, theme: &gpui_component::Theme) -> impl IntoElement {
    h_flex()
        .w_full()
        .h_4()
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
        .child(div().flex_1().h_px().bg(theme.border))
}

/// Estimated height and body-line budget for one entry card.
///
/// The virtual list needs a row size before layout, so the body's wrapped
/// line count is estimated from the available text width (average glyph width
/// ≈ 0.55em) and clamped: a card never shrinks below the design minimum and
/// never grows past the maximum, while the preview clamps inside the returned
/// line budget.
fn entry_card_metrics(
    entry: &WorktableEntry,
    text_width: Pixels,
    min_height: Pixels,
    max_height: Pixels,
    window: &Window,
) -> (Pixels, usize) {
    // The component theme renders every text line at a 1.5rem line height,
    // and the topic chip adds `py_0p5` on top of the meta line.
    let body_line = design::to_pixels(rems(1.5), window);
    let meta_line = design::to_pixels(rems(1.75), window);
    let body_gap = design::to_pixels(rems(0.5), window);
    let padding = design::to_pixels(rems(1.0), window);
    let thumb = design::to_pixels(rems(2.5), window);
    let border = Pixels::from(2.0);

    // The title shares the meta row, so the header is always one line.
    let header = meta_line;
    let fixed = padding + header + body_gap + border;
    let body_budget = (max_height - fixed).max(body_line);
    let max_lines = ((body_budget / body_line).floor() as usize).max(1);

    let body = if content_is_image(&entry.content) {
        thumb
    } else {
        let preview = entry
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let font = design::to_pixels(rems(0.875), window);
        let per_line = (text_width / (font * 0.55)).floor().max(1.0) as usize;
        let chars = preview.chars().count().max(1);
        let lines = chars.div_ceil(per_line).clamp(1, max_lines);
        body_line * lines as f32
    };

    let height = (fixed + body).clamp(min_height, max_height);
    let body_lines = (((height - fixed).max(body_line)) / body_line)
        .floor()
        .max(1.0) as usize;
    (height, body_lines)
}

/// One entry card: top-aligned, height derived from its content within the
/// design bounds. Selection circle's tick is centered; the whole card carries
/// the click/context-menu wiring.
fn render_entry_card(
    entry: &WorktableEntry,
    topic: &str,
    selected: &HashSet<String>,
    show_topic: bool,
    height: Pixels,
    max_body_lines: usize,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let view = cx.entity();
    let is_selected = selected.contains(&entry.id);
    let topic = topic.to_owned();
    let is_image = content_is_image(&entry.content);

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
                    .size_3()
                    .text_color(theme.primary_foreground),
            )
        });

    // One header row for every card: the title (when present) takes the left
    // slot, the timestamp and topic chip stay right-aligned. Keeping the title
    // on this line is what makes titled and untitled cards share a layout.
    let mut meta = h_flex().w_full().min_w_0().gap_2().items_center();
    match &entry.title {
        Some(title) => {
            meta = meta.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .line_clamp(1)
                    .child(title.clone()),
            );
        }
        None => {
            meta = meta.child(div().flex_1());
        }
    }
    meta = meta.child(
        div()
            .flex_shrink_0()
            .text_xs()
            .text_color(theme.muted_foreground.opacity(0.7))
            .child(crate::format::relative_time(entry.created_at)),
    );
    // In Topic mode the section heading already names the topic, so repeating
    // it on every card is noise. The chip stays one clamped line: a long AI
    // topic must never wrap the meta row into a taller block.
    if show_topic {
        meta = meta.child(
            div()
                .max_w(rems(12.0))
                .min_w_0()
                .overflow_hidden()
                .px_2()
                .py_0p5()
                .rounded(theme.radius_tokens().sm)
                .bg(theme.muted)
                .text_xs()
                .text_color(theme.muted_foreground)
                .line_clamp(1)
                .child(topic),
        );
    }
    // Two explicit blocks: a header (title/meta row) and the body (preview or
    // image row). The list hands the card its row height and the body's line
    // budget, so heights vary between the design min and max.
    let header = v_flex().w_full().min_w_0().child(meta);

    let body: gpui::AnyElement = if is_image {
        // Thumbnail + filename (kept whole) + path (truncates).
        let filename = entry
            .content
            .split('/')
            .next_back()
            .unwrap_or(&entry.content)
            .to_owned();
        let thumbnail = image_source_for(&entry.content).map(|source| {
            let selector = format!("entry-thumb-{}", entry.id);
            let selector_for_debug = selector.clone();
            let fallback_color = theme.muted_foreground;
            div()
                .size_10()
                .flex_shrink_0()
                .overflow_hidden()
                .rounded(theme.radius_tokens().sm)
                .bg(theme.muted.opacity(0.5))
                .child(
                    img(source)
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover)
                        .with_fallback(move || {
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    Icon::new(IconName::GalleryVerticalEnd)
                                        .size_4()
                                        .text_color(fallback_color),
                                )
                                .into_any_element()
                        })
                        .debug_selector(move || selector_for_debug.clone()),
                )
        });
        let mut row = h_flex()
            .gap_2()
            .items_center()
            .w_full()
            .min_w_0()
            .overflow_hidden();
        if let Some(thumbnail) = thumbnail {
            row = row.child(thumbnail);
        } else {
            row = row.child(
                div()
                    .size_10()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme.radius_tokens().sm)
                    .bg(theme.muted.opacity(0.5))
                    .child(
                        Icon::new(IconName::GalleryVerticalEnd)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    ),
            );
        }
        row = row
            .child(
                div()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.foreground)
                    .line_clamp(1)
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
            );
        row.into_any_element()
    } else {
        // Card preview: collapse hard line breaks so the body is one flowing
        // paragraph. `line_clamp` only clamps wrapped lines — raw `\n`s render
        // full-height and overflow the row. Full content is still used for
        // copy/open.
        let preview = entry
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        div()
            .line_clamp(max_body_lines)
            .text_color(if entry.title.is_some() {
                theme.muted_foreground
            } else {
                theme.foreground
            })
            .child(preview)
            .into_any_element()
    };

    let content = v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .justify_start()
        .pt_2()
        .pb_2()
        .gap_2()
        .text_sm()
        .text_color(theme.foreground)
        .child(header)
        .child(body);

    let entry_id = entry.id.clone();
    // The accessible name is the entry's title when it has one; otherwise a
    // bounded preview, so screen readers announce the object, not the raw row.
    let card_label = entry.title.clone().unwrap_or_else(|| {
        let mut preview: String = entry.content.chars().take(120).collect();
        if entry.content.chars().count() > 120 {
            preview.push('…');
        }
        preview
    });
    let card = h_flex()
        .id(format!("entry:{}", entry.id))
        .debug_selector({
            let selector = format!("entry:{}", entry.id);
            move || selector.clone()
        })
        .role(Role::ListItem)
        .aria_label(card_label)
        .aria_selected(is_selected)
        .w_full()
        .h(height)
        .overflow_hidden()
        .items_center()
        .gap_4()
        .px_4()
        .rounded(theme.radius_tokens().md)
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .on_click(cx.listener({
            let entry_id = entry_id.clone();
            move |this, event: &ClickEvent, window, cx| {
                // Clicking the list gives it the keyboard: Tab/arrow/Enter
                // commands then act on the entries.
                let handle = this.focus_handle.clone();
                handle.focus(window, cx);
                // Double click opens the full-content morph, growing out of
                // the row's own prepainted rect.
                if event.click_count() >= 2 {
                    if this.entry_modal.is_some() {
                        return;
                    }
                    let origin = this
                        .card_bounds
                        .borrow()
                        .get(&entry_id)
                        .map(|bounds| this.local_bounds(*bounds))
                        .unwrap_or_else(|| this.pointer_origin(event.position()));
                    this.open_entry_modal(&entry_id, origin, cx);
                    return;
                }
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
        .on_mouse_pressure(cx.listener({
            let entry_id = entry_id.clone();
            move |this, event: &MousePressureEvent, _window, cx| {
                // Force click (trackpad) opens the same view. While a detail
                // overlay is open, pressure over cards outside its panel must
                // not re-open (or retrigger) the modal.
                if this.entry_modal.is_some() {
                    return;
                }
                if event.stage == PressureStage::Force {
                    let origin = this
                        .card_bounds
                        .borrow()
                        .get(&entry_id)
                        .map(|bounds| this.local_bounds(*bounds))
                        .unwrap_or_else(|| this.pointer_origin(event.position));
                    this.open_entry_modal(&entry_id, origin, cx);
                }
            }
        }))
        .child(selection)
        .child(content);

    let card = if is_selected {
        card.bg(theme.tokens.list_active)
    } else {
        card
    };

    // Hover is a paint-time style, not a colour captured when the card was
    // built: GPUI resolves it from the live hit test on every paint, so the
    // tint cannot lag the pointer, stick to a card the pointer already left,
    // or depend on how many frames a row happens to be rebuilt in.
    let hover_background = *theme.tokens.list_hover;
    let card = card.hover(move |style| style.bg(hover_background));

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

    let content_for_menu = entry.content.clone();
    let id_del = entry.id.clone();
    let id_view = entry.id.clone();
    // A right click on a selected card keeps the selection, so the menu's
    // actions apply to every selected entry.
    let menu_multi = selected.contains(&entry.id) && selected.len() > 1;
    let menu_count = selected.len();
    let copy_label = if menu_multi {
        format!("Copy {menu_count} entries")
    } else {
        "Copy".to_owned()
    };
    let delete_label = if menu_multi {
        format!("Delete {menu_count} entries")
    } else {
        "Delete".to_owned()
    };
    let card = card.context_menu(move |menu, _window, _cx| {
        let content_copy = content_for_menu.clone();
        let url_open = content_for_menu.clone();
        let is_link_menu = content_is_link(&content_for_menu);
        let id_del2 = id_del.clone();
        let id_view_menu = id_view.clone();
        let view_for_view = view.clone();
        let view_del = view.clone();
        let view_copy_list = view.clone();
        let view_copy_menu = view.clone();
        let copy_label = copy_label.clone();
        let delete_label = delete_label.clone();
        let menu_multi = menu_multi;
        menu.item(
            PopupMenuItem::new("View entry")
                .icon(Icon::new(IconName::Eye))
                .on_click(move |_event, _window, cx| {
                    let id = id_view_menu.clone();
                    view_for_view.update(cx, |this, cx| {
                        let origin = this.entry_origin(&id);
                        this.open_entry_modal(&id, origin, cx);
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::new(copy_label)
                .icon(Icon::new(IconName::Copy))
                .on_click(move |_event, _window, cx| {
                    if menu_multi {
                        view_copy_menu.update(cx, |this, cx| this.copy_selected(cx));
                    } else {
                        cx.write_to_clipboard(ClipboardItem::new_string(content_copy.clone()));
                    }
                }),
        )
        .item(
            PopupMenuItem::new("Copy as list")
                .icon(Icon::new(IconName::Copy))
                .on_click(move |_event, _window, cx| {
                    view_copy_list.update(cx, |this, cx| this.copy_entries_as_list(cx));
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
            PopupMenuItem::new(delete_label)
                .icon(Icon::new(IconName::Delete))
                .on_click(move |_event, _window, cx| {
                    let id = id_del2.clone();
                    view_del.update(cx, |this, cx| this.delete_context_target(id, cx));
                }),
        )
    });

    card.into_any_element()
}

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

fn render_chats_sheet(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> Option<gpui::AnyElement> {
    let sheet = this.chats_sheet.as_ref()?;
    if this.mode != AppMode::Assistant {
        return None;
    }
    let (elapsed, spec, opening) = match sheet.closing_at {
        Some(started) => (started.elapsed(), worktable_ui::MODAL_CLOSE, false),
        None => (sheet.opened_at.elapsed(), worktable_ui::MODAL_OPEN, true),
    };
    let reduced = worktable_ui::reduced_motion(cx);
    if !opening && (reduced || elapsed >= spec.total()) {
        return None;
    }
    if !reduced && elapsed < spec.total() {
        let _ = worktable_ui::activity_now(cx.entity_id(), cx);
    }
    let progress = if reduced {
        1.0
    } else {
        spec.progress(elapsed.as_secs_f32() / spec.total().as_secs_f32())
    };
    let t = if opening { progress } else { 1.0 - progress };
    let theme = cx.theme().clone();
    let viewport = window.viewport_size();
    let width = design::content_column_width(window);
    let height = design::to_pixels(design::CHATS_SHEET_HEIGHT, window).min(viewport.height * 0.72);
    let allowed = this.chat_switch_allowed() && opening;

    let mut body = v_flex().size_full().min_h_0().child(
        h_flex()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Chats"),
            )
            .child(
                Button::new("new-chat")
                    .label("New chat")
                    .icon(app_icon(IconName::Plus))
                    .small()
                    .ghost()
                    .disabled(!allowed)
                    .debug_selector(|| "new-chat".into())
                    .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
            )
            .child(
                CircleAction::new("chats-close")
                    .ghost()
                    .icon(app_icon(IconName::Close))
                    .tooltip("Close chats")
                    .debug_selector("chats-close")
                    .on_click(cx.listener(|this, _, window, cx| this.close_chats(window, cx))),
            ),
    );

    let status = if this.assistant_busy {
        Some("Finish or stop the current answer before switching chats.")
    } else if this.chat_save_pending {
        Some("Saving chat")
    } else if this.chat_loading.is_some() {
        Some("Loading chat")
    } else {
        None
    };
    if let Some(status) = status {
        body = body.child(
            div()
                .id("chats-status")
                .px_4()
                .py_2()
                .role(Role::Status)
                .aria_label(status)
                .debug_selector(|| "chats-status".into())
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(status),
        );
    }
    if !this.service.has_ai_runtime() {
        body = body.child(
            div()
                .id("chats-offline")
                .px_4()
                .py_2()
                .role(Role::Status)
                .aria_label("Local storage unavailable")
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Local storage is unavailable. Chats are kept only until you quit."),
        );
    }
    if let Some(error) = this.chat_save_error.clone() {
        body = body.child(
            h_flex()
                .gap_2()
                .px_4()
                .py_2()
                .child(
                    div()
                        .id("chats-save-error")
                        .flex_1()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .text_xs()
                        .text_color(theme.danger)
                        .child(error),
                )
                .child(
                    Button::new("chats-retry-save")
                        .label("Retry")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| this.save_current_chat(cx))),
                ),
        );
    }
    if let ChatListState::Failed(error) = &this.chats_state {
        body = body.child(
            h_flex()
                .gap_2()
                .px_4()
                .py_2()
                .child(
                    div()
                        .id("chats-load-error")
                        .flex_1()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error.clone()),
                )
                .child(
                    Button::new("chats-retry")
                        .label("Retry")
                        .small()
                        .ghost()
                        .debug_selector(|| "chats-retry".into())
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_chats(cx))),
                ),
        );
    }
    if this.chats.is_empty() {
        body = body.child(
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .when(matches!(this.chats_state, ChatListState::Loading), |el| {
                    el.child(
                        Button::new("chats-loading")
                            .label("Loading chats")
                            .ghost()
                            .loading(true)
                            .disabled(true),
                    )
                })
                .when(matches!(this.chats_state, ChatListState::Ready), |el| {
                    el.debug_selector(|| "chats-empty".into())
                        .child(
                            svg()
                                .path(crate::assets::CHATS_PATH)
                                .size_8()
                                .text_color(theme.muted_foreground),
                        )
                        .child(div().text_sm().child("No chats yet"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Start a conversation and it will appear here."),
                        )
                }),
        );
    } else {
        let row_h = design::to_pixels(design::CHAT_ROW_HEIGHT, window);
        let row_sizes = Rc::new(vec![size(Pixels::ZERO, row_h); this.chats.len()]);
        let list = v_virtual_list(
            cx.entity(),
            "chats-list",
            row_sizes,
            move |this, range, _, cx| {
                let theme = cx.theme();
                range
                    .map(|ix| {
                        let chat = &this.chats[ix];
                        let id = chat.id.clone();
                        let selector = format!("chat-row:{id}");
                        let active = this.chat_id.as_ref() == Some(&id);
                        div()
                            .id(SharedString::from(format!("chat-item:{id}")))
                            .h_full()
                            .px_3()
                            .py_1()
                            .role(Role::ListItem)
                            .aria_label(chat.title.clone())
                            .aria_selected(active)
                            .child(
                                Button::new(SharedString::from(format!("chat:{id}")))
                                    .ghost()
                                    .selected(active)
                                    .disabled(!allowed)
                                    .w_full()
                                    .h_full()
                                    .justify_start()
                                    .overflow_hidden()
                                    .tooltip(chat.title.clone())
                                    .debug_selector(move || selector.clone())
                                    .child(
                                        h_flex()
                                            .gap_3()
                                            .w_full()
                                            .min_w_0()
                                            .child(
                                                svg()
                                                    .path(crate::assets::CHATS_PATH)
                                                    .size_4()
                                                    .flex_shrink_0()
                                                    .text_color(theme.muted_foreground),
                                            )
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .truncate()
                                                            .child(chat.title.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(theme.muted_foreground)
                                                            .child(crate::format::relative_time(
                                                                chat.updated_at,
                                                            )),
                                                    ),
                                            )
                                            .when(active, |el| {
                                                el.child(
                                                    div()
                                                        .text_xs()
                                                        .flex_shrink_0()
                                                        .text_color(theme.muted_foreground)
                                                        .child("Current chat"),
                                                )
                                            }),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.load_chat(id.clone(), window, cx)
                                    })),
                            )
                            .into_any_element()
                    })
                    .collect()
            },
        )
        .size_full()
        .track_scroll(&this.chats_scroll);
        body = body.child(
            div()
                .id("saved-chats")
                .flex_1()
                .min_h_0()
                .relative()
                .role(Role::List)
                .aria_label("Saved chats")
                .debug_selector(|| "chats-list".into())
                .child(list)
                .child(Scrollbar::vertical(&this.chats_scroll)),
        );
    }

    // The pinned Sheet hard-codes its animation. Reuse its focus-trap behavior
    // here, with the app's motion specs and a full-height rise from the bottom.
    Some(
        div()
            .id("chats-host")
            .absolute()
            .inset_0()
            .key_context("worktable-chats")
            .track_focus(&this.chats_focus)
            .focus_trap("chats", &this.chats_focus)
            .on_action(
                cx.listener(|this, _: &crate::actions::CancelComposer, window, cx| {
                    this.close_chats(window, cx)
                }),
            )
            .child(
                div()
                    .id("chats-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .debug_selector(|| "chats-backdrop".into())
                    .bg(theme.background.opacity(0.62 * t))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_chats(window, cx)),
                    ),
            )
            .child(
                div()
                    .id("chats-sheet")
                    .absolute()
                    .occlude()
                    .role(Role::Dialog)
                    .aria_label("Chats")
                    .debug_selector(|| "chats-sheet".into())
                    .left((viewport.width - width) / 2.0)
                    .bottom(-height * (1.0 - t))
                    .w(width)
                    .h(height)
                    .min_w_0()
                    .overflow_hidden()
                    .rounded_t(theme.radius_tokens().lg)
                    .border_t_1()
                    .border_color(theme.border)
                    .bg(theme.tokens.popover)
                    .shadow_lg()
                    .child(body),
            )
            .into_any_element(),
    )
}

fn render_assistant(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let configured = this.agent_ready();

    let mut messages = v_flex()
        .id("assistant-messages")
        .debug_selector(|| "assistant-messages".into())
        .role(Role::Log)
        .aria_label("Assistant conversation")
        .size_full()
        .gap_3()
        .overflow_y_scroll()
        .track_scroll(&this.assistant_scroll)
        .p_4();

    // One shared pulse-clock phase for every animated loader in this render.
    let dots_delta = if this.assistant_busy || this.messages.iter().any(|m| m.is_thinking_only()) {
        pulse_delta(&TEXT_DOTS, cx.entity_id(), cx)
    } else {
        0.0
    };

    // Citation chips route back into the app: note citations select their
    // entry, external links open in the browser.
    let citation_open: CitationOpenHandler = {
        let view = cx.entity();
        std::sync::Arc::new(
            move |url: &str, position: Point<Pixels>, _window: &mut Window, cx: &mut App| {
                if let Some(id) = url.strip_prefix("worktable-entry:") {
                    let id = id.to_owned();
                    view.update(cx, |this, cx| {
                        // Morph from the entry's card when the Library page is
                        // mounted; otherwise grow from the click point.
                        let origin = this
                            .card_bounds
                            .borrow()
                            .get(&id)
                            .map(|bounds| this.local_bounds(*bounds))
                            .unwrap_or_else(|| this.pointer_origin(position));
                        this.open_entry_modal(&id, origin, cx);
                    });
                } else {
                    cx.open_url(url);
                }
            },
        )
    };

    if this.messages.is_empty() {
        messages = messages.child(fade_in(
            "assistant-welcome",
            div()
                .child(welcome_panel(&theme, configured))
                .when(!configured, |el| {
                    el.mt_2().child(
                        Button::new("assistant-setup-cta")
                            .label("Configure provider")
                            .icon(app_icon(IconName::Settings))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_settings_at(SettingsTab::Providers, cx)
                            })),
                    )
                }),
        ));
    } else {
        for (idx, message) in this.messages.iter().enumerate() {
            // Chat is append-only between clears, so the index is a stable
            // identity; keying on the streamed text length would restart the
            // entrance animation on every delta.
            let id = SharedString::from(format!(
                "msg-{}-{idx}",
                this.chat_id.as_deref().unwrap_or("draft")
            ));
            // The thinking header toggles this message's block; long
            // reasoning is capped and scrolls instead of freezing layout.
            let toggle: Option<ToggleThinking> = {
                let view = cx.entity();
                Some(Rc::new(move |_, _, cx| {
                    view.update(cx, |this, cx| {
                        if let Some(message) = this.messages.get_mut(idx) {
                            message.thinking_collapsed = !message.thinking_collapsed;
                        }
                        cx.notify();
                    });
                }))
            };
            let bubble = render_message(
                &theme,
                message,
                idx,
                cx.entity_id(),
                dots_delta,
                MessageOptions {
                    show_thinking: this.show_thinking,
                    citation_open: Some(citation_open.clone()),
                    on_toggle_thinking: toggle,
                },
            );
            let animated = if message.streaming {
                fade_quick(id, div().child(bubble))
            } else {
                fade_in(id, div().child(bubble))
            };
            messages = messages.child(animated);
        }
        if let Some(tool) = this.active_tool.clone() {
            // A tool is executing: the knowledge search shows the globe orb
            // (the knowledge-base loader), other tools the S1 lattice.
            let (label, variant) = if tool == "search_knowledge" {
                ("Searching your knowledge…".to_owned(), OrbVariant::G2)
            } else {
                (format!("Using {tool}…"), OrbVariant::S1)
            };
            let orb = Orb::new("assistant-tool", variant)
                .view(cx.entity_id())
                .size(rems(1.5))
                .color(theme.primary)
                .label(label)
                .surface(theme.popover)
                .label_color(theme.muted_foreground)
                .border(theme.border)
                .text_xs();
            messages = messages.child(
                h_flex()
                    .id("assistant-tool")
                    .debug_selector(|| "assistant-tool".into())
                    .role(Role::Status)
                    .aria_label("Searching the knowledge base")
                    .px_3()
                    .child(orb),
            );
        } else if this.assistant_busy {
            // Waiting for the LLM: the S1 orb pulses while no answer is
            // streaming yet. When the reasoning block is visible its shimmer
            // label already carries the activity, so the orb would repeat it.
            let waiting = this
                .messages
                .last()
                .map(|m| !m.streaming || (m.is_thinking_only() && !this.show_thinking))
                .unwrap_or(true);
            if waiting {
                let orb = Orb::new("assistant-thinking", OrbVariant::S1)
                    .view(cx.entity_id())
                    .size(rems(1.25))
                    .color(theme.muted_foreground)
                    .label("Thinking…")
                    .surface(theme.popover)
                    .label_color(theme.muted_foreground)
                    .border(theme.border)
                    .text_xs();
                messages = messages.child(
                    h_flex()
                        .id("assistant-waiting")
                        .debug_selector(|| "assistant-thinking".into())
                        .px_3()
                        .child(orb),
                );
            }
        }
    }

    // One button for both states: the inner content toggles between the
    // build icon and the G2 orb (same slot, same size), and a click while
    // building stops the pass.
    let knowledge_button = {
        let mut button = CircleAction::new("build-knowledge")
            .ghost()
            .tooltip(if this.knowledge_building {
                "Stop building knowledge"
            } else {
                "Build knowledge from entries"
            })
            .debug_selector("build-knowledge")
            .on_click(cx.listener(|this, _, _, cx| this.build_knowledge(cx)));
        if this.knowledge_building {
            button = button.child(
                Orb::new("knowledge-building", OrbVariant::G2)
                    .view(cx.entity_id())
                    .size(rems(1.5))
                    .color(theme.primary),
            );
        } else {
            button = button.icon(app_icon(IconName::HardDrive));
        }
        button
    };

    v_flex()
        .flex_1()
        .min_h_0()
        .child(
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(messages)
                .child(Scrollbar::vertical(&this.assistant_scroll)),
        )
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
                                .debug_selector(|| "assistant-input".into())
                                .child(
                                    Input::new(&this.assistant_input)
                                        .disabled(!configured)
                                        .w_full()
                                        .appearance(true)
                                        .bordered(false)
                                        .focus_bordered(false)
                                        .bg(theme.muted)
                                        .text_color(theme.foreground),
                                ),
                        )
                        .child(
                            CircleAction::new("assistant-chats")
                                .ghost()
                                .child(
                                    svg()
                                        .path(crate::assets::CHATS_PATH)
                                        .size_4()
                                        .text_color(theme.foreground),
                                )
                                .tooltip("Chats")
                                .debug_selector("assistant-chats")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_chats(window, cx)),
                                ),
                        )
                        .child(knowledge_button)
                        .child({
                            let send: gpui::AnyElement = if this.assistant_busy {
                                CircleAction::new("abort-assistant")
                                    .ghost()
                                    .child(
                                        Orb::new("assistant-send-orb", OrbVariant::S1)
                                            .view(cx.entity_id())
                                            .size(rems(1.25))
                                            .color(theme.primary),
                                    )
                                    .tooltip("Stop the assistant")
                                    .debug_selector("abort-assistant")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cancel_assistant(cx)),
                                    )
                                    .into_any_element()
                            } else {
                                CircleAction::new("send-assistant")
                                    .primary()
                                    .icon(app_icon(IconName::ArrowUp))
                                    .tooltip("Send")
                                    .debug_selector("send-assistant")
                                    .disabled(!configured)
                                    .on_click(cx.listener(|this, _, _, cx| this.send_assistant(cx)))
                                    .into_any_element()
                            };
                            send
                        }),
                )
                .when_some(this.chat_save_error.clone(), |el, error| {
                    el.child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .id("chat-save-error")
                                    .role(Role::Alert)
                                    .aria_label(error.clone())
                                    .text_xs()
                                    .text_color(theme.danger)
                                    .child(error),
                            )
                            .child(
                                Button::new("retry-chat-save")
                                    .label("Retry")
                                    .small()
                                    .ghost()
                                    .debug_selector(|| "retry-chat-save".into())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.save_current_chat(cx)),
                                    ),
                            ),
                    )
                }),
        )
        .into_any_element()
}

/// The body of the provider configuration dialog.
///
/// It is a separate entity because dialogs are built while the main view is
/// still rendering; reading the main view from the dialog builder itself would
/// borrow it twice. This view only holds a weak handle and reads state when
/// its own render runs.
struct ProviderDialogView {
    main: WeakEntity<WorktableView>,
    provider_id: String,
    api_key_input: Entity<InputState>,
    /// When the dialog opened — drives the transitions.dev modal entrance
    /// (250ms, `cubic-bezier(0.22,1,0.36,1)`; the CSS scale is approximated
    /// with a 4px rise because gpui divs have no scale transform).
    opened_at: Instant,
}

impl ProviderDialogView {
    fn provider(&self, cx: &App) -> Option<ProviderInfo> {
        self.main
            .upgrade()?
            .read(cx)
            .providers
            .iter()
            .find(|provider| provider.id == self.provider_id)
            .cloned()
    }
}

impl Render for ProviderDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = cx.theme().clone();

        // transitions.dev modal entrance: fade + gentle rise over 250ms.
        let elapsed = self.opened_at.elapsed();
        let opening = elapsed < worktable_ui::MODAL_OPEN.total();
        if opening {
            // Keep frames coming for the short entrance.
            let _ = worktable_ui::activity_now(cx.entity_id(), cx);
        }
        let entrance = if opening {
            worktable_ui::MODAL_OPEN
                .progress(elapsed.as_secs_f32() / worktable_ui::MODAL_OPEN.total().as_secs_f32())
        } else {
            1.0
        };

        let Some(main) = self.main.upgrade() else {
            return div().into_any_element();
        };
        let Some(provider) = self.provider(cx) else {
            return div().into_any_element();
        };
        let (status, logging_in) = {
            let state = main.read(cx);
            (
                state.provider_dialog_status.clone(),
                state.logging_in.contains(&provider.id),
            )
        };
        let login_panel = render_active_login(&main, &provider.id, cx);

        let mut column = v_flex()
            .debug_selector(|| "provider-dialog".into())
            .relative()
            .top(rems(0.25 * (1.0 - entrance)))
            .opacity(entrance)
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Choose how the assistant authenticates with {}.",
                        provider.name
                    )),
            );

        if provider.supports_api_key {
            let hint = if provider.api_key_set {
                "An API key is saved. Enter a new key to replace it."
            } else {
                "Paste the API key from your provider account."
            };
            column = column.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(hint),
            );
            let main_for_save = main.clone();
            column = column.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.api_key_input)),
                    )
                    .child(
                        Button::new("save-provider-key")
                            .label("Save key")
                            .primary()
                            .debug_selector(|| "save-provider-key".into())
                            .on_click(move |_, _, cx| {
                                main_for_save.update(cx, |this, cx| this.save_api_key(cx));
                            }),
                    ),
            );
        }

        if provider.supports_oauth {
            let main_for_auth = main.clone();
            if provider.oauth_set {
                let provider_id = provider.id.clone();
                column = column.child(
                    Button::new("provider-sign-out")
                        .label("Sign out")
                        .ghost()
                        .on_click(move |_, _, cx| {
                            let provider_id = provider_id.clone();
                            main_for_auth
                                .update(cx, |this, cx| this.logout_provider(&provider_id, cx));
                        }),
                );
            } else if logging_in {
                let provider_id = provider.id.clone();
                column = column.child(
                    Button::new("provider-cancel-login")
                        .label("Cancel sign-in")
                        .ghost()
                        .on_click(move |_, _, cx| {
                            let provider_id = provider_id.clone();
                            main_for_auth
                                .update(cx, |this, cx| this.cancel_login(&provider_id, cx));
                        }),
                );
            } else {
                let provider_id = provider.id.clone();
                column = column.child(
                    Button::new("provider-sign-in")
                        .label("Sign in with OAuth")
                        .icon(app_icon(IconName::ExternalLink))
                        .on_click(move |_, _, cx| {
                            let provider_id = provider_id.clone();
                            main_for_auth.update(cx, |this, cx| this.login_oauth(&provider_id, cx));
                        }),
                );
            }
        }

        if let Some(panel) = login_panel {
            column = column.child(panel);
        }
        if let Some(status) = status {
            column = column.child(
                div()
                    .id("provider-dialog-status")
                    .role(Role::Status)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            );
        }

        column.into_any_element()
    }
}

/// The GitHub stars dialog body: account, fetch, the starred-repository
/// list, and the import action.
///
/// Own entity like the provider dialog, so the dialog layer can render it
/// while the main view is mid-render.
struct GithubStarsDialog {
    main: WeakEntity<WorktableView>,
}

impl Render for GithubStarsDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = cx.theme().clone();
        let Some(main) = self.main.upgrade() else {
            return div().into_any_element();
        };
        let (input, username, error, repos, loading, importing, import_status, total) = {
            let this = main.read(cx);
            (
                this.github_input.clone(),
                this.github_username.clone(),
                this.github_error.clone(),
                this.github_repos.clone(),
                this.github_loading,
                this.github_importing,
                this.github_import_status.clone(),
                this.github_total_stars,
            )
        };

        let main_for_fetch = main.clone();
        let fetch = Button::new("github-fetch")
            .label("Fetch stars")
            .small()
            .loading(loading)
            .debug_selector(|| "github-fetch".into())
            .on_click(move |_, _, cx| {
                main_for_fetch.update(cx, |this, cx| {
                    let value = this.github_input.read(cx).value().to_string();
                    if value.trim().is_empty() {
                        this.fetch_github_stars(cx);
                    } else {
                        this.save_github_username(cx);
                    }
                });
            });

        let main_for_import = main.clone();
        let import_enabled = username.as_deref().is_some_and(|user| !user.is_empty());
        let import = Button::new("github-import-stars")
            .label("Import stars to entries")
            .primary()
            .small()
            .loading(importing)
            .disabled(!import_enabled)
            .debug_selector(|| "github-import-stars".into())
            .on_click(move |_, _, cx| {
                main_for_import.update(cx, |this, cx| this.import_github_stars(cx));
            });

        let repo_list: gpui::AnyElement = if repos.is_empty() {
            div().into_any_element()
        } else {
            div()
                .id("github-repos")
                .debug_selector(|| "github-repos".into())
                .max_h(rems(12.0))
                .overflow_y_scrollbar()
                .rounded(theme.radius_tokens().md)
                .border_1()
                .border_color(theme.border)
                .children(repos.iter().take(100).enumerate().map(|(index, repo)| {
                    let url = repo.html_url.clone();
                    h_flex()
                        .id(gpui::ElementId::Name(format!("github-repo-{index}").into()))
                        .cursor_pointer()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .border_b_1()
                        .border_color(theme.border.opacity(0.4))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_ellipsis()
                                .text_sm()
                                .child(repo.name.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("★ {}", repo.stars)),
                        )
                }))
                .into_any_element()
        };

        let mut column = v_flex()
            .debug_selector(|| "github-dialog".into())
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Import the repositories you starred on GitHub as entries."),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("GitHub username"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .debug_selector(|| "github-username".into())
                                    .child(Input::new(&input).w_full()),
                            )
                            .child(fetch),
                    ),
            );
        if let Some(username) = username.filter(|user| !user.is_empty()) {
            column = column.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Account: {username}")),
            );
        }
        if let Some(error) = error {
            column = column.child(
                div()
                    .id("github-dialog-error")
                    .role(Role::Alert)
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error),
            );
        }
        if let Some(total) = total.filter(|_| !repos.is_empty()) {
            column = column.child(
                div()
                    .id("github-dialog-summary")
                    .role(Role::Status)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} starred repositories · {total} stars total",
                        repos.len()
                    )),
            );
        }
        column = column.child(repo_list).child(import);
        if let Some(status) = import_status {
            column = column.child(
                div()
                    .id("github-dialog-status")
                    .role(Role::Status)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            );
        }
        column.into_any_element()
    }
}
