//! The AI assistant pane: a chat interface backed by the Pi worker.

use gpui::{InteractiveElement as _, ParentElement as _, Justify, Styled, div, px, relative};
use gpui_component::theme::Theme;
use gpui_component::{ActiveTheme, Icon, IconName, h_flex, v_flex};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub text: String,
    pub streaming: bool,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            streaming: false,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            streaming: false,
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
                    "The AI assistant isn't configured yet.\n\nSet TURSO_DATABASE_URL / TURSO_AUTH_TOKEN / PI_PROVIDER / PI_MODEL and build the worker (cd agent && npm run build) to enable it."
                }),
        )
}

/// Render a single chat message bubble.
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

    h_flex()
        .w_full()
        .justify(if is_user { Justify::End } else { Justify::Start })
        .child(
            div()
                .max_w(relative(0.85))
                .rounded(theme.radius_lg)
                .bg(background)
                .text_color(foreground)
                .px_3()
                .py_2()
                .text_sm()
                .child(message.text.clone()),
        )
}
