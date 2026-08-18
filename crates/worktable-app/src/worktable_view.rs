//! The main Worktable view: sidebar navigation, searchable entries list,
//! inline composer, and the AI assistant pane.

use std::sync::Arc;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InputEvent, InteractiveElement as _,
    ParentElement as _, Render, SharedString, Styled, Subscription, Window, div, px,
};
use gpui_component::button::Button;
use gpui_component::input::{Input, InputState};
use gpui_component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_component::{ActiveTheme, Icon, IconName, h_flex, v_flex};
use worktable_events::WorktableEvent;
use worktable_ai::WorktableEntry;

use crate::assistant::{ChatMessage, Role, render_message, welcome_panel};
use crate::format::relative_time;
use crate::service::WorktableService;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Entries,
    Assistant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComposerKind {
    Note,
    Link,
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
    composer_title: Entity<InputState>,
    composer_body: Entity<InputState>,
    assistant_input: Entity<InputState>,

    // Assistant
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) assistant_busy: bool,

    // UI state
    pub(crate) sidebar_collapsed: bool,
    pub(crate) dark_mode: bool,

    pub(crate) _subscriptions: Vec<Subscription>,
}

impl WorktableView {
    pub fn new(
        service: Arc<WorktableService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let search_input = cx.new(|cx| InputState::new(window, cx)
            .placeholder("Search entries…")
            .clean_on_escape());
        let composer_title = cx.new(|cx| InputState::new(window, cx).placeholder("Title (optional)"));
        let composer_body = cx.new(|cx| InputState::new(window, cx).placeholder("Content…"));
        let assistant_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Ask your notes…")
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
            composer_title,
            composer_body,
            assistant_input,
            messages: Vec::new(),
            assistant_busy: false,
            sidebar_collapsed: false,
            dark_mode: false,
            _subscriptions: Vec::new(),
        };

        view.subscribe(window, cx);
        view.load_entries(cx);
        view
    }

    fn subscribe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Route search input changes into `self.query`.
        let query_input = self.search_input.clone();
        let subscription = cx.subscribe(&self.search_input, move |this, event, cx| {
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
        let composer_title = self.composer_title.clone();
        let composer_body = self.composer_body.clone();
        for field in [composer_title, composer_body] {
            let subscription = cx.subscribe(&field, move |this, event, cx| {
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
        let subscription = cx.subscribe(&assistant_input, move |this, event, cx| {
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

        // Focus the search field when the user opts in via Cmd+F.
        let _ = window;
    }

    fn load_entries(&mut self, cx: &mut Context<Self>) {
        let service = Arc::clone(&self.service);
        let view = cx.entity();
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

    fn visible_entries(&self) -> Vec<&WorktableEntry> {
        let query = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|entry| {
                if query.is_empty() {
                    return true;
                }
                entry.title.as_deref().unwrap_or("").to_lowercase().contains(&query)
                    || entry.content.to_lowercase().contains(&query)
                    || entry.source.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn visible_entry_ids(&self) -> Vec<String> {
        self.visible_entries().into_iter().map(|entry| entry.id.clone()).collect()
    }

    fn selected_entry(&self) -> Option<&WorktableEntry> {
        self.entries.iter().find(|entry| Some(&entry.id) == self.selected.as_ref())
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
        self.search_input.update(cx, |state, cx| state.set_value("", cx));
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
        let view = cx.entity();
        cx.spawn(async move |view, cx| {
            let _ = service.delete_entry(&id).await;
            let _ = view.update(cx, |_, _| {});
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
                if !self.messages.iter().any(|m| m.text.contains("using a tool")) {
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
                self.assistant_busy = false;
                for message in self.messages.iter_mut().rev() {
                    if message.streaming {
                        message.streaming = false;
                        break;
                    }
                }
                if !self.messages.iter().any(|m| m.text.contains(error)) {
                    self.messages.push(ChatMessage::assistant(format!("⚠ {error}")));
                }
                cx.notify();
            }
            _ => {}
        }
    }

    pub fn send_assistant(&mut self, cx: &mut Context<Self>) {
        if !self.service.has_ai_worker() {
            self.messages.push(ChatMessage::assistant(
                "The AI assistant isn't configured yet. See the welcome panel for setup steps.",
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
        self.assistant_input.update(cx, |state, cx| {
            state.set_value("", cx);
        });
        cx.notify();

        let service = Arc::clone(&self.service);
        let request_id = crate::service::new_entry_id();
        let view = cx.entity();
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

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.composer else {
            return;
        };
        let title = trim_opt(&self.composer_title.read(cx).value().to_string());
        let body = trim_opt(&self.composer_body.read(cx).value().to_string());

        let (valid, content, entry_kind) = match kind {
            ComposerKind::Note => {
                if title.is_none() && body.is_none() {
                    (false, String::new(), "text")
                } else {
                    (true, body.unwrap_or_default(), "text")
                }
            }
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
            title,
            source: "Worktable".to_owned(),
            created_at: crate::service::unix_time_ms(),
        };

        self.composer = None;
        cx.notify();

        let service = Arc::clone(&self.service);
        let view = cx.entity();
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
            state.set_value("", cx);
            if kind == ComposerKind::Note {
                state.set_placeholder("Write your note…");
            } else {
                state.set_placeholder("https://…  (or a plain link)");
            }
        });
        self.composer_title.update(cx, |state, cx| {
            state.set_value("", cx);
        });
        cx.notify();

        // Focus the first field on the next frame so the composer is mounted.
        let view = cx.entity();
        window.defer(cx, move |_, window, cx| {
            view.update(cx, |this, cx| {
                let handle = if kind == ComposerKind::Link {
                    this.composer_body.read(cx).focus_handle(cx)
                } else {
                    this.composer_title.read(cx).focus_handle(cx)
                };
                handle.focus(window, cx);
            });
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
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_size(theme.font_size)
            .key_context("worktable-list")
            .on_action(cx.listener(|this, _: &crate::actions::SelectPrevious, _, _| {
                this.move_selection(-1)
            }))
            .on_action(cx.listener(|this, _: &crate::actions::SelectNext, _, _| {
                this.move_selection(1)
            }))
            .on_action(cx.listener(|this, _: &crate::actions::DeleteEntry, _, cx| {
                this.delete_selected(cx)
            }))
            .on_action(cx.listener(|this, _: &crate::actions::OpenEntry, _, cx| {
                this.open_selected(cx)
            }))
            .child(if self.sidebar_collapsed {
                div().into_any_element()
            } else {
                render_sidebar(self, cx).into_any_element()
            })
            .child(render_main(self, window, cx))
    }
}

fn render_sidebar(this: &WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme();

    Sidebar::new("worktable-sidebar")
        .w(px(220.))
        .header(
            SidebarHeader::new().child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size_7()
                            .rounded(theme.radius)
                            .bg(theme.sidebar_primary)
                            .text_color(theme.sidebar_primary_foreground)
                            .child(Icon::new(IconName::GalleryVerticalEnd).size(px(16.))),
                    )
                    .child(
                        v_flex()
                            .child(div().font_semibold().child("Worktable"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Your notes"),
                            ),
                    ),
            ),
        )
        .child(
            SidebarGroup::new("Workspace").child(
                SidebarMenu::new().children([
                    menu_item(
                        "Entries",
                        IconName::Inbox,
                        AppMode::Entries,
                        this.mode,
                    ),
                    menu_item(
                        "AI Assistant",
                        IconName::Bot,
                        AppMode::Assistant,
                        this.mode,
                    ),
                ]),
            ),
        )
        .footer(
            SidebarFooter::new().child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(IconName::CircleUser)
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(if this.service.is_persistent() {
                                "Connected"
                            } else {
                                "Local (no Turso)"
                            }),
                    ),
            ),
        )
}

fn menu_item(label: &'static str, icon: IconName, mode: AppMode, active: AppMode) -> SidebarMenuItem {
    SidebarMenuItem::new(label).icon(icon).active(active == mode)
}

fn render_main(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
    let theme = cx.theme().clone();
    let total = this.entries.len();

    let toolbar = h_flex()
        .items_center()
        .gap_2()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if total == 1 { "1 entry".to_string() } else {
                    format!("{total} entries")
                }),
        )
        .child(
            div()
                .flex_1()
                .child(
                    Input::new(&this.search_input)
                        .prefix(Icon::new(IconName::Search).text_color(theme.muted_foreground))
                        .cleanable(true),
                ),
        )
        .child(new_menu_button(cx));

    let content = match this.mode {
        AppMode::Entries => render_entries(this, window, cx),
        AppMode::Assistant => render_assistant(this, cx),
    };

    v_flex()
        .flex_1()
        .min_w_0()
        .size_full()
        .child(toolbar)
        .child(content)
        .child(composer_bar(this, cx))
}

fn new_menu_button(cx: &mut Context<WorktableView>) -> impl IntoElement {
    let view = cx.entity();
    Button::new("new-entry")
        .label("New")
        .icon(IconName::Plus)
        .primary()
        .dropdown_menu(move |menu, _, _| {
            let entity = view.clone();
            menu.menu_element_with_icon(
                IconName::File,
                Box::new(crate::actions::NewNote),
                move |_, cx| {
                    let _ = entity.update(cx, |_, cx| {});
                    div().child("New Note").child(div().text_xs().text_color(cx.theme().muted_foreground).child("⌘N"))
                },
            )
        })
}

fn composer_bar(this: &mut WorktableView, cx: &mut Context<WorktableView>) -> impl IntoElement {
    let theme = cx.theme().clone();
    let Some(kind) = this.composer else {
        return div().into_any_element();
    };

    let (title, body) = match kind {
        ComposerKind::Note => (
            Some(Input::new(&this.composer_title).placeholder("Title (optional)").h(px(34.))),
            Some(Input::new(&this.composer_body).placeholder("Write your note…").h(px(34.))),
        ),
        ComposerKind::Link => (
            None,
            Some(Input::new(&this.composer_body).placeholder("https://…  (or a plain link)").h(px(34.))),
        ),
    };

    let mut fields = v_flex().flex_1().gap_2();
    if let Some(title) = title {
        fields = fields.child(title);
    }
    if let Some(body) = body {
        fields = fields.child(body);
    }

    h_flex()
        .items_end()
        .gap_2()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.surface)
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
                        .icon(IconName::Check)
                        .on_click(cx.listener(|this, _, _, cx| this.submit_composer(cx))),
                ),
        )
        .into_any_element()
}

fn render_entries(
    this: &mut WorktableView,
    window: &mut Window,
    cx: &mut Context<WorktableView>,
) -> impl IntoElement {
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
        .map(|entry| render_entry_card(entry, &this.selected, cx.listener(move |this, _: &Click, window, cx| {
            this.selected = cloned_selected.clone(); // updated below via per-card
            let _ = window;
            cx.notify();
        })))
        .collect::<Vec<_>>();

    v_flex()
        .p_4()
        .gap_2()
        .overflow_y_scroll()
        .children(items)
}
