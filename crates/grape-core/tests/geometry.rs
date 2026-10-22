//! Geometry and hit-test tests (`INTERACTION_SPEC.md` §3–§4).
#![allow(clippy::unwrap_used)]

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use grape_core::geometry::{
    clamp_anchor, hit_test, is_dense, start_angle, wheel_layout, Point, Rect, ANCHOR_MARGIN,
    BACKDROP_R, CORNER, DENSE_AT, GAP, GEOMETRY, INNER_R, LABEL_R, OUTER_R, SCREEN_MARGIN,
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
fn one_item_has_its_gap_at_the_bottom() {
    assert_eq!(hit_test(0.0, LABEL_R, 1), None);
    assert_eq!(hit_test(5.0, LABEL_R, 1), Some(0));
    assert_eq!(hit_test(LABEL_R, 0.0, 1), Some(0));
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
    /// in the gap.
    #[test]
    fn gaps_hit_nothing(
        n in 1usize..=24,
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
