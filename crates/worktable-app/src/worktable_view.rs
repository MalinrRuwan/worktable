//! The main Worktable view: sidebar navigation, searchable entries list,
//! inline composer, the AI assistant pane, and the AI provider settings panel.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Size,
    StatefulInteractiveElement as _, Styled, Subscription, Window, div, px, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::{Scrollbar, ScrollbarAxis};
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

use crate::assistant::{ChatMessage, Role, render_message, welcome_panel};
use crate::format::relative_time;
use crate::service::WorktableService;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Entries,
    Assistant,
    Settings,
    ProviderConfig,
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

    // Entries
    pub(crate) entries: Vec<WorktableEntry>,
    pub(crate) selected: Option<String>,
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

    // UI state
    pub(crate) sidebar_collapsed: bool,
    pub(crate) dark_mode: bool,
    settings_scroll: VirtualListScrollHandle,
    provider_row_sizes: Rc<Vec<Size<Pixels>>>,

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

        let mut view = Self {
            service,
            focus_handle,
            mode: AppMode::Entries,
            entries: Vec::new(),
            selected: None,
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
            sidebar_collapsed: false,
            dark_mode: false,
            settings_scroll: VirtualListScrollHandle::new(),
            provider_row_sizes: Rc::new(Vec::new()),

            _subscriptions: Vec::new(),
        };

        view.subscribe(window, cx);
        view.load_entries(cx);
        view.refresh_providers(cx);
        view
    }

    fn subscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Route search input changes into `self.query`.
        let query_input = self.search_input.clone();
        let subscription = cx.subscribe(&self.search_input, move |this, _emitter, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = query_input.read(cx).value().to_string();
                if this
                    .selected
                    .as_ref()
                    .map(|id| !this.visible_entry_ids().contains(id))
                    .unwrap_or(false)
                {
                    this.selected = None;
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
        self.entries
            .iter()
            .find(|entry| Some(&entry.id) == self.selected.as_ref())
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
        let current = self
            .selected
            .as_ref()
            .and_then(|id| visible.iter().position(|entry| &entry.id == id));
        let next = match current {
            Some(index) => {
                ((index as isize + direction).rem_euclid(visible.len() as isize)) as usize
            }
            None => 0,
        };
        self.selected = Some(visible[next].id.clone());
    }

    pub fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        self.entries.retain(|entry| entry.id != id);
        self.selected = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        cx.spawn(async move |_view, _cx| {
            let _ = service.delete_entry(&id).await;
        })
        .detach();
    }

    pub fn copy_selected(&mut self, cx: &mut Context<Self>) {
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

    pub fn select_at(&mut self, id: String) {
        self.selected = Some(id);
    }

    pub fn on_event(&mut self, event: &WorktableEvent, cx: &mut Context<Self>) {
        match event {
            WorktableEvent::AiRunStarted { .. } => {
                self.assistant_busy = true;
                cx.notify();
            }
            WorktableEvent::AiMessageDelta { delta, .. } => {
                match self.messages.last_mut().filter(|m| m.streaming) {
                    Some(message) => message.text.push_str(delta),
                    None => self.messages.push(ChatMessage {
                        role: Role::Assistant,
                        text: delta.clone(),
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
                    this.entries.insert(0, entry);
                    this.selected = Some(this.entries[0].id.clone());
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
                    this.entries.insert(0, entry);
                    this.selected = this.entries.first().map(|entry| entry.id.clone());
                    this.mode = AppMode::Entries;
                }
                cx.notify();
            });
        })
        .detach();
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
        let theme = cx.theme().clone();

        div()
            .id("worktable-root")
            .size_full()
            .flex()
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
            .child(render_sidebar(self, cx))
            .child(render_main(self, window, cx))
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
    let root_settings_breadcrumbs = if this.mode == AppMode::Settings {
        Some(
            h_flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child("Home")
                .child(
                    Icon::new(IconName::ChevronRight)
                        .size(px(14.))
                        .text_color(theme.muted_foreground),
                )
                .child("Settings"),
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
                    div().child("New Note").child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("⌘N"),
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
        return v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
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

    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
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
                .child("Choose a provider, model, and authentication method for the assistant."),
        )
        .child(div().text_sm().text_color(theme.primary).child(active))
        .child(configure)
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

fn composer_bar(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let Some(_kind) = this.composer else {
        return div().into_any_element();
    };

    let fields = v_flex()
        .flex_1()
        .gap_2()
        .child(Input::new(&this.composer_body).h(px(34.)));

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

    let cloned_selected = this.selected.clone();
    let items = visible
        .into_iter()
        .map(|entry| {
            let entry_selected = cloned_selected.clone();
            render_entry_card(
                entry,
                &this.selected,
                &theme,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.selected = entry_selected.clone();
                    let _ = window;
                    cx.notify();
                }),
            )
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
    selected: &Option<String>,
    theme: &gpui_component::Theme,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let heading = entry.title.as_deref().unwrap_or(&entry.content);
    let kind = entry_kind(&entry.kind);
    let is_selected = selected.as_deref() == Some(&entry.id);

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
        )
        .child(
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

    if is_selected {
        row = row.bg(theme.tokens.list_active);
    }

    row
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
        messages = messages.child(welcome_panel(&theme, configured));
    } else {
        for message in &this.messages {
            messages = messages.child(render_message(&theme, message));
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
