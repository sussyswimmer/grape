//! Wheel geometry, hit testing and anchor clamping.
//!
//! Source of truth: `docs/INTERACTION_SPEC.md` §3 (anchor and placement) and
//! §4 (geometry). All lengths are logical points/DIPs at 1× scale; callers
//! convert physical pixels with the display's scale factor first.
//!
//! Angles are math-style radians, counter-clockwise from +x. Screen
//! coordinates have y growing **down**, so a math angle `θ` at radius `r` is
//! the screen offset `(r·cos θ, −r·sin θ)`.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use serde::{Deserialize, Serialize};

/// Dead-zone radius: nothing is selected inside it.
pub const INNER_R: f64 = 48.0;
/// Outer edge of the segments.
pub const OUTER_R: f64 = 120.0;
/// Glass backdrop radius.
pub const BACKDROP_R: f64 = 126.0;
/// Visual and hit-test gap between segments.
pub const GAP: f64 = 4.0;
/// Outer corner rounding of each segment.
pub const CORNER: f64 = 20.0;
/// Radius where labels and icons sit.
pub const LABEL_R: f64 = 84.0;
/// At this many items or more, use the compact label style.
pub const DENSE_AT: usize = 7;
/// Margin kept between the wheel and the work-area edge (§3).
pub const SCREEN_MARGIN: f64 = 14.0;
/// How far the anchor stays from each work-area edge: the backdrop radius
/// plus the screen margin, so the whole wheel and its margin stay visible.
pub const ANCHOR_MARGIN: f64 = BACKDROP_R + SCREEN_MARGIN;

/// The geometry constants as one value for the UI, so it never hard-codes
/// them. Serialized with camelCase keys (`innerR`, `outerR`, …).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Geometry {
    /// [`INNER_R`]
    pub inner_r: f64,
    /// [`OUTER_R`]
    pub outer_r: f64,
    /// [`BACKDROP_R`]
    pub backdrop_r: f64,
    /// [`GAP`]
    pub gap: f64,
    /// [`CORNER`]
    pub corner: f64,
    /// [`LABEL_R`]
    pub label_r: f64,
    /// [`DENSE_AT`]
    pub dense_at: usize,
    /// [`SCREEN_MARGIN`]
    pub screen_margin: f64,
}

/// The wheel's geometry constants (`INTERACTION_SPEC.md` §4).
pub const GEOMETRY: Geometry = Geometry {
    inner_r: INNER_R,
    outer_r: OUTER_R,
    backdrop_r: BACKDROP_R,
    gap: GAP,
    corner: CORNER,
    label_r: LABEL_R,
    dense_at: DENSE_AT,
    screen_margin: SCREEN_MARGIN,
};

/// One wheel segment. Angles are math-style radians (counter-clockwise from
/// +x), not normalized. The label point is a screen offset from the anchor
/// (y down).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    /// Item index, `0..n`.
    pub index: usize,
    /// Center angle: `start_angle(n) − 2π·index/n`.
    pub center: f64,
    /// Counter-clockwise edge: `center + π/n`. The segment runs clockwise
    /// from `start` to `end`.
    pub start: f64,
    /// Clockwise edge: `center − π/n`.
    pub end: f64,
    /// Label x offset from the anchor: `LABEL_R·cos(center)`.
    pub label_x: f64,
    /// Label y offset from the anchor, screen y down: `−LABEL_R·sin(center)`.
    pub label_y: f64,
}

/// A point in logical screen coordinates (y down).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    /// x, growing right.
    pub x: f64,
    /// y, growing down.
    pub y: f64,
}

/// A rectangle in logical screen coordinates (y down), e.g. a work area.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// The center angle of item 0: `π/2` (top), except `π` (left) when `n == 3`.
pub fn start_angle(n: usize) -> f64 {
    if n == 3 {
        PI
    } else {
        FRAC_PI_2
    }
}

/// True when `n` items should use the compact label style.
pub fn is_dense(n: usize) -> bool {
    n >= DENSE_AT
}

/// The `n` equal segments, item 0 first, advancing clockwise.
pub fn wheel_layout(n: usize) -> Vec<Segment> {
    if n == 0 {
        return Vec::new();
    }
    let count = n as f64;
    let step = TAU / count;
    let half = PI / count;
    let start = start_angle(n);
    (0..n)
        .map(|index| {
            let center = start - step * index as f64;
            Segment {
                index,
                center,
                start: center + half,
                end: center - half,
                label_x: LABEL_R * center.cos(),
                label_y: -LABEL_R * center.sin(),
            }
        })
        .collect()
}

/// The segment under the screen offset `(dx, dy)` from the anchor (y down),
/// or `None` (§4):
///
/// - `n == 0`, or any NaN input.
/// - `d < INNER_R` or `d > OUTER_R`, where `d` is the distance from the
///   anchor. `d == INNER_R` and `d == OUTER_R` are selectable.
/// - In a gap: the angular distance from the nearest segment's center
///   exceeds `π/n − asin(GAP/2 / d)`.
///
/// The UI may additionally check its rendered path for rounded corners.
pub fn hit_test(dx: f64, dy: f64, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let d = dx.hypot(dy);
    // The range check also rejects NaN.
    if !(INNER_R..=OUTER_R).contains(&d) {
        return None;
    }
    let theta = (-dy).atan2(dx);
    let count = n as f64;
    let step = TAU / count;
    let start = start_angle(n);
    // `round()` gives an integer-valued float, so `rem_euclid` is exact and
    // lands in `0..n`.
    let k = ((start - theta) / step).round().rem_euclid(count);
    let index = (k as usize).min(n - 1);
    let center = start - step * index as f64;
    let offset = wrap_angle(theta - center).abs();
    let half_width = PI / count - (GAP / 2.0 / d).asin();
    if offset > half_width {
        None
    } else {
        Some(index)
    }
}

/// Wrap an angle into `[−π, π)`.
fn wrap_angle(a: f64) -> f64 {
    (a + PI).rem_euclid(TAU) - PI
}

/// Clamp the wheel's anchor so the backdrop plus [`SCREEN_MARGIN`] stays
/// inside `work_area` (§3). If an axis of the work area is too small for
/// that (under `2 × ANCHOR_MARGIN`), the anchor is centered on that axis.
/// Never panics, even for NaN input.
pub fn clamp_anchor(anchor: Point, work_area: Rect) -> Point {
    Point {
        x: clamp_axis(anchor.x, work_area.x, work_area.width),
        y: clamp_axis(anchor.y, work_area.y, work_area.height),
    }
}

fn clamp_axis(value: f64, origin: f64, length: f64) -> f64 {
    let lo = origin + ANCHOR_MARGIN;
    let hi = origin + length - ANCHOR_MARGIN;
    if lo <= hi {
        value.max(lo).min(hi)
    } else {
        origin + length / 2.0
    }
}
