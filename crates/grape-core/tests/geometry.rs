//! Geometry and hit-test tests (`INTERACTION_SPEC.md` §3–§4).
#![allow(clippy::unwrap_used)]

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use grape_core::geometry::{
    clamp_anchor, fillet_radius, hit_test, is_dense, start_angle, wheel_layout, Point, Rect,
    ANCHOR_MARGIN, BACKDROP_R, CORNER, DENSE_AT, GAP, GEOMETRY, INNER_R, LABEL_R, OUTER_R,
    SCREEN_MARGIN,
};
use proptest::prelude::*;

/// Screen offset of a math angle `theta` at radius `r` (y down).
fn at(theta: f64, r: f64) -> (f64, f64) {
    (r * theta.cos(), -r * theta.sin())
}

fn center(n: usize, i: usize) -> f64 {
    start_angle(n) - TAU * i as f64 / n as f64
}

#[test]
fn constants_match_spec() {
    assert_eq!(INNER_R, 48.0);
    assert_eq!(OUTER_R, 120.0);
    assert_eq!(BACKDROP_R, 126.0);
    assert_eq!(GAP, 4.0);
    assert_eq!(CORNER, 20.0);
    assert_eq!(LABEL_R, 84.0);
    assert_eq!(DENSE_AT, 7);
    assert_eq!(SCREEN_MARGIN, 14.0);
    assert_eq!(ANCHOR_MARGIN, 140.0);
}

#[test]
fn geometry_serializes_for_the_ui() {
    let json = serde_json::to_value(GEOMETRY).unwrap();
    assert_eq!(json["innerR"], 48.0);
    assert_eq!(json["outerR"], 120.0);
    assert_eq!(json["backdropR"], 126.0);
    assert_eq!(json["gap"], 4.0);
    assert_eq!(json["corner"], 20.0);
    assert_eq!(json["labelR"], 84.0);
    assert_eq!(json["denseAt"], 7);
    assert_eq!(json["screenMargin"], 14.0);
}

#[test]
fn dense_from_seven_items() {
    assert!(!is_dense(6));
    assert!(is_dense(7));
    assert!(is_dense(12));
}

#[test]
fn start_angles() {
    assert_eq!(start_angle(3), PI);
    for n in [1, 2, 4, 5, 6, 7, 8, 12] {
        assert_eq!(start_angle(n), FRAC_PI_2, "n={n}");
    }
}

#[test]
fn item_0_is_at_the_top_except_for_three() {
    for n in 1..=12 {
        if n == 3 {
            continue;
        }
        assert_eq!(hit_test(0.0, -LABEL_R, n), Some(0), "n={n}");
    }
}

#[test]
fn item_0_is_on_the_left_for_three() {
    assert_eq!(hit_test(-LABEL_R, 0.0, 3), Some(0));
}

#[test]
fn three_items_advance_clockwise_from_the_left() {
    let (x, y) = at(PI / 3.0, LABEL_R);
    assert!(x > 0.0 && y < 0.0, "item 1 is upper-right");
    assert_eq!(hit_test(x, y, 3), Some(1));
    let (x, y) = at(-PI / 3.0, LABEL_R);
    assert!(x > 0.0 && y > 0.0, "item 2 is lower-right");
    assert_eq!(hit_test(x, y, 3), Some(2));
}

#[test]
fn four_items_go_clockwise() {
    assert_eq!(hit_test(0.0, -LABEL_R, 4), Some(0)); // top
    assert_eq!(hit_test(LABEL_R, 0.0, 4), Some(1)); // right
    assert_eq!(hit_test(0.0, LABEL_R, 4), Some(2)); // bottom
    assert_eq!(hit_test(-LABEL_R, 0.0, 4), Some(3)); // left
}

#[test]
fn ring_edges_are_selectable() {
    assert_eq!(hit_test(0.0, -INNER_R, 4), Some(0));
    assert_eq!(hit_test(0.0, -OUTER_R, 4), Some(0));
    assert_eq!(hit_test(0.0, -(INNER_R - 1e-9), 4), None);
    assert_eq!(hit_test(0.0, -(OUTER_R + 1e-9), 4), None);
}

#[test]
fn anchor_itself_is_dead() {
    for n in 0..=12 {
        assert_eq!(hit_test(0.0, 0.0, n), None);
    }
}

#[test]
fn zero_items_hit_nothing() {
    assert_eq!(hit_test(0.0, -LABEL_R, 0), None);
    assert!(wheel_layout(0).is_empty());
}

#[test]
fn nan_and_infinity_hit_nothing() {
    for n in [1, 3, 6] {
        assert_eq!(hit_test(f64::NAN, -LABEL_R, n), None);
        assert_eq!(hit_test(0.0, f64::NAN, n), None);
        assert_eq!(hit_test(f64::INFINITY, 0.0, n), None);
        assert_eq!(hit_test(0.0, f64::NEG_INFINITY, n), None);
    }
}

#[test]
fn one_item_is_a_full_ring_with_no_gap() {
    // Straight below the anchor, where n ≥ 2 would put a boundary ray.
    assert_eq!(hit_test(0.0, LABEL_R, 1), Some(0));
    assert_eq!(hit_test(0.0, INNER_R, 1), Some(0));
    assert_eq!(hit_test(0.0, OUTER_R, 1), Some(0));
    for dx in [-GAP, -GAP / 2.0, -0.5, 0.5, GAP / 2.0, GAP] {
        assert_eq!(hit_test(dx, LABEL_R, 1), Some(0), "dx={dx}");
    }
    // Every angle, across the ring.
    for k in 0..3600 {
        let theta = TAU * f64::from(k) / 3600.0;
        for r in [INNER_R + 1e-9, 60.0, LABEL_R, 100.0, OUTER_R - 1e-9] {
            let (x, y) = at(theta, r);
            assert_eq!(hit_test(x, y, 1), Some(0), "theta={theta} r={r}");
        }
    }
    // The ring's edges still apply.
    assert_eq!(hit_test(0.0, INNER_R - 1e-9, 1), None);
    assert_eq!(hit_test(0.0, OUTER_R + 1e-9, 1), None);
}

#[test]
fn too_narrow_segments_hit_nothing() {
    // From n = 76 the square inner corners would cross, so the UI draws
    // nothing (`canDraw`) and nothing is selectable.
    assert!(PI / 75.0 > (GAP / 2.0 / INNER_R).asin());
    assert!(PI / 76.0 < (GAP / 2.0 / INNER_R).asin());
    for i in 0..5 {
        let (x, y) = at(center(75, i), 110.0);
        assert_eq!(hit_test(x, y, 75), Some(i), "n=75 i={i}");
        let (x, y) = at(center(76, i), 110.0);
        assert_eq!(hit_test(x, y, 76), None, "n=76 i={i}");
    }
}

// ---- Rounded outer corners (fillets) ----

/// The UI's `fromFrame`: `u` along the ray at math angle `beta`, `v`
/// perpendicular (math orientation), to a screen offset (y down).
fn from_frame(u: f64, v: f64, beta: f64) -> (f64, f64) {
    let (sin, cos) = beta.sin_cos();
    (u * cos - v * sin, -(u * sin + v * cos))
}

/// The inverse of [`from_frame`].
fn to_frame(x: f64, y: f64, beta: f64) -> (f64, f64) {
    let (sin, cos) = beta.sin_cos();
    let my = -y;
    (x * cos + my * sin, -x * sin + my * cos)
}

/// One radial edge of a drawn segment, as `geometry.ts`'s `edgeCorners`
/// builds it: the boundary ray `beta` and the side the segment is on
/// (`-1.0` for the start edge at `center + π/n`, `1.0` for the end edge).
struct Edge {
    beta: f64,
    side: f64,
}

fn edges(n: usize, i: usize) -> [Edge; 2] {
    let h = PI / n as f64;
    let c = center(n, i);
    [
        Edge {
            beta: c + h,
            side: -1.0,
        },
        Edge {
            beta: c - h,
            side: 1.0,
        },
    ]
}

/// The fillet of radius `rho` in its edge's frame: `(u_c, gap/2 + rho)`.
fn fillet_center_frame(rho: f64) -> (f64, f64) {
    let inset = OUTER_R - rho;
    let offset = GAP / 2.0 + rho;
    ((inset * inset - offset * offset).sqrt(), offset)
}

/// How far `(x, y)` is inside the drawn outline of segment `i` of `n`
/// (positive inside, negative outside; the minimum over the outline's
/// constraints, so a lower bound on the true distance). This is the UI's
/// shape from `segmentPath`, written with the frame vectors of
/// `segmentCorners` rather than `hit_test`'s angles:
///
/// - `n == 1`: the annulus.
/// - Otherwise nothing when the UI can't draw it (`canDraw`), else the
///   annulus between the two gap edges, minus each outer corner outside
///   its fillet: a point past the edge tangent point (`u > u_c`) and on the
///   ray's side of the line from the anchor through the fillet center is
///   inside only within `rho` (the UI's `filletRadius(n)`) of that center.
fn drawn_margin(x: f64, y: f64, n: usize, i: usize, rho: f64) -> f64 {
    let d = x.hypot(y);
    let ring = (d - INNER_R).min(OUTER_R - d);
    if n == 1 {
        return ring;
    }
    if n == 0 || PI / n as f64 - (GAP / 2.0 / INNER_R).asin() <= 0.0 {
        return f64::NEG_INFINITY;
    }
    let (u_c, offset) = fillet_center_frame(rho);
    let mut margin = ring;
    for edge in edges(n, i) {
        let (u, v) = to_frame(x, y, edge.beta);
        // Distance from the ray's line, toward the segment.
        let w = edge.side * v;
        margin = margin.min(w - GAP / 2.0);
        // `u·offset − w·u_c > 0`: closer to the ray than the line through
        // the anchor and the fillet center (and its outer tangent point).
        if rho > 0.0 && u > u_c && u * offset - w * u_c > 0.0 {
            margin = margin.min(rho - (u - u_c).hypot(w - offset));
        }
    }
    margin
}

/// `hit_test` agrees with [`drawn_margin`] at `(x, y)`: a hit is inside its
/// drawn segment, and a point clearly inside a drawn segment hits it.
fn agrees_with_drawing(x: f64, y: f64, n: usize) -> Result<(), String> {
    const SLACK: f64 = 1e-9;
    let rho = fillet_radius(n);
    let hit = hit_test(x, y, n);
    if let Some(i) = hit {
        let m = drawn_margin(x, y, n, i, rho);
        if m < -SLACK {
            return Err(format!("({x}, {y}) n={n} hits {i} but is {m} outside it"));
        }
    }
    for i in 0..n {
        let m = drawn_margin(x, y, n, i, rho);
        if m > SLACK && hit != Some(i) {
            return Err(format!(
                "({x}, {y}) n={n} is {m} inside {i} but hits {hit:?}"
            ));
        }
    }
    Ok(())
}

#[test]
fn fillet_radius_is_the_full_corner_up_to_fourteen_items() {
    for n in 2..=14 {
        assert_eq!(fillet_radius(n), CORNER, "n={n}");
    }
    assert_eq!(fillet_radius(12), CORNER);
    // The two fillets of a fifteenth of the ring would cross.
    let first_smaller = (2..=75).find(|&n| fillet_radius(n) < CORNER);
    assert_eq!(first_smaller, Some(15));
    assert!(fillet_radius(15) > 0.0);
}

#[test]
fn fillet_radius_is_zero_without_corners() {
    assert_eq!(fillet_radius(0), 0.0);
    assert_eq!(fillet_radius(1), 0.0, "a full ring has no corners");
    assert!(fillet_radius(75) > 0.0);
    for n in [76, 100, 500, usize::MAX] {
        assert_eq!(fillet_radius(n), 0.0, "n={n} can't be drawn");
    }
}

#[test]
fn fillet_radius_shrinks_to_the_largest_that_fits() {
    let u_inner = (INNER_R * INNER_R - (GAP / 2.0) * (GAP / 2.0)).sqrt();
    let angle = |rho: f64| {
        let (u_c, offset) = fillet_center_frame(rho);
        offset.atan2(u_c)
    };
    let tolerance = CORNER / 2f64.powi(20);
    let mut previous = f64::INFINITY;
    for n in 2..=75 {
        let h = PI / n as f64;
        let rho = fillet_radius(n);
        assert!(rho <= previous, "n={n}: never grows");
        assert!(rho > 0.0, "n={n}");
        // It fits: the fillets stop 1e-6 short of the center line, and the
        // edge tangent point is past the square inner corner.
        assert!(angle(rho) <= h - 1e-6, "n={n}");
        assert!(fillet_center_frame(rho).0 >= u_inner, "n={n}");
        // And it's the largest, within the 20-step bisection.
        if rho < CORNER {
            assert!(angle(rho + 2.0 * tolerance) > h - 1e-6, "n={n}");
        }
        previous = rho;
    }
}

#[test]
fn fillet_centers_hit_their_segment() {
    for n in 2..=24 {
        let (u_c, offset) = fillet_center_frame(fillet_radius(n));
        for i in 0..n {
            for edge in edges(n, i) {
                let (x, y) = from_frame(u_c, edge.side * offset, edge.beta);
                assert_eq!(hit_test(x, y, n), Some(i), "n={n} i={i}");
            }
        }
    }
}

#[test]
fn cut_off_corners_hit_nothing() {
    let half = GAP / 2.0;
    // The unrounded outer corner: the gap edge meets the outer circle.
    let k_u = (OUTER_R * OUTER_R - half * half).sqrt();
    for n in 2..=12 {
        let rho = fillet_radius(n);
        assert_eq!(rho, CORNER);
        let (u_c, offset) = fillet_center_frame(rho);
        let (to_u, to_w) = (k_u - u_c, half - offset);
        let to_len = to_u.hypot(to_w);
        assert!(to_len > rho + 0.5, "n={n}: the corner is beyond the arc");
        for i in 0..n {
            for edge in edges(n, i) {
                // From the fillet center toward the unrounded corner.
                let point = |dist: f64| {
                    let s = dist / to_len;
                    from_frame(u_c + s * to_u, edge.side * (offset + s * to_w), edge.beta)
                };
                let where_ = format!("n={n} i={i} beta={}", edge.beta);
                // Just outside the fillet arc, still inside the old square
                // corner (on the ring and out of the gap): cut off.
                let (x, y) = point(rho + 0.5);
                assert!(x.hypot(y) < OUTER_R, "{where_}");
                let (_, v) = to_frame(x, y, edge.beta);
                assert!(edge.side * v > half, "{where_}");
                assert_eq!(hit_test(x, y, n), None, "{where_}: outside the fillet");
                // The unrounded corner itself, nudged inward.
                let (x, y) = point(to_len - 0.25);
                assert_eq!(hit_test(x, y, n), None, "{where_}: square corner");
                // Just inside the arc: drawn, so it hits.
                let (x, y) = point(rho - 0.5);
                assert_eq!(hit_test(x, y, n), Some(i), "{where_}: inside the fillet");
            }
        }
    }
}

#[test]
fn edges_and_outer_arc_hit_up_to_the_tangent_points() {
    let half = GAP / 2.0;
    for n in 2..=12 {
        let rho = fillet_radius(n);
        let (u_c, offset) = fillet_center_frame(rho);
        let tangent_angle = offset.atan2(u_c);
        for i in 0..n {
            for edge in edges(n, i) {
                let where_ = format!("n={n} i={i} beta={}", edge.beta);
                // Along the gap edge, short of the edge tangent point: hits.
                let (x, y) = from_frame(u_c - 0.5, edge.side * (half + 0.01), edge.beta);
                assert_eq!(hit_test(x, y, n), Some(i), "{where_}: edge");
                // Along the outer circle, past the outer tangent point: hits.
                let beta = edge.beta + edge.side * (tangent_angle + 0.01);
                let (x, y) = at(beta, OUTER_R - 0.01);
                assert_eq!(hit_test(x, y, n), Some(i), "{where_}: outer arc");
            }
        }
    }
}

#[test]
fn hit_test_matches_the_drawn_shape_on_a_grid() {
    let step = 1.0;
    let mut checked = 0usize;
    for n in 1..=16 {
        let mut y = -125.0;
        while y <= 125.0 {
            let mut x = -125.0;
            while x <= 125.0 {
                if let Err(e) = agrees_with_drawing(x, y, n) {
                    panic!("{e}");
                }
                checked += 1;
                x += step;
            }
            y += step;
        }
    }
    assert_eq!(checked, 16 * 251 * 251);
}

#[test]
fn gap_between_top_and_right_for_four() {
    // Boundary ray at 45° (math), i.e. up-right on screen.
    let (x, y) = at(PI / 4.0, LABEL_R);
    assert_eq!(hit_test(x, y, 4), None);
}

#[test]
fn huge_n_has_no_selectable_area() {
    // π/n < asin(GAP/2/d) everywhere in the ring.
    for i in 0..10 {
        let (x, y) = at(center(500, i), LABEL_R);
        assert_eq!(hit_test(x, y, 500), None);
    }
}

#[test]
fn layout_matches_spec_formula() {
    for n in 1..=12 {
        let layout = wheel_layout(n);
        assert_eq!(layout.len(), n);
        for (i, s) in layout.iter().enumerate() {
            assert_eq!(s.index, i);
            assert!((s.center - center(n, i)).abs() < 1e-12);
            assert!((s.start - s.end - TAU / n as f64).abs() < 1e-12);
            assert!((s.start - s.center - PI / n as f64).abs() < 1e-12);
            assert!((s.label_x - LABEL_R * s.center.cos()).abs() < 1e-12);
            assert!((s.label_y + LABEL_R * s.center.sin()).abs() < 1e-12);
        }
    }
}

#[test]
fn every_label_hits_its_own_segment() {
    for n in 1..=24 {
        for s in wheel_layout(n) {
            assert_eq!(hit_test(s.label_x, s.label_y, n), Some(s.index), "n={n}");
        }
    }
}

#[test]
fn segment_serializes_camel_case() {
    let json = serde_json::to_value(wheel_layout(4)[0]).unwrap();
    assert_eq!(json["index"], 0);
    assert!(json.get("labelX").is_some());
    assert!(json.get("labelY").is_some());
}

// ---- clamp_anchor ----

const SCREEN: Rect = Rect {
    x: 0.0,
    y: 0.0,
    width: 1920.0,
    height: 1040.0,
};

#[test]
fn anchor_inside_is_unchanged() {
    let p = Point { x: 900.0, y: 500.0 };
    assert_eq!(clamp_anchor(p, SCREEN), p);
}

#[test]
fn anchor_near_edges_is_pulled_in() {
    let p = clamp_anchor(Point { x: 3.0, y: 1039.0 }, SCREEN);
    assert_eq!(p, Point { x: 140.0, y: 900.0 });
    let p = clamp_anchor(Point { x: 1919.0, y: 0.0 }, SCREEN);
    assert_eq!(
        p,
        Point {
            x: 1780.0,
            y: 140.0
        }
    );
}

#[test]
fn anchor_respects_offset_work_areas() {
    // A second monitor left of the primary, below a top taskbar.
    let area = Rect {
        x: -1280.0,
        y: 40.0,
        width: 1280.0,
        height: 984.0,
    };
    let p = clamp_anchor(
        Point {
            x: -1279.0,
            y: 41.0,
        },
        area,
    );
    assert_eq!(
        p,
        Point {
            x: -1140.0,
            y: 180.0
        }
    );
}

#[test]
fn tiny_work_area_centers_the_anchor() {
    let area = Rect {
        x: 100.0,
        y: 100.0,
        width: 200.0,
        height: 1000.0,
    };
    let p = clamp_anchor(Point { x: 0.0, y: 0.0 }, area);
    assert_eq!(p, Point { x: 200.0, y: 240.0 });
}

#[test]
fn exact_fit_work_area() {
    let area = Rect {
        x: 0.0,
        y: 0.0,
        width: 280.0,
        height: 280.0,
    };
    let p = clamp_anchor(Point { x: 0.0, y: 999.0 }, area);
    assert_eq!(p, Point { x: 140.0, y: 140.0 });
}

#[test]
fn clamp_never_panics_on_nan() {
    let p = clamp_anchor(
        Point {
            x: f64::NAN,
            y: 10.0,
        },
        SCREEN,
    );
    assert_eq!(p.x, 140.0);
    let bad = Rect {
        x: f64::NAN,
        ..SCREEN
    };
    let p = clamp_anchor(Point { x: 5.0, y: 5.0 }, bad);
    assert!(p.x.is_nan());
    assert_eq!(p.y, 140.0);
}

// ---- Property tests ----

proptest! {
    #[test]
    fn dead_zone_hits_nothing(
        r in 0.0..(INNER_R - 1e-6),
        theta in -PI..PI,
        n in 0usize..=32,
    ) {
        let (x, y) = at(theta, r);
        prop_assert_eq!(hit_test(x, y, n), None);
    }

    #[test]
    fn outside_the_ring_hits_nothing(
        r in (OUTER_R + 1e-6)..1.0e6,
        theta in -PI..PI,
        n in 0usize..=32,
    ) {
        let (x, y) = at(theta, r);
        prop_assert_eq!(hit_test(x, y, n), None);
    }

    /// Points closer than GAP/2 to a boundary ray between two segments are
    /// in the gap. (One item has no boundary rays: see the next property.)
    #[test]
    fn gaps_hit_nothing(
        n in 2usize..=24,
        i_seed in 0usize..1000,
        r in INNER_R..OUTER_R,
        s in -(GAP / 2.0 - 1e-6)..(GAP / 2.0 - 1e-6),
    ) {
        let i = i_seed % n;
        let boundary = center(n, i) - PI / n as f64;
        // Along the boundary ray by r, then sideways by s (math coords).
        let mx = r * boundary.cos() - s * boundary.sin();
        let my = r * boundary.sin() + s * boundary.cos();
        prop_assert_eq!(hit_test(mx, -my, n), None);
    }

    /// One item is a full ring: no gap anywhere, no corners.
    #[test]
    fn one_item_hits_the_whole_ring(
        r in (INNER_R + 1e-9)..(OUTER_R - 1e-9),
        theta in -PI..PI,
    ) {
        let (x, y) = at(theta, r);
        prop_assert_eq!(hit_test(x, y, 1), Some(0));
    }

    /// Every hit is inside the drawn outline of its segment, and every
    /// point clearly inside a drawn outline hits that segment.
    #[test]
    fn hits_are_inside_the_drawn_shape(
        dx in -130.0..130.0f64,
        dy in -130.0..130.0f64,
        n in 1usize..=80,
    ) {
        prop_assert_eq!(agrees_with_drawing(dx, dy, n), Ok(()));
    }

    /// The same, sampled around the fillets, where the outer corners are
    /// rounded off.
    #[test]
    fn corners_match_the_fillets(
        n in 2usize..=24,
        i_seed in 0usize..1000,
        start_edge in any::<bool>(),
        r in 0.0..(2.0 * CORNER),
        psi in -PI..PI,
    ) {
        let i = i_seed % n;
        let [start, end] = edges(n, i);
        let edge = if start_edge { start } else { end };
        let (u_c, offset) = fillet_center_frame(fillet_radius(n));
        let (cx, cy) = from_frame(u_c, edge.side * offset, edge.beta);
        let (x, y) = (cx + r * psi.cos(), cy + r * psi.sin());
        prop_assert_eq!(agrees_with_drawing(x, y, n), Ok(()));
    }

    #[test]
    fn center_rays_hit_their_segment(
        n in 1usize..=24,
        i_seed in 0usize..1000,
        r in (INNER_R + 1e-6)..(OUTER_R - 1e-6),
    ) {
        let i = i_seed % n;
        let (x, y) = at(center(n, i), r);
        prop_assert_eq!(hit_test(x, y, n), Some(i));
    }

    #[test]
    fn zero_items_never_hit(dx in -500.0..500.0f64, dy in -500.0..500.0f64) {
        prop_assert_eq!(hit_test(dx, dy, 0), None);
    }

    #[test]
    fn hits_are_in_range(dx in -200.0..200.0f64, dy in -200.0..200.0f64, n in 0usize..=64) {
        if let Some(i) = hit_test(dx, dy, n) {
            prop_assert!(i < n);
        }
    }

    #[test]
    fn hits_are_inside_the_ring(dx in -200.0..200.0f64, dy in -200.0..200.0f64, n in 1usize..=16) {
        if hit_test(dx, dy, n).is_some() {
            let d = dx.hypot(dy);
            prop_assert!((INNER_R..=OUTER_R).contains(&d));
        }
    }

    #[test]
    fn clamped_anchor_keeps_the_wheel_on_screen(
        x in -5000.0..5000.0f64,
        y in -5000.0..5000.0f64,
        ax in -3000.0..3000.0f64,
        ay in -3000.0..3000.0f64,
        w in 280.0..4000.0f64,
        h in 280.0..4000.0f64,
    ) {
        let area = Rect { x: ax, y: ay, width: w, height: h };
        let p = clamp_anchor(Point { x, y }, area);
        let eps = 1e-9;
        prop_assert!(p.x - ANCHOR_MARGIN >= ax - eps);
        prop_assert!(p.x + ANCHOR_MARGIN <= ax + w + eps);
        prop_assert!(p.y - ANCHOR_MARGIN >= ay - eps);
        prop_assert!(p.y + ANCHOR_MARGIN <= ay + h + eps);
    }
}
