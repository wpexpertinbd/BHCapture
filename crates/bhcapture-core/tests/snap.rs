use bhcapture_core::geom::*;
use bhcapture_core::snap::*;

fn sc(v: f64) -> Scale { Scale::exact(v) }

fn layout() -> DisplayLayout {
    DisplayLayout::new(vec![
        Display { id: DisplayId(1), bounds: LogRect::new(0.0, 0.0, 1000.0, 800.0),
                  physical: PhysSize { w: 2000, h: 1600 }, scale: sc(2.0) },
        Display { id: DisplayId(2), bounds: LogRect::new(-1000.0, 0.0, 1000.0, 800.0),
                  physical: PhysSize { w: 1000, h: 800 }, scale: sc(1.0) },
    ])
}

fn win(x: f64, y: f64, w: f64, h: f64, z: Option<u32>) -> Candidate {
    Candidate { bounds: LogRect::new(x, y, w, h), layer: 0, z }
}
fn at(x: f64, y: f64) -> LogPoint { LogPoint::new(x, y) }

#[test]
fn section0_rig_selfcheck() {
    let c = [win(100.0, 100.0, 300.0, 200.0, Some(0))];
    assert_eq!(snap_window(&c, &layout(), at(150.0, 150.0)), Some(LogRect::new(100.0, 100.0, 300.0, 200.0)));
    assert_eq!(snap_window(&c, &layout(), at(50.0, 50.0)), None, "outside every window");
}

#[test]
fn the_frontmost_overlapping_window_wins_regardless_of_list_order() {
    // Same two windows, listed both ways round: the answer must follow z, not list position.
    let back = win(0.0, 0.0, 500.0, 500.0, Some(5));
    let front = win(100.0, 100.0, 200.0, 200.0, Some(1));
    for list in [[back, front], [front, back]] {
        assert_eq!(snap_window(&list, &layout(), at(150.0, 150.0)), Some(front.bounds),
            "a hidden window was highlighted instead of the one on top");
    }
    // Outside the front window, the back one is what is actually visible there.
    assert_eq!(snap_window(&[back, front], &layout(), at(450.0, 450.0)), Some(back.bounds));
}

#[test]
fn only_ordinary_windows_are_offered() {
    let menubar = Candidate { bounds: LogRect::new(0.0, 0.0, 1000.0, 30.0), layer: 24, z: Some(0) };
    let dock    = Candidate { bounds: LogRect::new(0.0, 700.0, 1000.0, 100.0), layer: 20, z: Some(0) };
    let overlay = Candidate { bounds: LogRect::new(0.0, 0.0, 1000.0, 800.0), layer: 3, z: Some(0) };
    let desktop = Candidate { bounds: LogRect::new(0.0, 0.0, 1000.0, 800.0), layer: -2147483623, z: Some(9) };
    for c in [menubar, dock, overlay, desktop] {
        assert_eq!(snap_window(&[c], &layout(), at(500.0, 15.0)), None, "offered layer {}", c.layer);
        assert_eq!(snap_window(&[c], &layout(), at(500.0, 750.0)), None, "offered layer {}", c.layer);
    }
}

#[test]
fn windows_the_os_does_not_list_as_on_screen_are_ignored() {
    let minimised = win(0.0, 0.0, 500.0, 500.0, None);
    assert_eq!(snap_window(&[minimised], &layout(), at(100.0, 100.0)), None);
    // And it must not shadow a visible window behind its bounds.
    let visible = win(0.0, 0.0, 500.0, 500.0, Some(3));
    assert_eq!(snap_window(&[minimised, visible], &layout(), at(100.0, 100.0)), Some(visible.bounds));
}

#[test]
fn tiny_windows_are_not_snap_targets() {
    let hairline = win(100.0, 100.0, 400.0, 2.0, Some(0));
    let helper = win(100.0, 100.0, 10.0, 10.0, Some(0));
    let real = win(50.0, 50.0, 600.0, 400.0, Some(4));
    // The hairline is nearer the front but must not win.
    assert_eq!(snap_window(&[hairline, helper, real], &layout(), at(105.0, 101.0)), Some(real.bounds));
    assert_eq!(snap_window(&[hairline], &layout(), at(105.0, 101.0)), None);
}

#[test]
fn a_window_hanging_off_the_display_is_clipped_to_it() {
    let w = win(800.0, 600.0, 600.0, 600.0, Some(0)); // extends well past 1000x800
    let got = snap_window(&[w], &layout(), at(900.0, 700.0)).unwrap();
    assert_eq!(got, LogRect::new(800.0, 600.0, 200.0, 200.0));
    assert!(got.max_x() <= 1000.0 && got.max_y() <= 800.0, "selection reaches past the display");
}

#[test]
fn works_on_a_display_with_a_negative_origin() {
    let w = win(-900.0, 100.0, 500.0, 400.0, Some(0));
    assert_eq!(snap_window(&[w], &layout(), at(-800.0, 200.0)), Some(w.bounds));
}

#[test]
fn a_point_in_no_display_snaps_to_nothing() {
    let w = win(0.0, 0.0, 5000.0, 5000.0, Some(0));
    assert_eq!(snap_window(&[w], &layout(), at(3000.0, 3000.0)), None);
}

#[test]
fn equal_z_is_stable() {
    let a = win(0.0, 0.0, 400.0, 400.0, Some(2));
    let b = win(0.0, 0.0, 300.0, 300.0, Some(2));
    assert_eq!(snap_window(&[a, b], &layout(), at(10.0, 10.0)), Some(a.bounds));
    assert_eq!(snap_window(&[a, b], &layout(), at(10.0, 10.0)), Some(a.bounds));
}
