//! The AI assistant pane: a chat interface backed by the embedded Pi agent.

use gpui::{ParentElement as _, Styled, div, prelude::FluentBuilder, px, relative};
use gpui_component::theme::Theme;
use gpui_component::{Icon, IconName, h_flex, v_flex};

#[derive(Clone, Copy, PartialEq, Eq)]
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
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            thinking: String::new(),
            streaming: false,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            thinking: String::new(),
            streaming: false,
        }
    }

    /// Convenience for a streaming assistant message that is still thinking.
    pub fn assistant_thinking(thinking: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
            thinking: thinking.into(),
            streaming: true,
        }
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
                .size(px(44.))
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
/// above the final answer, and both `thinking` and `text` are rendered as
/// markdown (via `crate::markdown`) so code, bold, lists and unicode emojis
/// appear correctly. User bubbles also render markdown so the user can preview
/// formatting, but they inherit the primary bubble's foreground.
pub fn render_message<'a>(
    theme: &'a Theme,
    message: &'a ChatMessage,
) -> impl gpui::IntoElement + 'a {
    let is_user = message.role == Role::User;
    let (background, foreground) = if is_user {
        (theme.primary, theme.primary_foreground)
    } else {
        (theme.secondary, theme.secondary_foreground)
    };

    // Build the bubble's inner vertical stack.
    let mut bubble = v_flex().gap_2();

    // Thinking block — show when the assistant emitted reasoning. The block
    // is visually de-emphasized (muted, italic) and collapsible in spirit:
    // when streaming finishes it remains visible but muted so the user can
    // still inspect it. Unicode emojis inside thinking are preserved via
    // markdown rendering.
    if !message.thinking.is_empty() {
        let thinking_label = if message.streaming && message.text.is_empty() {
            "Thinking…"
        } else {
            "Thought"
        };
        bubble = bubble.child(
            v_flex()
                .gap_1()
                .p_2()
                .rounded(theme.radius)
                .bg(theme.muted.opacity(0.4))
                .border_1()
                .border_color(theme.border.opacity(0.5))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .child(thinking_label.to_owned()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .italic()
                        .child(crate::markdown::render_markdown(&message.thinking, theme)),
                ),
        );
    }

    if !message.text.is_empty() {
        // Render the message body as markdown. The bubble already sets
        // `text_color(foreground)` and `text_sm()`, so normal markdown spans
        // inherit the bubble's color. Code and links override with their own.
        let body = if is_user {
            // User text is typically short; render inline markdown but fall
            // back to block renderer so code blocks still work if the user
            // pastes markdown.
            crate::markdown::render_markdown(&message.text, theme)
        } else {
            crate::markdown::render_markdown(&message.text, theme)
        };
        bubble = bubble.child(div().text_sm().child(body));
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

    // Streaming cursor — a subtle pulse dot when the model is still typing.
    if message.streaming {
        bubble = bubble.child(
            div()
                .h(px(2.))
                .w(px(12.))
                .rounded_full()
                .bg(foreground.opacity(0.5)),
        );
    }

    h_flex()
        .w_full()
        .when(is_user, |this| this.justify_end())
        .when(!is_user, |this| this.justify_start())
        .child(
            div()
                .max_w(relative(0.85))
                .rounded(theme.radius_lg)
                .bg(background)
                .text_color(foreground)
                .px_3()
                .py_2()
                .child(bubble),
        )
}
