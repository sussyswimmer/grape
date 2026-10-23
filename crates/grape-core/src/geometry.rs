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

/// How far (radians) the fillets must stop short of a segment's center
/// line, so the two fillets of a narrow segment never cross. The UI's
/// `RHO_ANGLE_MARGIN` in `apps/desktop/ui/src/wheel/geometry.ts`.
const FILLET_ANGLE_MARGIN: f64 = 1e-6;

/// Bisection steps for the largest fillet that fits when [`CORNER`] is too
/// big. The UI's `RHO_SEARCH_STEPS`.
const FILLET_SEARCH_STEPS: usize = 20;

/// True when `n` segments can be drawn: at least one, and for `n ≥ 2`
/// segments wide enough at [`INNER_R`] that the square inner corners don't
/// cross (the UI's `canDraw` with the spec constants). False from `n = 76`.
fn can_draw(n: usize) -> bool {
    match n {
        0 => false,
        1 => true,
        _ => PI / n as f64 - (GAP / 2.0 / INNER_R).asin() > 0.0,
    }
}

/// An outer-corner fillet of radius `rho`: the circle tangent to a
/// segment's gap edge and, from inside, to the outer circle.
///
/// Measured in the frame of the boundary ray (`u` along the ray, `w` the
/// distance from the ray's line toward the segment), its center is
/// `(u, offset)`. It touches the gap edge at `(u, GAP/2)` and the outer
/// circle at `angle` from the ray.
#[derive(Debug, Clone, Copy)]
struct Fillet {
    /// The radius.
    rho: f64,
    /// The center's `u`: `√((OUTER_R − rho)² − (GAP/2 + rho)²)`.
    u: f64,
    /// The center's `w`: `GAP/2 + rho`.
    offset: f64,
    /// The angle from the ray to the center and to the outer tangent point
    /// (the UI's `filletAngle`).
    angle: f64,
}

impl Fillet {
    /// The fillet of radius `rho`, or `None` when it can't touch both the
    /// gap edge and the outer circle.
    fn new(rho: f64) -> Option<Self> {
        let inset = OUTER_R - rho;
        let offset = GAP / 2.0 + rho;
        if inset <= offset {
            return None;
        }
        let u = (inset * inset - offset * offset).sqrt();
        Some(Self {
            rho,
            u,
            offset,
            angle: offset.atan2(u),
        })
    }

    /// True when a point `d` from the anchor and `alpha` radians from the
    /// nearer boundary ray (toward the segment) is in the outer corner this
    /// fillet cuts off: past the edge tangent point along the ray (`u` beyond
    /// the center's), closer to the ray than the outer tangent point
    /// (`alpha < angle`), and outside the fillet circle. Only meaningful for
    /// a point that is already on the ring and out of the gap.
    fn cuts_off(&self, d: f64, alpha: f64) -> bool {
        let u = d * alpha.cos();
        let w = d * alpha.sin();
        alpha < self.angle && u > self.u && (u - self.u).hypot(w - self.offset) > self.rho
    }
}

/// The radius of the rounded outer corners of `n` segments (§4): [`CORNER`],
/// or the largest radius that fits when the segments are too narrow for it.
/// The same rule as the UI's `filletRadius` in
/// `apps/desktop/ui/src/wheel/geometry.ts`, which draws the shape.
///
/// A radius fits when its fillet touches both the gap edge and the outer
/// circle, its outer tangent point stays at least `1e-6` rad short of the
/// segment's center line (so the two fillets never cross), and its edge
/// tangent point is no closer to the anchor than the square inner corner.
/// The largest fitting radius is found by a 20-step bisection on
/// `0..CORNER`. With the spec constants that is `CORNER` for `n ≤ 14` and
/// less from `n = 15`.
///
/// Returns `0.0` (square corners) when `n < 2` (one item is a full ring
/// with no corners), when the segments are too narrow to draw at all
/// (`n ≥ 76`), or when no radius fits.
pub fn fillet_radius(n: usize) -> f64 {
    if n < 2 || !can_draw(n) {
        return 0.0;
    }
    let h = PI / n as f64;
    let half = GAP / 2.0;
    let u_inner = (INNER_R * INNER_R - half * half).sqrt();
    let fits = |rho: f64| {
        Fillet::new(rho).is_some_and(|f| f.angle <= h - FILLET_ANGLE_MARGIN && f.u >= u_inner)
    };
    if fits(CORNER) {
        return CORNER;
    }
    let (mut lo, mut hi) = (0.0, CORNER);
    for _ in 0..FILLET_SEARCH_STEPS {
        let mid = (lo + hi) / 2.0;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The segment under the screen offset `(dx, dy)` from the anchor (y down),
/// or `None`. It matches the shape the UI draws (§4), because the overlay
/// gets no pointer events during an OS drag and can't re-check its path:
///
/// - `n == 0`, or any NaN input: `None`.
/// - `d < INNER_R` or `d > OUTER_R`, where `d` is the distance from the
///   anchor: `None`. `d == INNER_R` and `d == OUTER_R` are selectable.
/// - `n == 1`: the wheel is a full ring with no gap, so every other point
///   is `Some(0)`.
/// - `n ≥ 76`: the segments are too narrow to draw, so `None`.
/// - In a gap: the angular distance from the nearest segment's center
///   exceeds `π/n − asin(GAP/2 / d)`, so `None`.
/// - In a rounded-off outer corner: outside the fillet of radius
///   [`fillet_radius`]`(n)`, in the zone past both of its tangent points
///   (see `Fillet::cuts_off`), so `None`. The inner corners are square.
/// - Otherwise the nearest segment by angle.
pub fn hit_test(dx: f64, dy: f64, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let d = dx.hypot(dy);
    // The range check also rejects NaN.
    if !(INNER_R..=OUTER_R).contains(&d) {
        return None;
    }
    if n == 1 {
        return Some(0);
    }
    if !can_draw(n) {
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
    let half = PI / count;
    let half_width = half - (GAP / 2.0 / d).asin();
    if offset > half_width {
        return None;
    }
    let cut =
        Fillet::new(fillet_radius(n)).is_some_and(|f| f.rho > 0.0 && f.cuts_off(d, half - offset));
    if cut {
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
