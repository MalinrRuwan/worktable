//! Animation kit — the zeron motion catalog as reusable helpers over gpui
//! [`Animation`]/[`AnimationExt`].
//!
//! Catalog (docs/research/feature-inventory.md §1.12):
//! - `fade-in`   0.5s  cubic-bezier(0.16,1,0.3,1), translateY 4→0 (entrances)
//! - `fade-quick` 0.15s
//! - `menu-in`   0.14s scale 0.96 + translateY −2 (popovers)
//! - `dialog-in` 0.18s scale 0.96→1
//! - `splash-out` 0.5s opacity + translateY −6, 0.15s delay
//! - `zeron-pulse` 2.4s staggered cell opacity 0.08→1, scale 0.9→1 (loaders)
//! - `gradient-spin-pulse` 750ms per-cell phase wave (working indicator)
//! - 200ms ease-out width/height transitions (sidebar/panes)
//!
//! Custom easing is a closure over gpui's `Fn(f32) -> f32` easing shape; CSS
//! `cubic-bezier()` is evaluated exactly by [`CubicBezier`].
//!
//! Reduced motion: gpui's `App::reduce_motion` flag is honored *automatically* by
//! every `with_animation` element — oneshot animations snap to their end state,
//! repeating ones to their start state, and no frames are scheduled. The
//! [`set_reduced_motion`]/[`reduced_motion`] wrappers make it a single global
//! switch; pure helpers take the flag explicitly where they run outside elements.
//!
//! translateY is implemented as a relative-position `top` inset: taffy applies
//! relative insets after layout, so — like a CSS transform — siblings never move.
//! gpui has no scale transform for `div`s at the pinned rev (only `svg`
//! transformations), so `menu-in`/`dialog-in` approximate their scale component
//! with fade + translate; see the module report in ARCHITECTURE §4 follow-ups.

pub mod action;
pub mod citations;
pub mod loading;
mod markdown;
pub mod streaming;
pub mod theme;
pub mod thinking;

pub use action::{ActionTone, CircleAction};
pub use citations::{
    CitationColors, CitationFooter, CitationRef, CitationSegment, InlineCitations, parse_citations,
};
pub use loading::{Orb, OrbVariant};
pub use streaming::StreamingText;
pub use thinking::ThinkingState;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationElement, App, Div, ElementId, EntityId, Global, Hsla, IntoElement,
    ParentElement, Pixels, Styled, div, relative, rems,
};

pub use gpui::AnimationExt;

// ---------------------------------------------------------------------------
// Pulse clock — throttled drive for the repeating loaders
// ---------------------------------------------------------------------------

/// Repeat-tick interval for the pulse/spinner/dots loaders — 120fps.
///
/// The clock only runs while at least one loader is mounted (see
/// [`PULSE_LEASE`]): views register on paint and drop off ~300ms after their
/// last loader unmounts, so a window with nothing animating schedules no
/// frames at all. 120fps keeps the dot/bob motion perfectly fluid on
/// ProMotion displays at double the old 30fps cost only while visible.
const PULSE_TICK: Duration = Duration::from_millis(8);

/// How long a view stays on the tick list after its last spinner paint. One
/// lease outlives a few missed frames; an unmounted spinner stops renewing and
/// the view drops off, letting the clock park.
const PULSE_LEASE: Duration = Duration::from_millis(300);

struct PulseClock {
    epoch: Instant,
    leases: HashMap<EntityId, Instant>,
    running: bool,
}

impl Global for PulseClock {}

impl Default for PulseClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            leases: HashMap::new(),
            running: false,
        }
    }
}

/// Current phase `[0,1)` of a repeating spec, plus a lease that keeps the
/// calling view re-rendering at [`PULSE_TICK`] while its spinner stays
/// mounted. All cells across all views share one epoch, so multi-instance
/// loaders stay phase-locked. Reduced motion returns a static 0 and schedules
/// nothing.
pub fn pulse_delta(spec: &MotionSpec, view: EntityId, cx: &mut App) -> f32 {
    if cx.reduce_motion() {
        return 0.0;
    }
    let period = spec.total().as_secs_f32();
    (lease_clock(view, cx).elapsed().as_secs_f32() / period).fract()
}

/// Time elapsed on the shared animation clock, in one call: leases the clock
/// for `view` (so frames keep arriving at [`PULSE_TICK`] while it stays
/// mounted) and returns how long the shared animation epoch has been running.
///
/// Components that need monotonic animation time (typewriter reveals) store a
/// start value from this clock; because every view shares one epoch, two
/// instances of the same component stay in step.
pub fn activity_now(view: EntityId, cx: &mut App) -> Duration {
    lease_clock(view, cx).elapsed()
}

/// Lease the shared clock for `view` and return the shared epoch. The clock
/// parks itself when no leases remain, so an idle window schedules no frames.
fn lease_clock(view: EntityId, cx: &mut App) -> Instant {
    let clock = cx.default_global::<PulseClock>();
    clock.leases.insert(view, Instant::now() + PULSE_LEASE);
    let epoch = clock.epoch;
    if !clock.running {
        clock.running = true;
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(PULSE_TICK).await;
                let parked = cx.update(|cx| {
                    let clock = cx.default_global::<PulseClock>();
                    let now = Instant::now();
                    clock.leases.retain(|_, until| *until > now);
                    if clock.leases.is_empty() {
                        clock.running = false;
                        return true;
                    }
                    let views: Vec<EntityId> = clock.leases.keys().copied().collect();
                    for view in views {
                        cx.notify(view);
                    }
                    false
                });
                if parked {
                    break;
                }
            }
        })
        .detach();
    }
    epoch
}

// ---------------------------------------------------------------------------
// Cubic bezier
// ---------------------------------------------------------------------------

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function (endpoints fixed at
/// (0,0) and (1,1)). Evaluation solves x(t) = input by Newton iteration with a
/// bisection fallback — the standard UnitBezier approach.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3.0 * a;
        let bb = 3.0 * (b - a) - c;
        let aa = 1.0 - c - bb;
        (aa, bb, c)
    }

    fn sample_x(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        ((a * t + b) * t + c) * t
    }

    fn sample_y(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.y1, self.y2);
        ((a * t + b) * t + c) * t
    }

    fn sample_x_derivative(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        (3.0 * a * t + 2.0 * b) * t + c
    }

    /// Curve parameter `t` for a given progress `x` (both 0..1).
    fn solve_t_for_x(&self, x: f32) -> f32 {
        // Newton–Raphson.
        let mut t = x;
        for _ in 0..8 {
            let err = self.sample_x(t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = self.sample_x_derivative(t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        // Bisection fallback (x(t) is monotonic for valid CSS beziers).
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.0;
            if self.sample_x(mid) < x {
                lo = mid
            } else {
                hi = mid
            }
        }
        (lo + hi) / 2.0
    }

    /// Eased output for input progress `x ∈ [0,1]` (clamped).
    pub fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        // f32 rounding can push sample_y a hair past 1.0 (observed 1.000000119
        // near the end of menu animations); gpui's animation element asserts
        // `delta ∈ [0,1]` and aborts, so clamp the output hard.
        self.sample_y(self.solve_t_for_x(x)).clamp(0.0, 1.0)
    }

    /// This curve as a gpui easing closure.
    pub fn easing(self) -> impl Fn(f32) -> f32 + 'static {
        move |x| self.eval(x)
    }
}

/// transitions.dev's standard ease — CSS `cubic-bezier(0.22, 1, 0.36, 1)`.
/// Every transition in the app rides this curve.
pub const EASE_TRANSITIONS: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// CSS `ease-in-out` — shapes the bobbing-dots bounce legs only.
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);

// ---------------------------------------------------------------------------
// Motion specs (the catalog)
// ---------------------------------------------------------------------------

/// One catalog entry: duration + optional delay + curve. The delay is folded into
/// the gpui animation timeline (gpui `Animation` has no native delay): the
/// animation runs for `delay + duration` and [`progress`](Self::progress) holds 0
/// until the delay has elapsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSpec {
    pub duration_ms: u64,
    pub delay_ms: u64,
    pub curve: CubicBezier,
}

impl MotionSpec {
    pub const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self {
            duration_ms,
            delay_ms: 0,
            curve,
        }
    }

    pub const fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// Wall-clock span of the whole timeline (delay + duration).
    pub fn total(&self) -> Duration {
        Duration::from_millis(self.delay_ms + self.duration_ms)
    }

    /// Eased progress (0..1) for a raw timeline delta (0..1 across [`total`](Self::total)).
    /// Pure — unit-testable without a window.
    pub fn progress(&self, raw_delta: f32) -> f32 {
        let total = (self.delay_ms + self.duration_ms) as f32;
        if total <= 0.0 || self.duration_ms == 0 {
            return 1.0;
        }
        let t =
            (raw_delta.clamp(0.0, 1.0) * total - self.delay_ms as f32) / self.duration_ms as f32;
        self.curve.eval(t.clamp(0.0, 1.0))
    }

    /// A oneshot gpui [`Animation`] for this spec (delay folded in).
    /// Wall-clock span honors [`speed_scale`] (measurement knob).
    pub fn animation(&self) -> Animation {
        let spec = *self;
        Animation::new(spec.total().mul_f32(speed_scale())).with_easing(move |d| spec.progress(d))
    }

    /// A repeating gpui [`Animation`] with linear easing over the raw period —
    /// for the pulse/wave loaders whose per-cell easing happens in the animator.
    pub fn repeating(&self) -> Animation {
        Animation::new(self.total()).repeat()
    }
}

/// Entrances: 0.5s fade + 4px rise.
pub const FADE_IN: MotionSpec = MotionSpec::new(500, EASE_TRANSITIONS);
/// Quick fade: 0.15s.
pub const FADE_QUICK: MotionSpec = MotionSpec::new(150, EASE_TRANSITIONS);
/// Popover-in: 0.14s (scale 0.96 approximated, translateY −2).
pub const MENU_IN: MotionSpec = MotionSpec::new(140, EASE_TRANSITIONS);
/// Popover-out: 0.1s — quicker than the entrance (exits should get out of the
/// way; matches the Radix convention of a shorter close than open).
pub const MENU_OUT: MotionSpec = MotionSpec::new(100, EASE_TRANSITIONS);
/// Modal open, transitions.dev: scale 0.96→1 + fade in 250ms. GPUI divs have
/// no scale transform, so callers approximate the scale with a small rise.
pub const MODAL_OPEN: MotionSpec = MotionSpec::new(250, EASE_TRANSITIONS);
/// Modal close, transitions.dev: scale back to 0.96 + fade out in 150ms.
pub const MODAL_CLOSE: MotionSpec = MotionSpec::new(150, EASE_TRANSITIONS);
/// Entry-detail morph open: a card's rect grows into the middle of the UI.
pub const MORPH_OPEN: MotionSpec = MotionSpec::new(320, EASE_TRANSITIONS);
/// Entry-detail morph close: the panel shrinks back toward its source rect.
pub const MORPH_CLOSE: MotionSpec = MotionSpec::new(200, EASE_TRANSITIONS);
/// Boot splash exit: 0.5s fade + 6px lift after a 0.15s hold.
pub const SPLASH_OUT: MotionSpec = MotionSpec::new(500, EASE_TRANSITIONS).with_delay(150);
/// Sidebar / pane width+height transitions: 200ms.
pub const RESIZE: MotionSpec = MotionSpec::new(200, EASE_TRANSITIONS);
/// Terminal tab drag-reorder sliding transforms: 150ms (§1.10).
pub const TAB_SLIDE: MotionSpec = MotionSpec::new(150, EASE_TRANSITIONS);
/// Diff-pane per-file collapse: 180ms height (§1.11).
pub const COLLAPSE: MotionSpec = MotionSpec::new(180, EASE_TRANSITIONS);
/// Diff-pane chevron rotate: 200ms (§1.11; approximated as a crossfade — gpui
/// divs have no rotation transform at the pinned rev, same caveat as scale).
pub const CHEVRON: MotionSpec = MotionSpec::new(200, EASE_TRANSITIONS);
/// Rail-tick / scroll-to-row glide: 500ms over the whole distance.
pub const SCROLL_GLIDE: MotionSpec = MotionSpec::new(500, EASE_TRANSITIONS);
/// Interactive hover wash: 150ms.
pub const HOVER_FADE: MotionSpec = MotionSpec::new(150, EASE_TRANSITIONS);
/// Zeron loader pulse period: 2.4s.
pub const ZERON_PULSE: MotionSpec = MotionSpec::new(2400, EASE_TRANSITIONS);
/// Gradient matrix spinner wave period: 750ms.
pub const GRADIENT_SPIN: MotionSpec = MotionSpec::new(750, EASE_TRANSITIONS);
/// Text-dots ("Thinking…") cycle: 1.4s opacity wave, one dot every 0.2s.
pub const TEXT_DOTS: MotionSpec = MotionSpec::new(1400, EASE_TRANSITIONS);
/// Entries ⇄ Agent page transition: transitions.dev's page slide — 250ms
/// over `cubic-bezier(0.22,1,0.36,1)` with an 8px offset and a 3px blur
/// (GPUI has no element blur; the fade carries the softening).
pub const PAGE_SLIDE: MotionSpec = MotionSpec::new(250, EASE_TRANSITIONS);
/// aiCSS ThinkingState shimmer: 2.25s with holds at each end of the sweep.
pub const THINKING_SHINE: MotionSpec = MotionSpec::new(2250, EASE_TRANSITIONS);
/// Bobbing-dots (waiting-on-LLM) cycle: 1s bounce, one dot every 0.2s.
pub const BOBBING_DOTS: MotionSpec = MotionSpec::new(1000, EASE_IN_OUT);
/// Stagger between text/bobbing dots (fraction of period) — 0.2s at 1s period.
pub const DOTS_STAGGER: f32 = 0.2;

// ---------------------------------------------------------------------------
// Element helpers (paint-layer entrances/exits)
// ---------------------------------------------------------------------------

/// Standard entrance: opacity 0→1 + translateY 4→0 over [`FADE_IN`].
pub fn fade_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_IN.animation(), |el, t| {
        el.relative().opacity(t).top(rems(0.25 * (1.0 - t)))
    })
}

/// Quick opacity-only fade over [`FADE_QUICK`].
pub fn fade_quick<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_QUICK.animation(), |el, t| el.opacity(t))
}

/// Popover entrance: fade + translateY −2→0 over [`MENU_IN`].
/// (zeron also scales 0.96→1; divs have no scale transform in gpui — approximated.)
pub fn menu_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, MENU_IN.animation(), |el, t| {
        el.relative()
            .opacity(0.3 + 0.7 * t)
            .top(rems(-0.125 * (1.0 - t)))
    })
}

/// Popover exit: the reverse of [`menu_in`] — fade to 0 + translateY 0→−2 over
/// [`MENU_OUT`]. Unlike the entrances, the eased progress `t` comes from the
/// caller (computed off [`crate::popover::Popup`]'s closing instant at render
/// time): `with_animation`'s element-id-keyed clock replays from 0 on remount
/// (the hover-blend comment's warning), and a replay mid-exit is a full-opacity
/// flash. The wall-clock progress is monotonic by construction; the animation
/// wrapper here only pumps frames for the exit's span, its own delta unused.
pub fn menu_out<E>(id: impl Into<ElementId>, t: f32, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, MENU_OUT.animation(), move |el, _| {
        el.relative().opacity(1.0 - t).top(rems(-0.125 * t))
    })
}

/// Boot-splash exit: hold 150ms, then fade out + lift 6px over 500ms.
pub fn splash_out<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, SPLASH_OUT.animation(), |el, t| {
        el.opacity(1.0 - t).top(rems(-0.375 * t))
    })
}

// ---------------------------------------------------------------------------
// Loader math (pure; rendered by crate::loaders)
// ---------------------------------------------------------------------------

/// Zeron-pulse floor opacity.
pub const PULSE_MIN_OPACITY: f32 = 0.08;
/// Zeron-pulse floor scale.
pub const PULSE_MIN_SCALE: f32 = 0.9;
/// Stagger between loader cells (fraction of period).
pub const PULSE_STAGGER: f32 = 0.12;

/// Triangle-wave pulse (0→1→0) for t in [0,1].
pub fn pulse_wave(t: f32) -> f32 {
    let t = t.fract();
    if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 }
}

/// Opacity for a loader cell at raw phase t.
pub fn pulse_opacity(t: f32) -> f32 {
    PULSE_MIN_OPACITY + (1.0 - PULSE_MIN_OPACITY) * pulse_wave(t)
}

/// Scale for a loader cell at raw phase t.
pub fn pulse_scale(t: f32) -> f32 {
    PULSE_MIN_SCALE + (1.0 - PULSE_MIN_SCALE) * pulse_wave(t)
}

/// Staggered phase for cell `index` out of a wave with `stagger` spacing.
pub fn staggered_phase(delta: f32, index: usize, stagger: f32) -> f32 {
    let phase = delta - index as f32 * stagger;
    phase.rem_euclid(1.0)
}

/// Gradient-matrix spinner wave: intensity (0..1) of cell `wave_index` out of
/// `wave_count` diagonals, at raw delta `raw_delta` of the 750ms period. The wave
/// front travels across diagonals once per period.
pub fn matrix_wave(raw_delta: f32, wave_index: usize, wave_count: usize) -> f32 {
    let count = wave_count.max(1) as f32;
    pulse_wave(staggered_phase(raw_delta, wave_index, 1.0 / count))
}

/// Gradient spin: opacity for a cell in the matrix spinner.
pub fn gspin_opacity(delta: f32, dim: f32) -> f32 {
    let t = delta.fract();
    if t < 0.1 {
        1.0
    } else if t < 0.45 {
        let p = (t - 0.1) / 0.35;
        1.0 + (dim - 1.0) * p
    } else if t < 0.9 {
        dim
    } else {
        let p = (t - 0.9) / 0.1;
        dim + (1.0 - dim) * p
    }
}

// ---------------------------------------------------------------------------
// Text dots + bobbing dots (LLM wait states)
// ---------------------------------------------------------------------------

/// Opacity of dot `index` (of `count`) at raw cycle position `delta` for the
/// text-dots wave: each dot fades 0→1→0 (triangle) with a per-dot delay of
/// `DOTS_STAGGER` — the CSS keyframe pair (0%/100% opacity 0, 50% opacity 1).
pub fn text_dot_opacity(delta: f32, index: usize) -> f32 {
    pulse_wave(staggered_phase(delta, index, DOTS_STAGGER))
}

/// Vertical bob factor (0..1) of dot `index` at raw cycle position `delta` for
/// the bobbing-dots bounce. The triangle wave's two legs are each shaped with
/// `EASE_IN_OUT` so the dot leaves and lands softly, like `motion`'s
/// `animate: { y: [0, "0.625em", 0] }, ease: "easeInOut"`.
pub fn bobbing_dot_lift(delta: f32, index: usize) -> f32 {
    let phase = staggered_phase(delta, index, DOTS_STAGGER);
    // 0..0.5 rises 0→1, 0.5..1 falls 1→0; each leg eased.
    let leg = if phase < 0.5 {
        EASE_IN_OUT.eval(phase * 2.0)
    } else {
        EASE_IN_OUT.eval((1.0 - phase) * 2.0)
    };
    leg.clamp(0.0, 1.0)
}

/// "Thinking" + animated trailing dots (the `TextDots` web component):
/// a text label with three dots whose opacity waves in a 0.2s stagger.
/// `delta` is the shared clock phase for [`TEXT_DOTS`].
pub fn text_dots(label: &str, delta: f32, color: Hsla) -> Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .text_color(color)
        .child(div().child(label.to_owned()))
        .child(div().flex().flex_row().children((0..3).map(|index| {
            let opacity = text_dot_opacity(delta, index);
            div()
                .opacity(opacity)
                // Keep the label's baseline: collapsed line box.
                .line_height(relative(1.0))
                .child(".")
        })))
}

/// Three round dots bobbing in sequence (the `BobbingDots` web component) —
/// the "waiting for the LLM's first token" state. `delta` is the shared clock
/// phase for [`BOBBING_DOTS`].
pub fn bobbing_dots(delta: f32, color: Hsla, dot: Pixels) -> Div {
    div()
        .flex()
        .flex_row()
        .items_end()
        // gap is 12% of the row, mirroring `gap-[12%]`.
        .gap(dot * 0.5)
        .pb(dot * 0.2)
        .children((0..3).map(|index| {
            let lift = bobbing_dot_lift(delta, index);
            div()
                .size(dot)
                .rounded_full()
                .bg(color)
                // translateY via a relative `top` inset (see module docs):
                // 0.625em at dot scale ≈ one dot height.
                .relative()
                .top(-dot * lift)
        }))
}

/// Linear interpolation (layout tweens).
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

// ---------------------------------------------------------------------------
// Reduced motion
// ---------------------------------------------------------------------------

/// Dev/measurement knob (`ZERON_MOTION_SCALE`, default 1): stretches every
/// catalog timeline by this factor — e.g. `ZERON_MOTION_SCALE=10` slows the
/// 200ms pane tweens to 2s so screenshot bursts can sample the geometry
/// per frame. Read once; never set in production.
pub fn speed_scale() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("ZERON_MOTION_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|s| s.is_finite())
            .map(|s| s.clamp(0.01, 100.0))
            .unwrap_or(1.0)
    })
}

/// Global reduced-motion flag. gpui snaps every `with_animation` element when
/// set (end state for oneshots, rest state for loops) and schedules no frames.
pub fn set_reduced_motion(cx: &mut App, reduced: bool) {
    cx.set_reduce_motion(reduced);
}

/// Read the global reduced-motion flag.
pub fn reduced_motion(cx: &App) -> bool {
    cx.reduce_motion()
}

#[cfg(test)]
mod tests {
    #[test]
    fn eval_never_escapes_unit_interval_dense_sweep() {
        // Regression: f32 rounding once produced 1.000000119 near a curve's
        // tail, tripping gpui's `delta ∈ [0,1]` assert (SIGABRT). Sweep
        // densely, including the values right below 1.0 where Newton lands
        // closest to the endpoint.
        for curve in [EASE_TRANSITIONS, EASE_IN_OUT] {
            for i in 0..=100_000u32 {
                let x = i as f32 / 100_000.0;
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
            for x in [0.999_999f32, 0.999_999_9, 1.0 - f32::EPSILON] {
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
        }
    }

    use super::*;

    fn assert_close(actual: f32, expected: f32, tol: f32, ctx: &str) {
        assert!(
            (actual - expected).abs() <= tol,
            "{ctx}: got {actual}, expected {expected} ±{tol}"
        );
    }

    #[test]
    fn bezier_linear_is_identity() {
        let linear = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for x in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            assert_close(linear.eval(x), x, 1e-4, "linear");
        }
    }

    #[test]
    fn bezier_known_values() {
        // References computed independently with the UnitBezier solver.
        let cases: [(&str, CubicBezier, [f32; 5]); 2] = [
            (
                "transitions",
                EASE_TRANSITIONS,
                [0.401097, 0.764865, 0.961383, 0.996894, 0.999840],
            ),
            (
                "ease-in-out",
                EASE_IN_OUT,
                [0.019722, 0.129162, 0.500000, 0.870838, 0.980278],
            ),
        ];
        for (name, curve, expected) in cases {
            for (x, want) in [0.1, 0.25, 0.5, 0.75, 0.9].into_iter().zip(expected) {
                assert_close(curve.eval(x), want, 1e-3, name);
            }
        }
    }

    #[test]
    fn bezier_endpoints_and_clamping() {
        for curve in [EASE_TRANSITIONS, EASE_IN_OUT] {
            assert_eq!(curve.eval(0.0), 0.0);
            assert_eq!(curve.eval(1.0), 1.0);
            assert_eq!(curve.eval(-0.5), 0.0);
            assert_eq!(curve.eval(1.5), 1.0);
        }
    }

    #[test]
    fn bezier_is_monotonic_for_catalog_curves() {
        for curve in [EASE_TRANSITIONS, EASE_IN_OUT] {
            let mut last = 0.0;
            for i in 0..=100 {
                let y = curve.eval(i as f32 / 100.0);
                assert!(y >= last - 1e-4, "monotonicity violated at {i}");
                last = y;
            }
        }
    }

    #[test]
    fn spec_delay_holds_then_runs() {
        // SPLASH_OUT: 150ms delay + 500ms run = 650ms total.
        assert_eq!(SPLASH_OUT.total(), Duration::from_millis(650));
        assert_eq!(SPLASH_OUT.progress(0.0), 0.0);
        // Still inside the delay window at raw 0.2 (130ms < 150ms).
        assert_eq!(SPLASH_OUT.progress(0.2), 0.0);
        // Fully done at the end; clamped beyond.
        assert_eq!(SPLASH_OUT.progress(1.0), 1.0);
        assert_eq!(SPLASH_OUT.progress(2.0), 1.0);
        // Midway through the run: raw 0.65 → 272.5ms into the 500ms run.
        let mid = SPLASH_OUT.progress(0.65);
        assert!(mid > 0.0 && mid < 1.0);
        // No-delay specs pass straight through the curve.
        assert_close(
            FADE_IN.progress(0.5),
            EASE_TRANSITIONS.eval(0.5),
            1e-6,
            "no-delay",
        );
    }

    #[test]
    fn catalog_timings_match_zeron() {
        assert_eq!(FADE_IN.duration_ms, 500);
        assert_eq!(FADE_QUICK.duration_ms, 150);
        assert_eq!(MENU_IN.duration_ms, 140);
        assert_eq!(MODAL_OPEN.duration_ms, 250);
        assert_eq!(MODAL_CLOSE.duration_ms, 150);
        assert_eq!((SPLASH_OUT.duration_ms, SPLASH_OUT.delay_ms), (500, 150));
        assert_eq!(RESIZE.duration_ms, 200);
        assert_eq!(TAB_SLIDE.duration_ms, 150);
        assert_eq!(COLLAPSE.duration_ms, 180);
        assert_eq!(CHEVRON.duration_ms, 200);
        assert_eq!(ZERON_PULSE.duration_ms, 2400);
        assert_eq!(GRADIENT_SPIN.duration_ms, 750);
        assert_eq!(EASE_TRANSITIONS, CubicBezier::new(0.22, 1.0, 0.36, 1.0));
    }

    #[test]
    fn pulse_wave_endpoints() {
        assert_close(pulse_wave(0.0), 0.0, 1e-6, "wave start");
        assert_close(pulse_wave(0.5), 1.0, 1e-6, "wave peak");
        assert_close(pulse_wave(1.0), 0.0, 1e-6, "wave end");
        assert_close(pulse_opacity(0.0), 0.08, 1e-6, "opacity floor");
        assert_close(pulse_opacity(0.5), 1.0, 1e-6, "opacity peak");
        assert_close(pulse_scale(0.0), 0.9, 1e-6, "scale floor");
        assert_close(pulse_scale(0.5), 1.0, 1e-6, "scale peak");
    }

    #[test]
    fn stagger_wraps_and_orders_cells() {
        // Cell 0 at delta 0 is at phase 0; later cells lag by the stagger.
        assert_close(staggered_phase(0.0, 0, PULSE_STAGGER), 0.0, 1e-6, "cell 0");
        assert_close(
            staggered_phase(0.0, 1, PULSE_STAGGER),
            1.0 - PULSE_STAGGER,
            1e-5,
            "cell 1 wraps",
        );
        // A full period later the phase is identical.
        assert_close(
            staggered_phase(0.3, 2, PULSE_STAGGER),
            staggered_phase(0.3 + 1.0, 2, PULSE_STAGGER),
            2e-6,
            "periodic",
        );
        // Matrix wave peaks travel: diagonal k peaks when the front reaches it.
        let peak0 = matrix_wave(0.5, 0, 5);
        assert_close(peak0, 1.0, 1e-5, "diag 0 peak at half period");
    }

    #[test]
    fn lerp_basics() {
        assert_eq!(lerp(208.0, 400.0, 0.0), 208.0);
        assert_eq!(lerp(208.0, 400.0, 1.0), 400.0);
        assert_eq!(lerp(0.0, 10.0, 0.5), 5.0);
    }

    #[test]
    fn gspin_pulse_shape() {
        // Full at the cycle start, dim through the rest band, rising at the tail.
        assert_close(gspin_opacity(0.0, 0.1), 1.0, 1e-6, "cycle start");
        assert_close(gspin_opacity(0.45, 0.1), 0.1, 1e-6, "fully dim");
        assert_close(gspin_opacity(0.9, 0.1), 0.1, 1e-6, "rest band");
        assert_close(gspin_opacity(1.0, 0.1), 1.0, 1e-6, "wraps to full");
        let mid_fall = gspin_opacity(0.2, 0.1);
        assert!(mid_fall > 0.1 && mid_fall < 1.0, "eases down");
        let mid_rise = gspin_opacity(0.96, 0.1);
        assert!(mid_rise > 0.1 && mid_rise < 1.0, "eases up");
    }

    #[test]
    fn text_dots_wave_staggers_and_bounds() {
        // Dot 0 starts dark, peaks a half-cycle in, returns to dark.
        assert_close(text_dot_opacity(0.0, 0), 0.0, 1e-6, "dot 0 start");
        // Peak at half the dot's own cycle (delta 0.5 → wave 1.0).
        assert_close(text_dot_opacity(0.5, 0), 1.0, 1e-6, "dot 0 peak");
        assert_close(text_dot_opacity(1.0, 0), 0.0, 1e-6, "dot 0 end");
        // Dot 1 lags dot 0 by the stagger.
        assert_close(
            text_dot_opacity(DOTS_STAGGER, 1),
            text_dot_opacity(0.0, 0),
            1e-6,
            "dot 1 lags",
        );
        // Opacity always lands in [0,1].
        for i in 0..3 {
            for step in 0..=100 {
                let delta = step as f32 / 100.0;
                let opacity = text_dot_opacity(delta, i);
                assert!(
                    (0.0..=1.0).contains(&opacity),
                    "dot {i} at {delta}: {opacity}"
                );
            }
        }
    }

    #[test]
    fn bobbing_dots_lift_eases_and_bounds() {
        assert_close(bobbing_dot_lift(0.0, 0), 0.0, 1e-6, "rest");
        assert_close(bobbing_dot_lift(0.5, 0), 1.0, 1e-6, "apex");
        assert_close(bobbing_dot_lift(1.0, 0), 0.0, 1e-6, "lands");
        // Eased legs: a quarter into the rise is gentler than linear (the
        // curve equals linear exactly at its midpoint, so probe off-center).
        let early = bobbing_dot_lift(0.125, 0);
        let linear_early = 0.25;
        assert!(
            early < linear_early,
            "ease-in-out starts slower than linear: {early}"
        );
        // Stagger keeps later dots behind.
        assert_close(
            bobbing_dot_lift(DOTS_STAGGER, 1),
            bobbing_dot_lift(0.0, 0),
            1e-6,
            "dot 1 lags",
        );
        for i in 0..3 {
            for step in 0..=100 {
                let delta = step as f32 / 100.0;
                let lift = bobbing_dot_lift(delta, i);
                assert!((0.0..=1.0).contains(&lift), "dot {i} at {delta}: {lift}");
            }
        }
    }
}
