//! Circular action buttons: one component for every round, icon-only control.
//!
//! The note bar's plus and tick, the agent's send/stop, the entry detail's
//! close, and the image viewer's controls are all the same shape — a circular
//! button in one of the theme's three weights. Defining them here keeps the
//! corner radius, sizing, and tone consistent instead of re-deriving the
//! modifier chain at every call site.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, RenderOnce, SharedString, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Disableable, Icon};

/// Visual weight of a circular action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActionTone {
    /// Quiet toolbar action (add image, close).
    #[default]
    Ghost,
    /// Raised neutral control (image download, page toggle).
    Secondary,
    /// The single commit action of its area (send, add note).
    Primary,
}

/// Click handler stored by [`CircleAction`].
type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A circular, icon-only action button.
///
/// ```ignore
/// CircleAction::new("add-note")
///     .primary()
///     .icon(app_icon(IconName::Check))
///     .tooltip("Add note")
///     .on_click(...)
/// ```
#[derive(IntoElement)]
pub struct CircleAction {
    id: ElementId,
    tone: ActionTone,
    icon: Option<Icon>,
    label: Option<SharedString>,
    child: Option<AnyElement>,
    tooltip: Option<SharedString>,
    disabled: bool,
    large: bool,
    on_click: Option<ClickHandler>,
    debug_selector: Option<SharedString>,
}

impl CircleAction {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            tone: ActionTone::Ghost,
            icon: None,
            label: None,
            child: None,
            tooltip: None,
            disabled: false,
            large: false,
            on_click: None,
            debug_selector: None,
        }
    }

    /// The commit action of its area.
    pub fn primary(mut self) -> Self {
        self.tone = ActionTone::Primary;
        self
    }

    /// A raised neutral control.
    pub fn secondary(mut self) -> Self {
        self.tone = ActionTone::Secondary;
        self
    }

    /// A quiet toolbar action (the default).
    pub fn ghost(mut self) -> Self {
        self.tone = ActionTone::Ghost;
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// A text label instead of an icon (still circular).
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Arbitrary inner content, e.g. an animated orb.
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.child = Some(child.into_any_element());
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// The 40px header size; the default is the 36px control size.
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    pub fn debug_selector(mut self, selector: impl Into<SharedString>) -> Self {
        self.debug_selector = Some(selector.into());
        self
    }
}

impl RenderOnce for CircleAction {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut button = Button::new(self.id).rounded_full().flex_shrink_0();
        button = match self.tone {
            ActionTone::Primary => button.primary(),
            ActionTone::Secondary => button.secondary(),
            ActionTone::Ghost => button.ghost(),
        };
        if self.large {
            button = button.size_10();
        }
        if let Some(icon) = self.icon {
            button = button.icon(icon);
        }
        if let Some(label) = self.label {
            button = button.label(label);
        }
        if let Some(child) = self.child {
            button = button.child(child);
        }
        if let Some(tooltip) = self.tooltip {
            button = button.tooltip(tooltip);
        }
        if self.disabled {
            button = button.disabled(true);
        }
        if let Some(handler) = self.on_click {
            button = button.on_click(move |event, window, cx| handler(event, window, cx));
        }
        if let Some(selector) = self.debug_selector {
            button = button.debug_selector(move || selector.to_string());
        }
        button
    }
}
