//! Streaming text: a typewriter reveal with a caret.
//!
//! Ported from the MIT-licensed [aiCSS](https://www.aicss.dev) `StreamingText`
//! component. The reference reveals 2 characters every 9ms (~222 chars/s) and
//! holds a steady caret while text remains, then blinks it once idle.
//!
//! The GPUI port reveals monotonically toward the current text, so a growing
//! buffer keeps typing instead of restarting on every delta. Frames come from
//! the shared animation clock when a view is attached, and reduced motion
//! shows the full text immediately.

use std::time::Duration;

use crate::activity_now;
use crate::citations::selection;
use gpui::{
    App, ElementId, EntityId, InteractiveElement as _, IntoElement, ParentElement, Refineable as _,
    RenderOnce, SharedString, StyleRefinement, Styled, Window, div,
};

/// The reference reveal rate: 2 characters every 9ms.
const DEFAULT_CHARS_PER_SECOND: f32 = 2.0 / 0.009;
/// Idle caret blink: half a second on, half a second off.
const BLINK_PERIOD_MS: u128 = 1_000;

/// Per-element reveal progress, kept in GPUI element state so nothing is
/// allocated per frame and the text never restarts while it grows.
#[derive(Clone, Copy, Debug, Default)]
struct Reveal {
    /// Shared-clock time the current text started revealing.
    started: Duration,
    /// Bytes revealed so far (always kept on a char boundary).
    shown: usize,
}

/// Typewriter text with a caret.
///
/// ```ignore
/// StreamingText::new("answer", message.text.clone())
///     .streaming(message.streaming)
///     .view(cx.entity_id())
///     .text_color(theme.foreground);
/// ```
#[derive(IntoElement)]
pub struct StreamingText {
    id: ElementId,
    text: SharedString,
    streaming: bool,
    view: Option<EntityId>,
    chars_per_second: f32,
    caret: bool,
    /// Blink the caret once the text has finished revealing.
    blink: bool,
    style: StyleRefinement,
}

impl StreamingText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            streaming: false,
            view: None,
            chars_per_second: DEFAULT_CHARS_PER_SECOND,
            caret: true,
            blink: true,
            style: StyleRefinement::default(),
        }
    }

    /// Whether more text is still arriving. When false the full text is shown.
    pub fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// The view that owns this element; lets it lease the shared clock so the
    /// reveal keeps advancing while mounted.
    pub fn view(mut self, view: EntityId) -> Self {
        self.view = Some(view);
        self
    }

    /// Reveal rate. Defaults to the reference's ~222 characters per second.
    pub fn chars_per_second(mut self, chars_per_second: f32) -> Self {
        self.chars_per_second = chars_per_second.max(1.0);
        self
    }

    /// Show the caret (default true).
    pub fn caret(mut self, caret: bool) -> Self {
        self.caret = caret;
        self
    }

    /// Blink the caret after the reveal completes (default true). While text
    /// is still revealing the caret is steady, matching the reference.
    pub fn blink(mut self, blink: bool) -> Self {
        self.blink = blink;
        self
    }

    /// How many bytes of `text` are visible after `elapsed` seconds at `rate`.
    /// Pure so the reveal contract is unit-testable.
    pub fn revealed_bytes(elapsed: f32, rate: f32, text_len: usize) -> usize {
        let revealed = (elapsed.max(0.0) * rate.max(1.0)) as usize;
        revealed.min(text_len)
    }

    /// Whether the caret is visible: steady while revealing, blinking once the
    /// text has settled.
    fn caret_visible(&self, now: Duration, complete: bool, reduced: bool) -> bool {
        if !self.caret {
            return false;
        }
        if !complete || reduced {
            // Steady while streaming and under reduced motion (no blink).
            return true;
        }
        self.blink && (now.as_millis() % BLINK_PERIOD_MS) < BLINK_PERIOD_MS / 2
    }
}

impl Styled for StreamingText {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for StreamingText {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let reduced = cx.reduce_motion();
        // Keep frames arriving while there is animation to show: the reveal
        // itself, or an idle blinking caret.
        let needs_frames = self.streaming || (self.caret && self.blink);
        let now = if needs_frames {
            match self.view {
                Some(view) => activity_now(view, cx),
                None => Duration::ZERO,
            }
        } else {
            Duration::ZERO
        };

        let text = self.text.clone();
        let streaming = self.streaming && !reduced;
        let rate = self.chars_per_second;
        let id = self.id.clone();
        let shown = window.with_global_id(id, |global_id, window| {
            window.with_element_state(global_id, |state: Option<Reveal>, _window| {
                let mut state = state.unwrap_or_default();
                // A shorter text is a replaced message, not a shrinking one:
                // restart the reveal from the beginning.
                if text.len() < state.shown {
                    state.started = now;
                    state.shown = 0;
                }
                state.shown = if streaming {
                    Self::revealed_bytes(
                        now.saturating_sub(state.started).as_secs_f32(),
                        rate,
                        text.len(),
                    )
                } else {
                    text.len()
                };
                (state.shown, state)
            })
        });

        let mut end = shown.min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        let complete = end >= text.len();
        let mut display = SharedString::from(text[..end].to_owned());
        if self.caret_visible(now, complete, reduced) {
            display = SharedString::from(format!("{display}\u{258d}"));
        }
        let document = selection::Document::default();

        // A plain block: the streaming bubble gives it a definite width, so
        // the text wraps and the bubble grows in height as the answer types.
        let mut root = div()
            .debug_selector(|| "streaming-text".into())
            .min_w_0()
            .max_w_full()
            .child(selection::text(&document, display, "").copy_end(end));
        root.style().refine(&self.style);
        selection::Surface {
            id: self.id,
            child: root.into_any_element(),
            document,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_advances_linearly_and_clamps() {
        assert_eq!(StreamingText::revealed_bytes(0.0, 222.0, 100), 0);
        assert_eq!(StreamingText::revealed_bytes(0.045, 222.0, 100), 9);
        assert_eq!(StreamingText::revealed_bytes(1.0, 222.0, 100), 100);
    }

    #[test]
    fn caret_is_steady_while_revealing_and_blinks_when_idle() {
        let text = StreamingText::new("t", "hello");
        assert!(text.caret_visible(Duration::from_millis(0), false, false));
        // Idle: on for the first half of the second, off for the second half.
        assert!(text.caret_visible(Duration::from_millis(100), true, false));
        assert!(!text.caret_visible(Duration::from_millis(600), true, false));
        // Reduced motion keeps it steady.
        assert!(text.caret_visible(Duration::from_millis(600), true, true));
        // A disabled caret never shows.
        let no_caret = StreamingText::new("t", "hello").caret(false);
        assert!(!no_caret.caret_visible(Duration::from_millis(0), false, false));
    }
}
