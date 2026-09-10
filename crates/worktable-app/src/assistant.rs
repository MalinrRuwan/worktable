//! The AI assistant pane: a chat interface backed by the embedded Pi agent.

use std::rc::Rc;

use gpui::{
    AnyElement, ClickEvent, EntityId, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder, relative,
};
use gpui_component::text::TextView;
use gpui_component::theme::Theme;
use gpui_component::{Icon, IconName, h_flex, v_flex};
use worktable_ui::citations::CitationOpenHandler;
use worktable_ui::{CitationColors, CitationRef, InlineCitations, StreamingText, ThinkingState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    /// Reasoning / "thinking" streamed before the final answer (AiThoughtDelta).
    /// Rendered as a muted, collapsible block above `text`. Empty if the model
    /// did not emit thinking.
    pub thinking: String,
    pub streaming: bool,
    /// Sources for `[n]` markers in `text`, rendered as inline citation
    /// chips and a source footer.
    pub citations: Vec<CitationRef>,
    /// Whether the thinking block is collapsed to its header. The run clears
    /// this when the answer completes so long reasoning does not dominate the
    /// transcript; the header stays clickable.
    pub thinking_collapsed: bool,
}

/// How long reasoning may get before the block switches to a fixed-height
/// scroll area. Short thoughts render fully; long ones never blow up layout.
const THINKING_SCROLL_THRESHOLD: usize = 1_200;

/// Callback for the thinking header toggle (the caller owns the index).
pub type ThinkingToggle = Rc<dyn Fn(&ClickEvent, &mut Window, &mut gpui::App)>;

/// Inputs `render_message` needs beyond the message itself.
#[derive(Clone, Default)]
pub struct MessageOptions {
    /// Render reasoning blocks at all (Settings → UI "Show thinking").
    pub show_thinking: bool,
    /// Citation activation, with the click position for in-app morphs.
    pub citation_open: Option<CitationOpenHandler>,
    /// Toggle a message's thinking block (the caller owns the index).
    pub on_toggle_thinking: Option<ThinkingToggle>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            thinking: String::new(),
            streaming: false,
            citations: Vec::new(),
            thinking_collapsed: false,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            thinking: String::new(),
            streaming: false,
            citations: Vec::new(),
            thinking_collapsed: false,
        }
    }

    /// True while the model is emitting reasoning and no answer text yet —
    /// the state that renders the animated "Thinking" dots.
    pub fn is_thinking_only(&self) -> bool {
        self.streaming && self.text.is_empty() && !self.thinking.is_empty()
    }
}

/// An empty-state / welcome panel for the assistant.
pub fn welcome_panel(theme: &Theme, configured: bool) -> impl gpui::IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .text_center()
        .child(
            Icon::new(IconName::Bot)
                .size_11()
                .text_color(theme.muted_foreground),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .max_w(relative(0.7))
                .child(if configured {
                    "Ask about your notes — summarize, search, or brainstorm. Responses stream in as they're generated."
                } else {
                    "The AI assistant isn't configured yet.\n\nOpen Settings (⌘,) to pick a provider, add an API key, or sign in with OAuth."
                }),
        )
}

/// Render a single chat message bubble.
///
/// Assistant messages render `thinking` (if present) as a muted italic block
/// above the final answer. A live answer types out through
/// [`StreamingText`]; once complete it renders as markdown, with
/// [`InlineCitations`] used when the message carries sources for `[n]`
/// markers. User bubbles also render markdown but inherit the primary bubble's
/// foreground.
pub fn render_message<'a>(
    theme: &'a Theme,
    message: &'a ChatMessage,
    index: usize,
    view: EntityId,
    dots_delta: f32,
    options: MessageOptions,
) -> impl gpui::IntoElement + 'a {
    let MessageOptions {
        show_thinking,
        citation_open,
        on_toggle_thinking,
    } = options;
    let is_user = message.role == Role::User;
    // User bubbles are a light primary wash so the dark "sent" treatment does
    // not dominate the transcript; assistant bubbles keep the muted secondary.
    let (background, foreground) = if is_user {
        (theme.primary.opacity(0.16), theme.foreground)
    } else {
        (theme.secondary, theme.secondary_foreground)
    };

    // Build the bubble's inner vertical stack.
    let mut bubble = v_flex().gap_2().w_full().min_w_0().max_w_full();

    // Thinking block — shown only when the user enabled reasoning in
    // Settings → UI. While the model is still reasoning the label shimmers
    // through the aiCSS ThinkingState port; once the answer starts it reads
    // "Thought" and stays inspectable.
    let thinking_visible = show_thinking && !message.thinking.is_empty();
    let _ = dots_delta;
    if thinking_visible {
        let expanded = !message.thinking_collapsed;
        let thinking_label: gpui::AnyElement = if message.is_thinking_only() {
            ThinkingState::new(
                SharedString::from(format!("msg-{index}-thinking-state")),
                "Thinking",
            )
            .view(view)
            .color(theme.muted_foreground)
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .into_any_element()
        } else {
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child("Thought")
                .into_any_element()
        };

        // The header toggles the block; the body is hidden while collapsed so
        // long reasoning never dominates layout (it froze the UI before).
        let mut header = h_flex()
            .id(gpui::ElementId::Name(
                format!("thinking-header-{index}").into(),
            ))
            .debug_selector({
                let selector = format!("thinking-header-{index}");
                move || selector.clone()
            })
            .role(gpui::Role::Button)
            .aria_label(if expanded {
                "Collapse thoughts"
            } else {
                "Expand thoughts"
            })
            .cursor_pointer()
            .items_center()
            .gap_1()
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3()
                .text_color(theme.muted_foreground),
            )
            .child(thinking_label);
        if let Some(toggle) = on_toggle_thinking {
            header = header.on_click(move |event, window, cx| toggle(event, window, cx));
        }

        let body: gpui::AnyElement = if expanded {
            let long = message.thinking.chars().count() > THINKING_SCROLL_THRESHOLD;
            let text = TextView::markdown(
                SharedString::from(format!("msg-{index}-thinking")),
                message.thinking.clone(),
            )
            .selectable(true);
            if long {
                // Fixed height + the component's own scroll/virtualization.
                div()
                    .w_full()
                    .min_w_0()
                    .h(crate::design::THINKING_MAX_HEIGHT)
                    .debug_selector({
                        let selector = format!("thinking-body-{index}");
                        move || selector.clone()
                    })
                    .child(text.scrollable(true))
                    .into_any_element()
            } else {
                div()
                    .w_full()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .italic()
                    .debug_selector({
                        let selector = format!("thinking-body-{index}");
                        move || selector.clone()
                    })
                    .child(text)
                    .into_any_element()
            }
        } else {
            div().into_any_element()
        };

        let mut block = v_flex()
            .gap_1()
            .w_full()
            .min_w_0()
            .max_w_full()
            .overflow_hidden()
            .p_2()
            .rounded(theme.radius)
            .bg(theme.muted.opacity(0.4))
            .border_1()
            .border_color(theme.border.opacity(0.5))
            .child(header);
        if expanded {
            block = block.child(body);
        }
        bubble = bubble.child(block);
    }

    if !message.text.is_empty() {
        // Render the message body. The bubble already sets
        // `text_color(foreground)` and `text_sm()`, so markdown spans inherit
        // the bubble's color; code and links override with their own.
        let body: AnyElement = if is_user {
            // Plain wrapped text inside a shrink-to-fit bubble. The bubble
            // takes the text's width up to its max, so a "yes" is narrow and a
            // paragraph fills the measure.
            div()
                .w_full()
                .min_w_0()
                .whitespace_normal()
                .text_sm()
                .child(message.text.clone())
                .into_any_element()
        } else if message.streaming {
            // Live tokens type out; the finished answer swaps to markdown.
            StreamingText::new(
                SharedString::from(format!("msg-{index}-stream")),
                message.text.clone(),
            )
            .streaming(true)
            .view(view)
            .text_sm()
            .into_any_element()
        } else if !message.citations.is_empty() {
            // The aiCSS-style inline citations: numbered marker chips in the
            // prose with a hover preview and a source footer. Chips click
            // through to the same handler as the footer rows.
            let colors = CitationColors {
                // `popover` and `border` keep the chips readable on the
                // assistant bubble (`secondary`); Ayu Light gives
                // `muted` and `secondary` the same value.
                foreground,
                muted: theme.muted_foreground,
                chip_background: theme.popover,
                chip_hover_background: theme.border,
                background: theme.background,
                border: theme.border,
            };
            let citations = InlineCitations::new(
                SharedString::from(format!("msg-{index}-citations")),
                message.text.clone(),
                message.citations.clone(),
            )
            .colors(colors)
            .radius(theme.radius_tokens().sm);
            let citations = if let Some(handler) = citation_open {
                citations.on_open(handler)
            } else {
                citations
            };
            div()
                .w_full()
                .min_w_0()
                .debug_selector(|| "assistant-citations".into())
                .child(citations)
                .into_any_element()
        } else {
            TextView::markdown(
                SharedString::from(format!("msg-{index}")),
                message.text.clone(),
            )
            .selectable(true)
            .into_any_element()
        };
        bubble = bubble.child(div().w_full().min_w_0().text_sm().child(body));
    } else if message.streaming {
        // Empty while streaming — show a subtle placeholder so the bubble has
        // height.
        bubble = bubble.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground.opacity(0.7))
                .child("…"),
        );
    }

    // Streaming cursor for an empty streaming bubble; once text is present the
    // typewriter caret carries the state.
    if message.streaming && message.text.is_empty() {
        bubble = bubble.child(
            div()
                .h_0p5()
                .w_3()
                .rounded_full()
                .bg(foreground.opacity(0.5)),
        );
    }

    h_flex()
        .w_full()
        .min_w_0()
        .when(is_user, |this| this.justify_end())
        .when(!is_user, |this| this.justify_start())
        .child(
            div()
                // Assistant bodies are selectable rich text that needs a
                // bounded frame, so they take a definite measure. User bubbles
                // shrink to their text, capped at the same 85% of the row.
                .when(is_user, |el| el.max_w(relative(0.85)))
                .when(!is_user, |el| el.w(relative(0.85)))
                .debug_selector({
                    let selector = format!("bubble-{index}");
                    move || selector.clone()
                })
                .min_w_0()
                .overflow_hidden()
                .rounded(theme.radius_lg)
                .bg(background)
                .text_color(foreground)
                .px_3()
                .py_2()
                .child(bubble),
        )
}
