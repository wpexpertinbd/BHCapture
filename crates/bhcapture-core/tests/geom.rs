//! Coordinate math tests.
//!
//! Test discipline from PLAN.md:
//! - section 0 pins a known-good case, so a broken rig cannot masquerade as working code;
//! - assertions target the MECHANISM, never the spelling of the source;
//! - the DPI matrix is a first-class test target, not a manual check.

use bhcapture_core::geom::*;

fn sc(v: f64) -> Scale { Scale::exact(v) }

// ---------------------------------------------------------------------------
// 0. Self-check. If these fail, nothing below means anything.
// ---------------------------------------------------------------------------

#[test]
fn section0_rig_selfcheck() {
    let r = LogRect::new(0.0, 0.0, 100.0, 50.0);
    assert_eq!(r.max_x(), 100.0);
    assert_eq!(r.max_y(), 50.0);
    // 1x must be the identity, in both directions.
    assert_eq!(r.to_physical(sc(1.0)), PhysRect::new(0, 0, 100, 50));
    assert_eq!(PhysRect::new(0, 0, 100, 50).to_logical(sc(1.0)), r);
}

// ---------------------------------------------------------------------------
// 1. Scale rejects values that would silently produce nonsense.
// ---------------------------------------------------------------------------

#[test]
fn scale_rejects_degenerate_values() {
    for bad in [0.0, -1.0, -0.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(Scale::new(bad).is_none(), "scale {bad} should be rejected");
    }
    for good in [0.5, 1.0, 1.25, 2.0, 3.0] {
        assert!(Scale::new(good).is_some(), "scale {good} should be accepted");
    }
}

// ---------------------------------------------------------------------------
// 2. A drag works in all four directions.
// ---------------------------------------------------------------------------

#[test]
fn drag_normalises_from_any_direction() {
    let a = LogPoint::new(100.0, 80.0);
    let b = LogPoint::new(40.0, 20.0);
    let expected = LogRect::new(40.0, 20.0, 60.0, 60.0);
    // All four corner orderings must produce the same rectangle.
    assert_eq!(LogRect::from_corners(a, b), expected);
    assert_eq!(LogRect::from_corners(b, a), expected);
    assert_eq!(LogRect::from_corners(LogPoint::new(40.0, 80.0), LogPoint::new(100.0, 20.0)), expected);
    assert_eq!(LogRect::from_corners(LogPoint::new(100.0, 20.0), LogPoint::new(40.0, 80.0)), expected);
}

#[test]
fn zero_size_drag_is_empty_not_negative() {
    let p = LogPoint::new(10.0, 10.0);
    let r = LogRect::from_corners(p, p);
    assert!(r.is_empty());
    assert!(r.to_physical(sc(2.0)).is_empty());
}

// ---------------------------------------------------------------------------
// 3. THE rounding property: edges, not origin-and-size.
// ---------------------------------------------------------------------------

/// Adjacent logical rectangles must stay exactly adjacent in physical pixels — no seam, no
/// overlap. This is the property that breaks when origin and size are rounded independently,
/// and it is asserted behaviourally rather than by pinning any particular arithmetic.
#[test]
fn rounding_is_edge_consistent() {
    for &scale in &[1.0, 1.25, 1.5, 2.0, 3.0] {
        for &start in &[0.0, 0.3, 0.5, 0.7, 10.3, 99.9] {
            for &w in &[1.0, 10.4, 33.3, 100.7] {
                let a = LogRect::new(start, 0.0, w, 10.0);
                let b = LogRect::new(start + w, 0.0, w, 10.0);
                let (pa, pb) = (a.to_physical(sc(scale)), b.to_physical(sc(scale)));
                assert_eq!(
                    pa.max_x(), pb.min_x(),
                    "seam at scale={scale} start={start} w={w}: {pa:?} then {pb:?}"
                );
            }
        }
    }
}

/// Guards the *reason* the above holds: deriving width from rounded edges is not the same as
/// rounding the width directly. If this case ever stops differing, `rounding_is_edge_consistent`
/// has quietly become unable to detect the bug it exists to catch.
#[test]
fn edge_rounding_actually_differs_from_naive_rounding() {
    let r = LogRect::new(10.3, 0.0, 10.4, 10.0);
    let k = 2.0;
    let edge_derived = r.to_physical(sc(k)).size.w;
    let naive = (r.size.w * k).round() as u32;
    assert_ne!(edge_derived, naive, "this case no longer distinguishes the two policies");
    assert_eq!(edge_derived, 20);
    assert_eq!(naive, 21);
}

#[test]
fn inverted_edges_clamp_to_empty_not_to_four_billion() {
    let r = PhysRect::from_edges(100, 100, 40, 40);
    assert!(r.is_empty());
    assert_eq!(r.size, PhysSize { w: 0, h: 0 });
}

// ---------------------------------------------------------------------------
// 4. The DPI matrix.
// ---------------------------------------------------------------------------

#[test]
fn dpi_matrix_roundtrips_and_scales() {
    // (scale, logical rect, expected physical)
    let cases: &[(f64, LogRect, PhysRect)] = &[
        (1.0,  LogRect::new(0.0, 0.0, 1920.0, 1080.0), PhysRect::new(0, 0, 1920, 1080)),
        (2.0,  LogRect::new(0.0, 0.0, 1352.0, 878.0),  PhysRect::new(0, 0, 2704, 1756)),
        (2.0,  LogRect::new(10.0, 20.0, 100.0, 50.0),  PhysRect::new(20, 40, 200, 100)),
        (1.5,  LogRect::new(0.0, 0.0, 1280.0, 800.0),  PhysRect::new(0, 0, 1920, 1200)),
        (1.25, LogRect::new(0.0, 0.0, 1536.0, 864.0),  PhysRect::new(0, 0, 1920, 1080)),
        (3.0,  LogRect::new(5.0, 5.0, 10.0, 10.0),     PhysRect::new(15, 15, 30, 30)),
        // negative origins: a display left of / above the primary
        (2.0,  LogRect::new(-1352.0, -878.0, 100.0, 100.0), PhysRect::new(-2704, -1756, 200, 200)),
        (1.0,  LogRect::new(-1920.0, 0.0, 1920.0, 1080.0),  PhysRect::new(-1920, 0, 1920, 1080)),
    ];
    for (k, log, want) in cases {
        let got = log.to_physical(sc(*k));
        assert_eq!(got, *want, "scale {k} on {log:?}");
        // Whole-pixel cases must survive a round trip exactly.
        assert_eq!(got.to_logical(sc(*k)), *log, "roundtrip at scale {k}");
    }
}

// ---------------------------------------------------------------------------
// 5. Layout: negative coordinates, gaps, and lookup.
// ---------------------------------------------------------------------------

fn display(id: u32, x: f64, y: f64, w: f64, h: f64, k: f64) -> Display {
    Display {
        id: DisplayId(id),
        bounds: LogRect::new(x, y, w, h),
        physical: PhysSize { w: (w * k) as u32, h: (h * k) as u32 },
        scale: sc(k),
    }
}

/// Retina laptop at the origin, 1080p monitor to its LEFT (negative x) and one ABOVE it
/// (negative y) — the two arrangements PLAN.md names.
fn mixed_layout() -> DisplayLayout {
    DisplayLayout::new(vec![
        display(1, 0.0, 0.0, 1352.0, 878.0, 2.0),        // built-in Retina
        display(2, -1920.0, 0.0, 1920.0, 1080.0, 1.0),   // to the left
        display(3, 0.0, -1080.0, 1920.0, 1080.0, 1.0),   // above
    ])
}

#[test]
fn virtual_bounds_spans_negative_space() {
    let vb = mixed_layout().virtual_bounds().unwrap();
    assert_eq!(vb.min_x(), -1920.0);
    assert_eq!(vb.min_y(), -1080.0);
    assert_eq!(vb.max_x(), 1920.0);
    assert_eq!(vb.max_y(), 1080.0);
}

#[test]
fn empty_layout_has_no_bounds() {
    assert!(DisplayLayout::default().virtual_bounds().is_none());
}

#[test]
fn display_at_finds_the_right_screen_and_rejects_gaps() {
    let l = mixed_layout();
    assert_eq!(l.display_at(LogPoint::new(10.0, 10.0)).unwrap().id, DisplayId(1));
    assert_eq!(l.display_at(LogPoint::new(-100.0, 10.0)).unwrap().id, DisplayId(2));
    assert_eq!(l.display_at(LogPoint::new(10.0, -10.0)).unwrap().id, DisplayId(3));
    // A gap: right of the Retina panel, below the top monitor. No display there.
    assert!(l.display_at(LogPoint::new(1500.0, 500.0)).is_none());
}

#[test]
fn a_rect_is_half_open_so_touching_displays_do_not_both_claim_a_point() {
    let l = mixed_layout();
    // x = 0 belongs to display 1; the monitor to the left ends exactly there.
    assert_eq!(l.display_at(LogPoint::new(0.0, 5.0)).unwrap().id, DisplayId(1));
    assert_eq!(l.display_at(LogPoint::new(-0.5, 5.0)).unwrap().id, DisplayId(2));
}

// ---------------------------------------------------------------------------
// 6. Spanning displays at different scales — the real mixed-DPI case.
// ---------------------------------------------------------------------------

#[test]
fn output_scale_prefers_the_sharpest_display_touched() {
    let l = mixed_layout();
    // Straddles the 1x monitor and the 2x laptop -> compose at 2x, not 1x.
    let spanning = LogRect::new(-100.0, 100.0, 300.0, 200.0);
    assert_eq!(l.preferred_output_scale(spanning).unwrap().get(), 2.0);
    // Entirely on the 1x monitor -> 1x.
    let only_left = LogRect::new(-500.0, 100.0, 100.0, 100.0);
    assert_eq!(l.preferred_output_scale(only_left).unwrap().get(), 1.0);
    // Touching nothing -> no answer at all.
    assert!(l.preferred_output_scale(LogRect::new(5000.0, 5000.0, 10.0, 10.0)).is_none());
}

#[test]
fn region_spanning_two_scales_reads_each_at_its_own_scale() {
    let l = mixed_layout();
    // 100pt on the 1x monitor, 200pt on the 2x laptop, 50pt tall, starting 10pt down.
    let region = LogRect::new(-100.0, 10.0, 300.0, 50.0);
    let out = l.preferred_output_scale(region).unwrap();
    let pieces = l.split_region(region, out);
    assert_eq!(pieces.len(), 2, "should touch exactly two displays, got {pieces:?}");

    let left = pieces.iter().find(|p| p.display == DisplayId(2)).unwrap();
    let right = pieces.iter().find(|p| p.display == DisplayId(1)).unwrap();

    // Read from the 1x monitor at 1x, from its own top-left: x starts 1820px in, 100px wide.
    assert_eq!(left.source, PhysRect::new(1820, 10, 100, 50));
    // Read from the Retina panel at 2x, from ITS top-left: x starts at 0, 400px wide.
    assert_eq!(right.source, PhysRect::new(0, 20, 400, 100));

    // Both are written into one 2x output: 600x100 total, the 1x part upscaled into 200px.
    assert_eq!(left.dest, PhysRect::new(0, 0, 200, 100));
    assert_eq!(right.dest, PhysRect::new(200, 0, 400, 100));
}

#[test]
fn pieces_tile_the_output_without_gaps_or_overlap() {
    let l = mixed_layout();
    let region = LogRect::new(-100.0, 10.0, 300.0, 50.0);
    let out = l.preferred_output_scale(region).unwrap();
    let pieces = l.split_region(region, out);

    let total: u64 = pieces.iter().map(|p| p.dest.area()).sum();
    let whole = region.to_physical(out);
    assert_eq!(total, whole.area(), "destination pieces must exactly cover the output");

    for (i, a) in pieces.iter().enumerate() {
        for b in &pieces[i + 1..] {
            assert!(a.dest.intersection(&b.dest).is_none(), "overlapping dests: {a:?} {b:?}");
        }
    }
}

#[test]
fn source_never_exceeds_the_displays_real_pixel_buffer() {
    // A display whose logical bounds imply MORE pixels than the OS actually reports --
    // the rounding mismatch that would otherwise read past the end of the capture buffer.
    let l = DisplayLayout::new(vec![Display {
        id: DisplayId(1),
        bounds: LogRect::new(0.0, 0.0, 1000.0, 1000.0),
        physical: PhysSize { w: 1990, h: 1990 }, // not 2000 -- deliberately short
        scale: sc(2.0),
    }]);
    let pieces = l.split_region(LogRect::new(0.0, 0.0, 1000.0, 1000.0), sc(2.0));
    let p = pieces[0];
    assert!(p.source.max_x() <= 1990, "source {:?} reads past the buffer", p.source);
    assert!(p.source.max_y() <= 1990, "source {:?} reads past the buffer", p.source);
}

#[test]
fn region_off_screen_or_empty_yields_no_work() {
    let l = mixed_layout();
    assert!(l.split_region(LogRect::new(5000.0, 5000.0, 100.0, 100.0), sc(2.0)).is_empty());
    assert!(l.split_region(LogRect::new(0.0, 0.0, 0.0, 0.0), sc(2.0)).is_empty());
    assert!(l.split_region(LogRect::new(0.0, 0.0, -10.0, 50.0), sc(2.0)).is_empty());
}

#[test]
fn split_order_is_stable() {
    let l = mixed_layout();
    let region = LogRect::new(-100.0, 10.0, 300.0, 50.0);
    let a = l.split_region(region, sc(2.0));
    let b = l.split_region(region, sc(2.0));
    assert_eq!(a, b);
    assert!(a.windows(2).all(|w| w[0].display <= w[1].display), "pieces must be ordered by id");
}
