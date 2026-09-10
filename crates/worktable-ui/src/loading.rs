//! Loading indicators: compact animated activity orbs.
//!
//! Ported from the MIT-licensed [aiCSS](https://www.aicss.dev) `Orb`
//! component. Two variants are implemented:
//!
//! - [`OrbVariant::S1`] — a 3×3 dot lattice pulsing outward from the centre
//!   ("Thinking").
//! - [`OrbVariant::G2`] — a globe of five latitude rings spinning in
//!   alternating directions ("Sequencing").
//!
//! Geometry is authored on a 28px stage and scaled to the requested size, so
//! the hand-tuned pitch and opacities hold at any size. Both variants are
//! driven by the shared animation clock in [`crate`], so every orb on screen
//! stays phase-locked. Colors and sizes come from the caller, keeping the
//! component theme-agnostic; reduced motion renders the resting frame and
//! schedules no frames.

use std::sync::OnceLock;

use gpui::{
    App, Div, ElementId, EntityId, Hsla, InteractiveElement as _, IntoElement, ParentElement,
    Pixels, Refineable as _, Rems, RenderOnce, SharedString, StyleRefinement, Styled, Window, div,
    px, rems,
};

use crate::{CubicBezier, EASE_TRANSITIONS, MotionSpec, pulse_delta};

/// The stage the geometry is tuned on; every constant below is in stage px.
const STAGE: f32 = 28.0;

/// Lattice wave period and globe spin period, straight from aiCSS.
const S1_WAVE: MotionSpec = MotionSpec::new(1_700, EASE_TRANSITIONS);
const G2_SPIN: MotionSpec = MotionSpec::new(3_600, EASE_TRANSITIONS);

/// The reference's per-segment wave easing (`--orb-ease-in-out`).
const ORB_EASE_IN_OUT: CubicBezier = CubicBezier::new(0.66, 0.0, 0.34, 1.0);

/// Which activity indicator to render.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OrbVariant {
    /// 3×3 lattice pulsing outward from the centre — "Thinking".
    #[default]
    S1,
    /// Counter-rotating dot globe — "Sequencing".
    G2,
}

impl OrbVariant {
    /// The short task label aiCSS pairs with this variant.
    pub fn task(self) -> &'static str {
        match self {
            Self::S1 => "Thinking",
            Self::G2 => "Sequencing",
        }
    }

    fn spec(self) -> MotionSpec {
        match self {
            Self::S1 => S1_WAVE,
            Self::G2 => G2_SPIN,
        }
    }
}

/// An animated activity indicator.
///
/// ```ignore
/// Orb::new("thinking", OrbVariant::S1)
///     .view(cx.entity_id()) // keeps frames arriving while mounted
///     .size(rems(1.25))
///     .color(theme.muted_foreground);
/// ```
#[derive(IntoElement)]
pub struct Orb {
    id: ElementId,
    variant: OrbVariant,
    size: Rems,
    color: Hsla,
    /// Resting dot opacity: aiCSS uses 0.14 in light mode, 0.2 in dark.
    rest_ink: f32,
    /// View that leases the shared animation clock for frames.
    view: Option<EntityId>,
    /// Deterministic phase override (tests and static renders).
    phase: Option<f32>,
    label: Option<SharedString>,
    surface: Option<Hsla>,
    label_color: Option<Hsla>,
    border: Option<Hsla>,
    style: StyleRefinement,
}

impl Orb {
    pub fn new(id: impl Into<ElementId>, variant: OrbVariant) -> Self {
        Self {
            id: id.into(),
            variant,
            size: rems(1.5),
            color: Hsla::default(),
            rest_ink: 0.14,
            view: None,
            phase: None,
            label: None,
            surface: None,
            label_color: None,
            border: None,
            style: StyleRefinement::default(),
        }
    }

    /// The default "thinking" lattice.
    pub fn s1(id: impl Into<ElementId>) -> Self {
        Self::new(id, OrbVariant::S1)
    }

    /// The counter-rotating globe.
    pub fn g2(id: impl Into<ElementId>) -> Self {
        Self::new(id, OrbVariant::G2)
    }

    /// Rendered edge length; the 28px geometry scales to fit (default 1.5rem).
    pub fn size(mut self, size: impl Into<Rems>) -> Self {
        self.size = size.into();
        self
    }

    /// Dot color.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }

    /// Resting opacity for unlit dots (light themes 0.14, dark 0.2).
    pub fn rest_ink(mut self, rest_ink: f32) -> Self {
        self.rest_ink = rest_ink.clamp(0.0, 1.0);
        self
    }

    /// The view that owns this orb; letting it lease the shared clock so it
    /// keeps animating while mounted.
    pub fn view(mut self, view: EntityId) -> Self {
        self.view = Some(view);
        self
    }

    /// Render at a fixed phase `[0,1)` instead of the shared clock (tests).
    pub fn phase(mut self, phase: f32) -> Self {
        self.phase = Some(phase);
        self
    }

    /// Render the orb inside a status pill with this label.
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Pill background (the pill form needs a surface and label color).
    pub fn surface(mut self, surface: Hsla) -> Self {
        self.surface = Some(surface);
        self
    }

    /// Pill label color.
    pub fn label_color(mut self, color: Hsla) -> Self {
        self.label_color = Some(color);
        self
    }

    /// Pill hairline. The reference pairs a ring with shadows; Worktable uses
    /// a border + surface, its flat-surface convention.
    pub fn border(mut self, border: Hsla) -> Self {
        self.border = Some(border);
        self
    }

    fn current_phase(&self, cx: &mut App) -> f32 {
        if cx.reduce_motion() {
            // Static resting frame: the lattice centre stays lit, the globe
            // sits at its first pose.
            return 0.0;
        }
        if let Some(phase) = self.phase {
            return phase;
        }
        match self.view {
            Some(view) => pulse_delta(&self.variant.spec(), view, cx),
            None => 0.0,
        }
    }
}

impl Styled for Orb {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Orb {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let phase = self.current_phase(cx);
        let size = self.size.to_pixels(window.rem_size());
        let k = size.as_f32() / STAGE;
        let reduced = cx.reduce_motion();
        let glyph = match self.variant {
            OrbVariant::S1 => render_lattice(size, k, self.color, self.rest_ink, phase, reduced),
            OrbVariant::G2 => render_globe(size, k, self.color, phase, reduced),
        };

        let debug_name = element_id_label(&self.id);
        let mut root = match self.label {
            None => div()
                .debug_selector(move || debug_name)
                .flex_shrink_0()
                .child(glyph),
            Some(label) => div()
                .debug_selector(move || debug_name)
                .flex()
                .flex_row()
                .items_center()
                .gap(rems(0.4375))
                .h(rems(1.875))
                .pl(rems(0.3125))
                .pr(rems(0.6875))
                .rounded_full()
                .bg(self.surface.unwrap_or_else(|| self.color.opacity(0.08)))
                .border_1()
                .border_color(self.border.unwrap_or_else(|| self.color.opacity(0.15)))
                .child(glyph)
                .child(
                    div()
                        .text_color(self.label_color.unwrap_or(self.color))
                        .child(label),
                ),
        };
        root.style().refine(&self.style);
        root
    }
}

/// A readable label for an element id, used as the orb's debug selector so
/// tests can address it without a wrapper.
fn element_id_label(id: &ElementId) -> String {
    match id {
        ElementId::Name(name) => name.to_string(),
        other => format!("{other:?}"),
    }
}

/// One lattice cell's animated state, in stage units.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LatticeCell {
    x: f32,
    y: f32,
    opacity: f32,
    scale: f32,
}

/// The 3×3 lattice at `phase`, including the per-cell delay wavefront. The
/// centre cell leads by 180ms so the next swell never sits behind the outer
/// fade (the aiCSS comment explains the same beat).
fn s1_cells(phase: f32, rest_ink: f32) -> [LatticeCell; 9] {
    let mut cells = [LatticeCell {
        x: 0.0,
        y: 0.0,
        opacity: rest_ink,
        scale: 1.0,
    }; 9];
    let mut index = 0;
    for y in 0..3 {
        for x in 0..3 {
            let dx = x as f32 - 1.0;
            let dy = y as f32 - 1.0;
            let delay = dx.hypot(dy) * 700.0 - if x == 1 && y == 1 { 180.0 } else { 0.0 };
            let offset = delay / S1_WAVE.total().as_millis() as f32;
            let t = (phase - offset).rem_euclid(1.0);
            let (opacity, scale) = s1_wave(t, rest_ink);
            cells[index] = LatticeCell {
                x: x as f32 * 6.0,
                y: y as f32 * 6.0,
                opacity,
                scale,
            };
            index += 1;
        }
    }
    cells
}

/// The `orb-wave` keyframes: swell on the symmetric curve, then a long rest.
fn s1_wave(t: f32, rest_ink: f32) -> (f32, f32) {
    if t < 0.28 {
        let p = ORB_EASE_IN_OUT.eval(t / 0.28);
        (rest_ink + (1.0 - rest_ink) * p, 1.0 + 0.18 * p)
    } else if t < 0.56 {
        let p = ORB_EASE_IN_OUT.eval((t - 0.28) / 0.28);
        (1.0 - (1.0 - rest_ink) * p, 1.18 - 0.18 * p)
    } else {
        (rest_ink, 1.0)
    }
}

fn render_lattice(
    size: Pixels,
    k: f32,
    color: Hsla,
    rest_ink: f32,
    phase: f32,
    reduced: bool,
) -> Div {
    let mut lattice = div().relative().size(size).overflow_hidden();
    let cell_px = 3.0 * k;
    for (index, cell) in s1_cells(phase, rest_ink).iter().enumerate() {
        // Reduced motion: the centre cell stays lit and every other cell rests.
        let (opacity, scale) = if reduced {
            if index == 4 {
                (1.0, 1.0)
            } else {
                (rest_ink, 1.0)
            }
        } else {
            (cell.opacity, cell.scale)
        };
        // GPUI divs have no scale transform, so the 1.18× swell is drawn as a
        // larger dot kept centred on its cell origin.
        let dot = cell_px * scale;
        let offset = (dot - cell_px) / 2.0;
        lattice = lattice.child(
            div()
                .absolute()
                .left(px((6.5 + cell.x) * k - offset))
                .top(px((6.5 + cell.y) * k - offset))
                .size(px(dot))
                .rounded_full()
                .bg(color)
                .opacity(opacity),
        );
    }
    lattice
}

/// One globe dot's eight precomputed poses, in stage units. `x`/`y` are
/// screen-space offsets from the stage centre (`y` already negated) and
/// `opacity` is the depth cue.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GlobePose {
    x: f32,
    y: f32,
    opacity: f32,
}

const GLOBE_R: f32 = 10.0;
const GLOBE_TILT: f32 = 14.0 * std::f32::consts::PI / 180.0;
const GLOBE_STEPS: usize = 8;
const GLOBE_RINGS: [(f32, usize); 5] = [(52.0, 8), (26.0, 8), (0.0, 8), (-26.0, 8), (-52.0, 8)];

fn project_globe(x: f32, y: f32, z: f32, spin: f32) -> (f32, f32, f32) {
    let (sin, cos) = spin.sin_cos();
    let x1 = x * cos - z * sin;
    let z1 = x * sin + z * cos;
    let (tilt_sin, tilt_cos) = GLOBE_TILT.sin_cos();
    (
        x1,
        y * tilt_cos - z1 * tilt_sin,
        y * tilt_sin + z1 * tilt_cos,
    )
}

/// Depth cue from aiCSS: dots behind the globe fade toward 0.12, front dots
/// approach 1.0.
fn globe_opacity(z: f32) -> f32 {
    let t = ((z / GLOBE_R + 0.15) / 1.15).clamp(0.0, 1.0);
    0.12 + 0.88 * t * t
}

/// All 40 dot poses for G2, computed once per process. Ring parity flips the
/// spin direction, which is what makes the globe read as a helix rather than
/// one rigid rotation.
fn globe_poses() -> &'static [[GlobePose; GLOBE_STEPS]] {
    static POSES: OnceLock<Vec<[GlobePose; GLOBE_STEPS]>> = OnceLock::new();
    POSES.get_or_init(|| {
        let mut dots = Vec::with_capacity(40);
        for (ring_index, (lat, count)) in GLOBE_RINGS.iter().enumerate() {
            let lat_rad = lat * std::f32::consts::PI / 180.0;
            let y0 = lat_rad.sin() * GLOBE_R;
            let ring_r = lat_rad.cos() * GLOBE_R;
            let direction = if ring_index % 2 == 1 { -1.0 } else { 1.0 };
            for dot in 0..*count {
                let lon = (dot as f32 / *count as f32) * std::f32::consts::TAU;
                let mut poses = [GlobePose::default(); GLOBE_STEPS];
                for (step, pose) in poses.iter_mut().enumerate() {
                    let spin =
                        direction * (step as f32 / GLOBE_STEPS as f32) * std::f32::consts::TAU;
                    let (x, y, z) = project_globe(lon.cos() * ring_r, y0, lon.sin() * ring_r, spin);
                    *pose = GlobePose {
                        x,
                        y: -y,
                        opacity: globe_opacity(z),
                    };
                }
                dots.push(poses);
            }
        }
        dots
    })
}

/// Interpolate the poses at `phase` (linear between the eight keyframes,
/// wrapping from the last back to the first, matching the infinite CSS
/// animation).
fn g2_poses(phase: f32) -> impl Iterator<Item = GlobePose> {
    let scaled = phase.rem_euclid(1.0) * GLOBE_STEPS as f32;
    let index = (scaled.floor() as usize) % GLOBE_STEPS;
    let next = (index + 1) % GLOBE_STEPS;
    let t = scaled.fract();
    globe_poses().iter().map(move |poses| {
        let a = poses[index];
        let b = poses[next];
        GlobePose {
            x: a.x + (b.x - a.x) * t,
            y: a.y + (b.y - a.y) * t,
            opacity: a.opacity + (b.opacity - a.opacity) * t,
        }
    })
}

fn render_globe(size: Pixels, k: f32, color: Hsla, phase: f32, reduced: bool) -> Div {
    let mut globe = div().relative().size(size).overflow_hidden();
    // The globe reads as a wireframe: dots need real weight and the sphere
    // should fill its stage (the S1 lattice uses the same 3.4px pitch, so both
    // variants read at the same size).
    let dot = 3.4 * k;
    let center = size.as_f32() / 2.0;
    let phase = if reduced { 0.0 } else { phase };
    for pose in g2_poses(phase) {
        globe = globe.child(
            div()
                .absolute()
                .left(px(center + pose.x * k - dot / 2.0))
                .top(px(center + pose.y * k - dot / 2.0))
                .size(px(dot))
                .rounded_full()
                .bg(color)
                .opacity(pose.opacity),
        );
    }
    globe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s1_cells_radiate_from_the_centre() {
        // At phase 0.28 the centre is mid-swell while a corner is resting:
        // the wavefront radiates outward.
        let cells = s1_cells(0.28, 0.14);
        let centre = cells[4];
        let corner = cells[0];
        assert_eq!((centre.x, centre.y), (6.0, 6.0));
        assert!(
            centre.opacity > corner.opacity,
            "centre {} should lead corner {}",
            centre.opacity,
            corner.opacity
        );
        assert_eq!(corner.opacity, 0.14, "corner is between beats");
    }

    #[test]
    fn s1_wave_swells_and_rests() {
        let (opacity, scale) = s1_wave(0.0, 0.14);
        assert_eq!((opacity, scale), (0.14, 1.0));
        let (opacity, scale) = s1_wave(0.28, 0.14);
        assert!((opacity - 1.0).abs() < 1e-4, "peak opacity {opacity}");
        assert!((scale - 1.18).abs() < 1e-4, "peak scale {scale}");
        let (opacity, scale) = s1_wave(0.56, 0.14);
        assert!((opacity - 0.14).abs() < 1e-4);
        assert!((scale - 1.0).abs() < 1e-4);
        // The long tail stays at rest.
        let (opacity, scale) = s1_wave(0.9, 0.14);
        assert_eq!((opacity, scale), (0.14, 1.0));
    }

    #[test]
    fn g2_has_five_rings_of_eight_dots() {
        assert_eq!(globe_poses().len(), 40);
        for poses in globe_poses() {
            for pose in poses {
                assert!((0.12..=1.0).contains(&pose.opacity));
                assert!(pose.x.abs() <= GLOBE_R + 0.01);
                assert!(pose.y.abs() <= GLOBE_R + 0.01);
            }
        }
    }

    #[test]
    fn g2_interpolation_lands_on_keyframes_and_wraps() {
        // At exact keyframe fractions the pose equals the stored pose.
        for (pose, expected) in g2_poses(0.0).zip(globe_poses()[0].iter()) {
            assert!((pose.x - expected.x).abs() < 1e-4);
            assert!((pose.opacity - expected.opacity).abs() < 1e-4);
        }
        // Mid-step is between the two neighbouring keyframes.
        let mid: Vec<GlobePose> = g2_poses(0.5 / GLOBE_STEPS as f32).collect();
        let stored = globe_poses()[0];
        let first = mid[0];
        assert!(
            first.x > stored[0].x.min(stored[1].x) - 1e-4
                && first.x < stored[0].x.max(stored[1].x) + 1e-4
        );
    }
}
