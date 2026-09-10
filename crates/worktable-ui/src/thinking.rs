//! Thinking state: a minimal shimmering label for the reasoning phase.
//!
//! Ported from the MIT-licensed [aiCSS](https://www.aicss.dev)
//! [`ThinkingState`](https://www.aicss.dev/components/thinking-state)
//! component: a plain "Thinking" label with a dim band sweeping across it
//! (CSS `background-position: 100% → 0%` over 2.25s with holds at each end).
//!
//! GPUI cannot clip a gradient to glyphs at the pinned revision, so the port
//! reproduces the effect by coloring each character from the band's current
//! position: characters inside the band take the dim color, the rest the base
//! color. Layout, wrapping, and baseline behavior match ordinary text.
//!
//! Reduced motion renders the static base color and schedules no frames; the
//! shimmer rides the shared pulse clock through
//! [`pulse_delta`](crate::pulse_delta) and only leases it while mounted.

use gpui::{
    App, ElementId, EntityId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Refineable as _, RenderOnce, SharedString, StyleRefinement, Styled, Window, div,
};

use crate::{EASE_TRANSITIONS, THINKING_SHINE, pulse_delta};

/// Half-width of the dim band as a fraction of the label.
const SHINE_BAND: f32 = 0.22;
/// Band center at the holds: fully past the label on either side, so the loop
/// restarts with no visible jump.
const SHINE_FAR: f32 = 1.0 + SHINE_BAND;
const SHINE_NEAR: f32 = -SHINE_BAND;

/// Where the dim band's center sits for a raw clock phase.
///
/// Matches the CSS keyframes — hold beyond the trailing edge for the first
/// 18%, sweep across over the eased middle, hold beyond the leading edge for
/// the last 18%. Because both holds are off the label, the loop wraps cleanly
/// (the CSS gradient's visible window is uniform at both `background-position`
/// extremes, which this reproduces).
pub fn shine_position(raw: f32) -> f32 {
    let t = raw.fract();
    if t < 0.18 {
        SHINE_FAR
    } else if t < 0.82 {
        SHINE_FAR + (SHINE_NEAR - SHINE_FAR) * EASE_TRANSITIONS.eval((t - 0.18) / 0.64)
    } else {
        SHINE_NEAR
    }
}

/// Band influence at label position `x ∈ [0,1]`: 0 outside the band, 1 at
/// its center. At both hold positions every glyph is at 0, so the shimmer
/// never pops when it loops.
pub fn shine_t(raw: f32, x: f32) -> f32 {
    let distance = (x - shine_position(raw)).abs();
    (1.0 - distance / SHINE_BAND).clamp(0.0, 1.0)
}

/// The dim color for band influence `t` — the base color at reduced alpha,
/// like the CSS `rgba(161, 161, 161, 0.45)` dip.
pub fn shine_color(base: Hsla, t: f32) -> Hsla {
    Hsla {
        a: base.a * (1.0 - 0.55 * t),
        ..base
    }
}

/// A shimmering "Thinking" label. See the module docs for the port notes.
///
/// ```ignore
/// ThinkingState::new("thinking", "Thinking")
///     .view(cx.entity_id())
///     .color(theme.muted_foreground)
///     .text_xs()
/// ```
#[derive(IntoElement)]
pub struct ThinkingState {
    id: ElementId,
    label: SharedString,
    color: Hsla,
    view: Option<EntityId>,
    style: StyleRefinement,
}

impl ThinkingState {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            color: Hsla::default(),
            view: None,
            style: StyleRefinement::default(),
        }
    }

    /// Lease the shared animation clock for this view.
    pub fn view(mut self, view: EntityId) -> Self {
        self.view = Some(view);
        self
    }

    /// Base label color; the band dims it.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }
}

impl Styled for ThinkingState {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for ThinkingState {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let phase = self
            .view
            .map(|view| pulse_delta(&THINKING_SHINE, view, cx))
            .unwrap_or(0.0);
        let chars: Vec<char> = self.label.chars().collect();
        let count = chars.len().max(1);
        let base = self.color;

        let mut row =
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .children(chars.into_iter().enumerate().map(|(index, ch)| {
                    let x = if count == 1 {
                        0.5
                    } else {
                        index as f32 / (count - 1) as f32
                    };
                    div()
                        .text_color(shine_color(base, shine_t(phase, x)))
                        .child(ch.to_string())
                }));
        row.style().refine(&self.style);
        row.id(self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_holds_then_sweeps() {
        let far = 1.0 + SHINE_BAND;
        let near = -SHINE_BAND;
        assert_eq!(shine_position(0.0), far, "starts beyond the trailing edge");
        assert_eq!(shine_position(0.18), far, "holds through 18%");
        assert_eq!(shine_position(0.82), near, "lands beyond the leading edge");
        assert_eq!(shine_position(0.95), near, "holds before the wrap");
        assert_eq!(shine_position(1.0), far, "wraps back to the held end");
        let mid = shine_position(0.5);
        assert!(mid > near && mid < far, "sweeps across: {mid}");
        // The curve is fast out of the gate: the first quarter of the sweep
        // travels farther than linear.
        let early = shine_position(0.18 + 0.16);
        assert!(early < 0.75, "ease curve starts fast: {early}");
    }

    #[test]
    fn band_is_centered_and_bounded() {
        assert_eq!(shine_t(0.0, 0.5), 0.0, "hold dims nothing on the label");
        let mid_position = shine_position(0.5);
        assert_eq!(shine_t(0.5, mid_position), 1.0, "mid-sweep center");
        for step in 0..=100 {
            let t = shine_t(step as f32 / 100.0, 0.3);
            assert!((0.0..=1.0).contains(&t));
        }
    }

    /// The loop boundary must be invisible: both holds leave every glyph at
    /// full base color, so restarting the CSS keyframes cannot pop.
    #[test]
    fn loop_boundary_is_seamless() {
        for x in [0.0, 0.15, 0.5, 0.85, 1.0] {
            assert_eq!(shine_t(0.0, x), 0.0, "start hold at x={x}");
            assert_eq!(shine_t(1.0, x), 0.0, "wrap hold at x={x}");
            assert_eq!(shine_t(0.99, x), 0.0, "end hold at x={x}");
        }
    }

    #[test]
    fn color_dim_floor_is_45_percent() {
        let base = Hsla {
            h: 0.2,
            s: 0.1,
            l: 0.5,
            a: 1.0,
        };
        assert_eq!(shine_color(base, 0.0).a, 1.0, "base outside the band");
        let dim = shine_color(base, 1.0);
        assert!((dim.a - 0.45).abs() < 1e-5, "band floor alpha: {}", dim.a);
        assert_eq!(dim.h, base.h, "only alpha changes");
    }
}
