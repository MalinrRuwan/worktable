//! The main Worktable view: sidebar navigation, searchable entries list,
//! inline composer, the AI assistant pane, and the AI provider settings panel.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Render,
    SharedString, Size, StatefulInteractiveElement as _, Styled, Subscription, Window, div, px,
    relative, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu, PopupMenuItem};
use gpui_component::scroll::{ScrollableElement as _, Scrollbar, ScrollbarAxis};
use gpui_component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu,
    SidebarMenuItem, SidebarToggleButton,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, TitleBar, VirtualListScrollHandle, h_flex, v_flex,
    v_virtual_list,
};
use worktable_ai::WorktableEntry;
use worktable_events::{
    AuthNotifyKind, AuthPromptKind, ProviderInfo, ProvidersSnapshot, WorktableEvent,
};
use worktable_ui::{
    GRADIENT_SPIN, ZERON_PULSE, dialog_in, fade_in, fade_quick, hover_blend, hover_fades_active,
    hover_listener, menu_in, pulse_delta, splash_out,
};

use crate::assistant::{ChatMessage, Role, render_message, welcome_panel};
use crate::format::relative_time;
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SettingsTab {
    Ui,
    Data,
    Providers,
}

const PROVIDER_ROW_HEIGHT: f32 = 56.0;

fn app_icon(name: IconName) -> Icon {
    Icon::new(name).size(px(16.))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComposerKind {
    Note,
    Link,
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
    service: Arc<WorktableService>,
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

    // UI state
    pub(crate) sidebar_collapsed: bool,
    // Dwell-open state: hover for 300ms opens, leave for 400ms re-collapses.
    // `sidebar_dwell_opened` tracks whether the current expansion was caused
    // by dwell so a leave only collapses dwell-opened sidebars, not a manual
    // `cmd-shift-s` expansion.
    sidebar_dwell_hovered: bool,
    sidebar_dwell_seq: u64,
    sidebar_dwell_opened: bool,
    pub(crate) dark_mode: bool,
    settings_tab: SettingsTab,
    // UI settings (exposed in Settings → UI, persisted via wt_ai_config)
    dwell_enabled: bool,
    dwell_open_ms: u64,
    dwell_close_ms: u64,
    settings_scroll: VirtualListScrollHandle,
    provider_row_sizes: Rc<Vec<Size<Pixels>>>,
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
                .placeholder("Search entries…")
                .clean_on_escape()
        });
        let composer_body = cx.new(|cx| InputState::new(window, cx).placeholder("Content…"));
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
            sidebar_collapsed: false,
            sidebar_dwell_hovered: false,
            sidebar_dwell_seq: 0,
            sidebar_dwell_opened: false,
            dark_mode: false,
            settings_tab: SettingsTab::Ui,
            dwell_enabled: true,
            dwell_open_ms: 300,
            dwell_close_ms: 400,
            settings_scroll: VirtualListScrollHandle::new(),
            provider_row_sizes: Rc::new(Vec::new()),
            splash_start: Some(Instant::now()),

            _subscriptions: Vec::new(),
        };

        view.subscribe(window, cx);
        view.load_entries(cx);
        view.refresh_providers(cx);
        view.load_github_username(cx);
        view.load_ui_settings(cx);
        view
    }

    fn load_ui_settings(&mut self, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        cx.spawn(async move |view, cx| {
            let dwell_enabled = service
                .get_config("ui_dwell_enabled")
                .await
                .ok()
                .flatten()
                .map(|v| v != "0" && v != "false")
                .unwrap_or(true);
            let dwell_open = service
                .get_config("ui_dwell_open_ms")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(300)
                .clamp(100, 800);
            let dwell_close = service
                .get_config("ui_dwell_close_ms")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(400)
                .clamp(100, 800);
            let _ = view.update(cx, |this, cx| {
                this.dwell_enabled = dwell_enabled;
                this.dwell_open_ms = dwell_open;
                this.dwell_close_ms = dwell_close;
                cx.notify();
            });
        })
        .detach();
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
        for field in [composer_body] {
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
        let input = self.github_input.clone();
        cx.spawn(async move |view, cx| {
            let result = service.get_github_username().await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(Some(username)) if !username.trim().is_empty() => {
                        let username = username.trim().to_owned();
                        this.github_username = Some(username.clone());
                        this.set_input(&input, &username, cx);
                        // Auto-fetch stars for the stored username (non-blocking, updates UI when done).
                        this.fetch_github_stars(cx);
                    }
                    Ok(Some(username)) => {
                        let username = username.trim().to_owned();
                        if !username.is_empty() {
                            this.github_username = Some(username.clone());
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

    fn visible_entries(&self) -> Vec<&WorktableEntry> {
        let query = self.query.trim().to_lowercase();
        self.entries
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
            .collect()
    }

    fn visible_entry_ids(&self) -> Vec<String> {
        self.visible_entries()
            .into_iter()
            .map(|entry| entry.id.clone())
            .collect()
    }

    fn selected_entry(&self) -> Option<&WorktableEntry> {
        let anchor = self
            .selected_anchor
            .as_ref()
            .or_else(|| self.selected.iter().next())?;
        self.entries.iter().find(|entry| &entry.id == anchor)
    }

    fn focus(&mut self, window: &mut Window, cx: &mut App) {
        let _ = self;
        let _ = window;
        let _ = cx;
    }

    // ---- Public action methods (called from app-level handlers) ------------

    pub fn open_composer_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(ComposerKind::Note, window, cx);
    }

    pub fn open_composer_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(ComposerKind::Link, window, cx);
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search_input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn show_entries(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::Entries;
        cx.notify();
    }

    pub fn show_assistant(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::Assistant;
        cx.notify();
    }

    pub fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::Settings;
        if self.providers.is_empty() {
            self.refresh_providers(cx);
        }
        cx.notify();
    }

    pub fn show_provider_config(&mut self, cx: &mut Context<Self>) {
        self.mode = AppMode::ProviderConfig;
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
        self.mode = AppMode::ProviderConfig;
        cx.notify();
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        // Manual toggle cancels any pending dwell timer and clears the
        // dwell-opened flag so a subsequent hover-leave does not collapse a
        // user-intended expansion.
        self.sidebar_dwell_opened = false;
        self.sidebar_dwell_seq = self.sidebar_dwell_seq.wrapping_add(1);
        cx.notify();
    }

    /// Handle hover changes for the sidebar dwell-open behavior.
    ///
    /// When the sidebar is collapsed and the mouse dwells over its 52px icon
    /// strip for 300ms, the sidebar expands. When the mouse leaves the expanded
    /// sidebar for 400ms, it re-collapses — but only if the expansion was
    /// caused by dwell, so a `cmd-shift-s` expansion is sticky until the user
    /// toggles again.
    fn handle_sidebar_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if !self.dwell_enabled {
            return;
        }
        self.sidebar_dwell_hovered = hovered;
        self.sidebar_dwell_seq = self.sidebar_dwell_seq.wrapping_add(1);
        let seq = self.sidebar_dwell_seq;

        if hovered {
            if self.sidebar_collapsed {
                let delay = Duration::from_millis(self.dwell_open_ms);
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(delay).await;
                    let _ = view.update(cx, |this, cx| {
                        if this.sidebar_dwell_seq != seq {
                            return;
                        }
                        if !this.sidebar_dwell_hovered {
                            return;
                        }
                        if this.sidebar_collapsed {
                            this.sidebar_collapsed = false;
                            this.sidebar_dwell_opened = true;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        } else if !self.sidebar_collapsed && self.sidebar_dwell_opened {
            let delay = Duration::from_millis(self.dwell_close_ms);
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(delay).await;
                let _ = view.update(cx, |this, cx| {
                    if this.sidebar_dwell_seq != seq {
                        return;
                    }
                    if this.sidebar_dwell_hovered {
                        return;
                    }
                    if !this.sidebar_collapsed && this.sidebar_dwell_opened {
                        this.sidebar_collapsed = true;
                        this.sidebar_dwell_opened = false;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
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
        if let Some(entry) = self.selected_entry() {
            if entry.kind == "link" {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(entry.content.clone()));
            }
        }
    }

    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.selected_entry() {
            if entry.kind == "link" {
                cx.open_url(&entry.content);
            }
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
                self.selected_anchor = Some(id);
            }
        } else {
            self.selected.clear();
            self.selected.insert(id.clone());
            self.selected_anchor = Some(id);
        }
    }

    fn is_selected(&self, id: &str) -> bool {
        self.selected.contains(id)
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
        self.provider_row_sizes = Rc::new(
            (0..self.providers.len())
                .map(|_| size(px(0.), px(PROVIDER_ROW_HEIGHT)))
                .collect(),
        );
        self.active_provider = snapshot.active_provider.clone();
        self.active_model = snapshot.active_model.clone();
        // Drop any api-key entry row that no longer exists.
        if let Some(provider) = &self.api_key_provider {
            if !self.providers.iter().any(|p| &p.id == provider) {
                self.api_key_provider = None;
            }
        }
    }

    pub fn send_assistant(&mut self, cx: &mut Context<Self>) {
        if !self.service.has_ai_worker() {
            self.messages.push(ChatMessage::assistant(
                "The AI assistant isn't configured yet. Open Settings to pick a provider.",
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
            if let Err(error) = result {
                let _ = view.update(cx, |this, cx| {
                    this.assistant_busy = false;
                    if !this.messages.iter().any(|m| m.text.contains(&error)) {
                        this.messages
                            .push(ChatMessage::assistant(format!("⚠ {error}")));
                    }
                    cx.notify();
                });
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
        let body = trim_opt(&self.composer_body.read(cx).value().to_string());

        let (valid, content, entry_kind) = match kind {
            ComposerKind::Note => match body {
                Some(content) => (true, content, "text"),
                None => (false, String::new(), "text"),
            },
            ComposerKind::Link => match body {
                Some(url) => (true, url, "link"),
                None => (false, String::new(), "link"),
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
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.selected.clear();
                    this.selected.insert(entry.id.clone());
                    this.selected_anchor = Some(entry.id);
                    this.mode = AppMode::Entries;
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
            if kind == ComposerKind::Note {
                state.set_placeholder("Write your note…", window, cx);
            } else {
                state.set_placeholder("https://…  (or a plain link)", window, cx);
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
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.selected.clear();
                    this.selected.insert(entry.id.clone());
                    this.selected_anchor = Some(entry.id);
                    this.mode = AppMode::Entries;
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
        cx.spawn(async move |view, cx| {
            let result = service.insert_entry(entry.clone()).await;
            let _ = view.update(cx, |this, cx| {
                if result.is_ok() {
                    this.entries.insert(0, entry.clone());
                    this.selected.clear();
                    this.selected.insert(entry.id.clone());
                    this.selected_anchor = Some(entry.id);
                    this.mode = AppMode::Entries;
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
        if let Some(start) = self.splash_start {
            if start.elapsed() > Duration::from_millis(650) {
                self.splash_start = None;
            }
        }
        let theme = cx.theme().clone();
        let is_splash = self.splash_start.is_some();

        div()
            .id("worktable-root")
            .size_full()
            .flex()
            .relative()
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
                cx.listener(|this, _: &crate::actions::ToggleSidebar, _, cx| {
                    this.toggle_sidebar(cx)
                }),
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
                div()
                    .id("worktable-sidebar-dwell")
                    .flex_shrink_0()
                    .h_full()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.handle_sidebar_hover(*hovered, cx);
                    }))
                    .child(render_sidebar(self, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
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
                        .bg(theme.tokens.background.clone())
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

fn render_sidebar(this: &WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme();

    let brand = div()
        .flex()
        .items_center()
        .justify_center()
        .size_7()
        .flex_shrink_0()
        .rounded(theme.radius)
        .bg(theme.sidebar_primary)
        .text_color(theme.sidebar_primary_foreground)
        .child(app_icon(IconName::GalleryVerticalEnd));

    let header = if this.sidebar_collapsed {
        h_flex().justify_center().child(brand).into_any_element()
    } else {
        h_flex()
            .gap_2()
            .items_center()
            .child(brand)
            .child(
                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Worktable"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Your notes"),
                    ),
            )
            .into_any_element()
    };

    let footer = if this.sidebar_collapsed {
        h_flex()
            .justify_center()
            .child(app_icon(IconName::CircleUser))
            .into_any_element()
    } else {
        h_flex()
            .gap_2()
            .text_sm()
            .child(app_icon(IconName::CircleUser))
            .child(div().text_color(theme.muted_foreground).child(
                if this.service.is_persistent() {
                    "Connected"
                } else {
                    "Local"
                },
            ))
            .into_any_element()
    };

    Sidebar::new("worktable-sidebar")
        .collapsible(SidebarCollapsible::Icon)
        .collapsed(this.sidebar_collapsed)
        .w(px(220.))
        .header(SidebarHeader::new().child(header))
        .child(
            SidebarGroup::new("Workspace").child(SidebarMenu::new().children([
                menu_item(
                    "Entries",
                    IconName::Inbox,
                    AppMode::Entries,
                    this.mode,
                    cx.listener(|this, _, _, cx| this.show_entries(cx)),
                ),
                menu_item(
                    "AI Assistant",
                    IconName::Bot,
                    AppMode::Assistant,
                    this.mode,
                    cx.listener(|this, _, _, cx| this.show_assistant(cx)),
                ),
                menu_item(
                    "Settings",
                    IconName::Settings,
                    AppMode::Settings,
                    this.mode,
                    cx.listener(|this, _, _, cx| this.show_settings(cx)),
                ),
            ])),
        )
        .footer(SidebarFooter::new().child(footer))
}

fn menu_item(
    label: &'static str,
    icon: IconName,
    mode: AppMode,
    active: AppMode,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> SidebarMenuItem {
    SidebarMenuItem::new(label)
        .icon(app_icon(icon))
        .active(active == mode || (mode == AppMode::Settings && active == AppMode::ProviderConfig))
        .on_click(on_click)
}

fn render_main(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let root_settings_breadcrumbs =
        if this.mode == AppMode::Settings || this.mode == AppMode::ProviderConfig {
            Some(
                h_flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .id("breadcrumb-home")
                            .cursor_pointer()
                            .text_color(theme.muted_foreground)
                            .hover(|s| s.text_color(theme.foreground))
                            .child("Home")
                            .on_click(cx.listener(|this, _, _, cx| this.show_entries(cx))),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size(px(14.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .id("breadcrumb-settings")
                            .cursor_pointer()
                            .text_color(if this.mode == AppMode::Settings {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .hover(|s| s.text_color(theme.foreground))
                            .child("Settings")
                            .on_click(cx.listener(|this, _, _, cx| this.show_settings(cx))),
                    )
                    .when(this.mode == AppMode::ProviderConfig, |this| {
                        this.child(
                            Icon::new(IconName::ChevronRight)
                                .size(px(14.))
                                .text_color(theme.muted_foreground),
                        )
                        .child(div().text_color(theme.foreground).child("Providers"))
                    }),
            )
        } else {
            None
        };

    let mut title_leading = h_flex().items_center().gap_2().child(
        SidebarToggleButton::new()
            .collapsed(this.sidebar_collapsed)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
    );
    if let Some(breadcrumbs) = root_settings_breadcrumbs {
        title_leading = title_leading.child(breadcrumbs);
    }

    let title_bar = TitleBar::new().child(title_leading).child(
        h_flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("title-search")
                    .icon(app_icon(IconName::Search))
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| this.focus_search(window, cx))),
            )
            .child(
                Button::new("title-more")
                    .icon(app_icon(IconName::Ellipsis))
                    .ghost(),
            ),
    );

    let entries_toolbar = if this.mode == AppMode::Entries {
        h_flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div().flex_1().child(
                    Input::new(&this.search_input)
                        .prefix(Icon::new(IconName::Search).text_color(theme.muted_foreground))
                        .cleanable(true),
                ),
            )
            .child(new_menu_button(cx))
            .into_any_element()
    } else {
        div().into_any_element()
    };

    let content = match this.mode {
        AppMode::Entries => render_entries(this, window, cx).into_any_element(),
        AppMode::Assistant => render_assistant(this, cx).into_any_element(),
        AppMode::Settings => render_settings(this, window, cx).into_any_element(),
        AppMode::ProviderConfig => render_provider_config(this, window, cx).into_any_element(),
    };

    v_flex()
        .flex_1()
        .min_w_0()
        .size_full()
        .child(title_bar)
        .child(entries_toolbar)
        .child(content)
        .child(if this.mode == AppMode::Entries {
            composer_bar(this, cx).into_any_element()
        } else {
            div().into_any_element()
        })
}

fn new_menu_button(cx: &mut Context<WorktableView>) -> impl IntoElement {
    let view = cx.entity();
    Button::new("new-entry")
        .label("New")
        .icon(app_icon(IconName::Plus))
        .primary()
        .dropdown_menu(move |menu, _, _| {
            let entity = view.clone();
            menu.menu_element_with_icon(
                app_icon(IconName::File),
                Box::new(crate::actions::NewNote),
                move |_, cx| {
                    let _ = entity.update(cx, |_, _| {});
                    menu_in(
                        "new-note-menu",
                        div().child("New Note").child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("⌘N"),
                        ),
                    )
                },
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
                        let _ = view.update(cx, |this, cx| this.refresh_providers(cx));
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

    let github = render_github_section(this, _window, cx);

    // Settings tab bar — UI / Data / Providers
    let tab_bar = h_flex()
        .gap_1()
        .p_1()
        .bg(theme.tokens.background)
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border.opacity(0.6))
        .child({
            let active = this.settings_tab == SettingsTab::Ui;
            let mut btn = Button::new("settings-tab-ui")
                .label("UI")
                .icon(app_icon(IconName::Palette))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings_tab = SettingsTab::Ui;
                    cx.notify();
                }));
            if active {
                btn = btn.primary();
            } else {
                btn = btn.ghost();
            }
            btn
        })
        .child({
            let active = this.settings_tab == SettingsTab::Data;
            let mut btn = Button::new("settings-tab-data")
                .label("Data")
                .icon(app_icon(IconName::HardDrive))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings_tab = SettingsTab::Data;
                    cx.notify();
                }));
            if active {
                btn = btn.primary();
            } else {
                btn = btn.ghost();
            }
            btn
        })
        .child({
            let active = this.settings_tab == SettingsTab::Providers;
            let mut btn = Button::new("settings-tab-providers")
                .label("Providers")
                .icon(app_icon(IconName::Bot))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings_tab = SettingsTab::Providers;
                    cx.notify();
                }));
            if active {
                btn = btn.primary();
            } else {
                btn = btn.ghost();
            }
            btn
        });

    let ui_section = v_flex().gap_4().p_4().child(
        v_flex()
            .gap_2()
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("UI"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Sidebar, theme, and motion"),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .child("Sidebar dwell (hover 300ms→open, 400ms→close)"),
                    )
                    .child(
                        Button::new("toggle-dwell")
                            .label(if this.dwell_enabled { "On" } else { "Off" })
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dwell_enabled = !this.dwell_enabled;
                                this.save_ui_setting(
                                    "ui_dwell_enabled",
                                    if this.dwell_enabled { "1" } else { "0" },
                                    cx,
                                );
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "Open {}ms / Close {}ms",
                                this.dwell_open_ms, this.dwell_close_ms
                            )),
                    )
                    .child(
                        Button::new("dwell-faster")
                            .label("-50ms")
                            .ghost()
                            .h(px(24.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dwell_open_ms = this.dwell_open_ms.saturating_sub(50).max(100);
                                this.dwell_close_ms =
                                    this.dwell_close_ms.saturating_sub(50).max(100);
                                this.save_ui_setting(
                                    "ui_dwell_open_ms",
                                    &this.dwell_open_ms.to_string(),
                                    cx,
                                );
                                this.save_ui_setting(
                                    "ui_dwell_close_ms",
                                    &this.dwell_close_ms.to_string(),
                                    cx,
                                );
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("dwell-slower")
                            .label("+50ms")
                            .ghost()
                            .h(px(24.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dwell_open_ms = (this.dwell_open_ms + 50).min(800);
                                this.dwell_close_ms = (this.dwell_close_ms + 50).min(800);
                                this.save_ui_setting(
                                    "ui_dwell_open_ms",
                                    &this.dwell_open_ms.to_string(),
                                    cx,
                                );
                                this.save_ui_setting(
                                    "ui_dwell_close_ms",
                                    &this.dwell_close_ms.to_string(),
                                    cx,
                                );
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().child("Theme"))
                    .child(
                        Button::new("toggle-theme")
                            .label(if this.dark_mode { "Dark" } else { "Light" })
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
                    ),
            ),
    );

    let data_section = v_flex().gap_4().p_4().child(
        v_flex()
            .gap_2()
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Data"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("GitHub stars and local database"),
            )
            .child(github),
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

    v_flex()
        .flex_1()
        .overflow_y_scrollbar()
        .p_4()
        .gap_4()
        .child(v_flex().items_center().child(tab_bar))
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
                        let _ = view.update(cx, |this, cx| this.refresh_providers(cx));
                    }),
            )
            .into_any_element();
    }

    let view = cx.entity();
    let back_button = Button::new("back-to-settings")
        .label("Back to Settings")
        .ghost()
        .on_click(move |_, _, cx| {
            let _ = view.update(cx, |this, cx| this.show_settings(cx));
        });

    let provider_heading = h_flex()
        .items_center()
        .justify_between()
        .px_4()
        .pt_3()
        .child(
            v_flex()
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

    let github = render_github_section(this, _window, cx);

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
        )
        .child(
            div()
                .px_4()
                .pt_3()
                .pb_3()
                .border_t_1()
                .border_color(theme.border.opacity(0.5))
                .child(github),
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

    let mut left = h_flex().gap_2().items_center();
    left = left.child(
        div()
            .text_sm()
            .font_weight(gpui::FontWeight::BOLD)
            .child(name),
    );
    if !badge_text.is_empty() {
        left = left.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
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
                let _ = configure_view.update(cx, |this, cx| {
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
                                let _ = view.update(cx, |this, cx| this.save_api_key(cx));
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
                        let _ = view.update(cx, |this, cx| {
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
                        let _ = view.update(cx, |this, cx| this.logout_provider(&provider_id, cx));
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
                        let _ = view.update(cx, |this, cx| this.cancel_login(&provider_id, cx));
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
                        let _ = view.update(cx, |this, cx| this.login_oauth(&provider_id, cx));
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
                        let _ = view.update(cx, |this, cx| {
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
    if let Some(notice) = &this.auth_notice {
        if notice.provider_id == provider_id {
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
                                    let _ =
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
                                    let _ = view.update(cx, |this, cx| {
                                        this.open_auth_url(&verification_uri, cx)
                                    });
                                }),
                        );
                }
                _ => {}
            }
        }
    }

    if let Some(pending) = &this.pending_prompt {
        if pending.provider_id == provider_id {
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
                                        let _ = view.update(cx, |this, cx| this.answer_prompt(cx));
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
                                        let _ = view.update(cx, |this, cx| {
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

fn render_github_section(
    this: &WorktableView,
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
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("GitHub Stars"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Total stars + per-repo breakdown"),
                ),
        )
        .child(div().text_xs().text_color(theme.muted_foreground).child(
            "Stored in wt_ai_config (github_username). Uses GITHUB_TOKEN if set for 5000 req/h.",
        ));

    // Input + Save + Fetch row
    let save_view = view.clone();
    let fetch_view = view.clone();
    let input_row = h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .max_w(px(220.))
                .child(Input::new(&this.github_input).h(px(32.))),
        )
        .child(
            Button::new("github-save")
                .label("Save")
                .icon(app_icon(IconName::Check))
                .h(px(28.))
                .on_click(move |_, _, cx| {
                    let _ = save_view.update(cx, |this, cx| this.save_github_username(cx));
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
                .disabled(this.github_loading)
                .on_click(move |_, _, cx| {
                    let _ = fetch_view.update(cx, |this, cx| this.fetch_github_stars(cx));
                }),
        );

    column = column.child(input_row);

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
                            let _ = view_for_click.update(cx, |this, cx| {
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

    // Outer wrapper ensures a sensible max width in Settings (centered).
    div()
        .w_full()
        .max_w(px(520.))
        .child(column)
        .into_any_element()
}

fn composer_bar(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let Some(kind) = this.composer else {
        return div().into_any_element();
    };

    let fields = v_flex()
        .flex_1()
        .gap_2()
        .child(Input::new(&this.composer_body).h(px(34.)));

    let composer_id = match kind {
        ComposerKind::Note => "composer-note",
        ComposerKind::Link => "composer-link",
    };
    dialog_in(
        composer_id,
        h_flex()
            .items_end()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .child(fields)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("cancel-composer")
                            .label("Cancel")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_composer(cx))),
                    )
                    .child(
                        Button::new("submit-composer")
                            .label("Save")
                            .primary()
                            .icon(app_icon(IconName::Check))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_composer(cx))),
                    ),
            ),
    )
    .into_any_element()
}

fn render_entries(
    this: &mut WorktableView,
    _window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let visible = this.visible_entries();

    if visible.is_empty() {
        let empty = if this.query.is_empty() {
            "No entries yet — press ⌘N to create one."
        } else {
            "No entries match your search."
        };
        return v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_color(theme.muted_foreground)
            .child(div().text_sm().child(empty))
            .into_any_element();
    }

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
            visible.iter().copied().collect()
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

    let items = visible
        .into_iter()
        .map(|entry| {
            let entry_id = entry.id.clone();
            let entry_card = render_entry_card(
                entry,
                &this.selected,
                &theme,
                cx.listener({
                    let entry_id = entry_id.clone();
                    move |this, event: &ClickEvent, _window, cx| {
                        let extend = event.modifiers().shift || is_shift_held();
                        this.select_at(entry_id.clone(), extend);
                        cx.notify();
                    }
                }),
            );
            // Shift+right-click context menu: copy visible entries as list.
            // We wrap the card with a context menu that only materializes when Shift is held.
            let text_for_menu = clipboard_text.clone();
            // Also support direct Shift+right-click via mouse_down that copies without opening menu.
            let text_for_direct = clipboard_text.clone();
            let card_with_menu = div()
                .child(entry_card)
                .on_mouse_down(MouseButton::Right, move |_event, window, cx| {
                    if is_shift_held() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text_for_direct.clone()));
                        // Prevent propagation so the plain right-click doesn't also trigger other handlers.
                        window.refresh();
                    }
                })
                .context_menu(move |menu, _window, _cx| {
                    if !is_shift_held() {
                        return menu;
                    }
                    let text = text_for_menu.clone();
                    menu.item(PopupMenuItem::new("Copy as list").on_click(
                        move |_event, _window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        },
                    ))
                });
            card_with_menu.into_any_element()
        })
        .collect::<Vec<_>>();

    v_flex()
        .id("entries-scroll")
        .p_4()
        .gap_2()
        .overflow_y_scroll()
        .children(items)
        .into_any_element()
}

fn render_entry_card(
    entry: &WorktableEntry,
    selected: &HashSet<String>,
    theme: &gpui_component::Theme,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let kind = entry_kind(&entry.kind);
    let is_selected = selected.contains(&entry.id);

    let mut row = div()
        .id(format!("entry:{}", entry.id))
        .flex()
        .flex_col()
        .gap_2()
        .rounded(px(6.))
        .px_3()
        .py_3()
        .on_click(listener)
        .cursor_pointer()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(entry_kind_color(kind, theme))
                        .child(kind),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(if is_selected {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        })
                        .child(relative_time(entry.created_at)),
                ),
        );

    // Markdown rendering: `text` entries are stored as raw markdown in the DB
    // (the composer Input accepts markdown). `link`/`image` entries keep their
    // previous plain rendering so URLs and file paths are not mis-parsed.
    if entry.kind == "text" {
        if let Some(title) = &entry.title {
            // Title is a single-line heading — render inline markdown (bold,
            // italic, inline code, emojis) at `text_lg`. Body is full markdown.
            row = row
                .child(
                    div()
                        .text_lg()
                        .text_color(theme.foreground)
                        .child(crate::markdown::render_markdown_inline(title, theme)),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(crate::markdown::render_markdown(&entry.content, theme)),
                );
        } else {
            // No separate title — the content itself is the heading/body.
            // Render the whole markdown block so headings, lists, code fences
            // and emojis appear correctly.
            row = row.child(
                div()
                    .text_color(theme.foreground)
                    .child(crate::markdown::render_markdown(&entry.content, theme)),
            );
        }
    } else {
        // Non-text entries (link / image) — preserve the previous plain layout
        // but still support markdown for titles if present.
        let heading = entry.title.as_deref().unwrap_or(&entry.content);
        row = row.child(
            div()
                .text_lg()
                .text_color(theme.foreground)
                .child(heading.to_owned()),
        );
        if entry.title.is_some() {
            row = row.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(entry.content.clone()),
            );
        }
    }

    if is_selected {
        row = row.bg(theme.tokens.list_active);
    }

    // Hover wash + entrance
    let hover_key = format!("entry-hover:{}", entry.id);
    row = row
        .on_hover(hover_listener(hover_key.clone()))
        .bg(hover_blend(
            &hover_key,
            if is_selected {
                *theme.tokens.list_active
            } else {
                gpui::transparent_black()
            },
            *theme.tokens.list_hover,
        ));

    fade_in(entry.id.clone(), row)
}

fn entry_kind(kind: &str) -> &'static str {
    match kind {
        "text" => "TEXT",
        "link" => "LINK",
        "image" => "IMAGE",
        _ => "ENTRY",
    }
}

fn entry_kind_color(kind: &str, theme: &gpui_component::Theme) -> gpui::Hsla {
    match kind {
        "text" => theme.success,
        "link" => theme.link,
        "image" => theme.warning,
        _ => theme.muted_foreground,
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

#[cfg(target_os = "macos")]
fn is_shift_held() -> bool {
    crate::status_item::is_shift_held()
}

#[cfg(not(target_os = "macos"))]
fn is_shift_held() -> bool {
    false
}

// ---------------------------------------------------------------------------
// AI Assistant pane
// ---------------------------------------------------------------------------

fn render_assistant(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let configured = this.service.has_ai_worker();

    let mut messages = v_flex()
        .id("assistant-messages")
        .flex_1()
        .gap_3()
        .overflow_y_scroll()
        .p_4();

    if this.messages.is_empty() {
        messages = messages.child(fade_in(
            "assistant-welcome",
            div().child(welcome_panel(&theme, configured)),
        ));
    } else {
        for (idx, message) in this.messages.iter().enumerate() {
            let id = SharedString::from(format!("msg-{}-{}", idx, message.text.len()));
            let bubble = render_message(&theme, message);
            let animated = if message.streaming {
                fade_quick(id, div().child(bubble))
            } else {
                fade_in(id, div().child(bubble))
            };
            messages = messages.child(animated);
        }
        if this.assistant_busy {
            // WebGPU-style gradient spin loader driven by the shared pulse clock
            let phase = pulse_delta(&GRADIENT_SPIN, cx.entity_id(), cx);
            let opacity = worktable_ui::gspin_opacity(phase, 0.08);
            messages = messages.child(
                div()
                    .h(px(3.))
                    .w_full()
                    .rounded(px(2.))
                    .bg(theme.tokens.background)
                    .child(
                        div()
                            .h_full()
                            .w(relative(opacity.clamp(0.12, 1.0)))
                            .bg(theme.primary)
                            .rounded(px(2.)),
                    ),
            );
        }
    }

    v_flex()
        .flex_1()
        .min_h_0()
        .child(messages)
        .child(
            h_flex()
                .gap_2()
                .px_4()
                .py_3()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    Input::new(&this.assistant_input)
                        .disabled(!configured)
                        .h(px(36.)),
                )
                .child(
                    Button::new("send-assistant")
                        .label(if this.assistant_busy { "…" } else { "Send" })
                        .primary()
                        .icon(app_icon(IconName::ArrowRight))
                        .disabled(!configured || this.assistant_busy)
                        .on_click(cx.listener(|this, _, _, cx| this.send_assistant(cx))),
                ),
        )
        .into_any_element()
}
