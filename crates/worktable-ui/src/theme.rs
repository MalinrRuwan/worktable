//! Minimal theme stub for motion tests — mirrors `crate::theme` used in the original zeron tests.
//! Provides `neutral`, `ink`, and `lock_appearance` so `mix` tests don't need the full theme crate.

use gpui::{Hsla, Rgba};

/// A neutral grey at lightness `l` (0..1), alpha 1. Mirrors `theme::neutral`.
pub fn neutral(l: f32) -> Hsla {
    Hsla {
        h: 0.0,
        s: 0.0,
        l: l.clamp(0.0, 1.0),
        a: 1.0,
    }
}

/// Ink wash at alpha `a` (white with alpha), mirrors `theme::ink`.
pub fn ink(a: f32) -> Hsla {
    Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: a.clamp(0.0, 1.0),
    }
}

/// No-op guard for `theme::lock_appearance` used to pin appearance in tests.
pub fn lock_appearance() -> impl Drop {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {}
    }
    Guard
}

/// Helper to check Hsla -> Rgba conversion if needed in tests.
#[allow(dead_code)]
pub fn hsla_to_rgba(hsla: Hsla) -> Rgba {
    Rgba::from(hsla)
}
