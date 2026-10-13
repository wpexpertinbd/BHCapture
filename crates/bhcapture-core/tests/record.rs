use bhcapture_core::geom::*;
use bhcapture_core::record::*;

fn layout() -> DisplayLayout {
    DisplayLayout::new(vec![
        Display { id: DisplayId(1), bounds: LogRect::new(0.0, 0.0, 1000.0, 800.0),
                  physical: PhysSize { w: 2000, h: 1600 }, scale: Scale::exact(2.0) },
        Display { id: DisplayId(2), bounds: LogRect::new(-1000.0, 100.0, 1000.0, 800.0),
                  physical: PhysSize { w: 1000, h: 800 }, scale: Scale::exact(1.0) },
    ])
}

#[test]
fn a_region_on_one_display_is_recorded_from_it_unchanged() {
    let t = record_target(&layout(), LogRect::new(100.0, 50.0, 400.0, 300.0)).unwrap();
    assert_eq!(t.display, DisplayId(1));
    assert_eq!(t.global, LogRect::new(100.0, 50.0, 400.0, 300.0));
    assert_eq!(t.local, t.global, "display 1 sits at the origin");
}

#[test]
fn local_coordinates_are_relative_to_the_display() {
    let t = record_target(&layout(), LogRect::new(-900.0, 200.0, 300.0, 200.0)).unwrap();
    assert_eq!(t.display, DisplayId(2));
    assert_eq!(t.local, LogRect::new(100.0, 100.0, 300.0, 200.0));
}

#[test]
fn a_straddling_region_goes_to_the_display_with_more_of_it_and_is_clipped() {
    // 100 pt on display 2, 300 pt on display 1.
    let t = record_target(&layout(), LogRect::new(-100.0, 200.0, 400.0, 100.0)).unwrap();
    assert_eq!(t.display, DisplayId(1));
    assert_eq!(t.global, LogRect::new(0.0, 200.0, 300.0, 100.0));
    // And the other way round.
    let t = record_target(&layout(), LogRect::new(-300.0, 200.0, 400.0, 100.0)).unwrap();
    assert_eq!(t.display, DisplayId(2));
    assert_eq!(t.global, LogRect::new(-300.0, 200.0, 300.0, 100.0));
}

#[test]
fn fractional_regions_snap_to_whole_points_inside_the_display() {
    let t = record_target(&layout(), LogRect::new(10.4, 20.6, 100.3, 50.2)).unwrap();
    assert_eq!(t.global.min_x(), 10.0);
    assert_eq!(t.global.min_y(), 20.0);
    assert!(t.global.max_x() <= 1000.0 && t.global.max_y() <= 800.0);
    // Whole numbers throughout.
    for v in [t.global.min_x(), t.global.min_y(), t.global.size.w, t.global.size.h] { assert_eq!(v, v.round()); }
}

#[test]
fn off_screen_or_tiny_regions_are_refused() {
    assert!(record_target(&layout(), LogRect::new(5000.0, 5000.0, 100.0, 100.0)).is_none());
    assert!(record_target(&layout(), LogRect::new(10.0, 10.0, 4.0, 300.0)).is_none());
}

#[test]
fn the_bar_goes_below_the_region_when_it_fits() {
    let d = LogRect::new(0.0, 0.0, 1000.0, 800.0);
    let p = place_bar(d, LogRect::new(300.0, 100.0, 400.0, 300.0), LogSize { w: 200.0, h: 50.0 });
    assert_eq!(p, LogPoint::new(400.0, 412.0));
}

#[test]
fn the_bar_goes_above_when_there_is_no_room_below() {
    let d = LogRect::new(0.0, 0.0, 1000.0, 800.0);
    let p = place_bar(d, LogRect::new(300.0, 400.0, 400.0, 380.0), LogSize { w: 200.0, h: 50.0 });
    assert_eq!(p.y, 400.0 - 12.0 - 50.0);
}

#[test]
fn a_full_screen_region_puts_the_bar_inside_and_on_screen() {
    let d = LogRect::new(0.0, 0.0, 1000.0, 800.0);
    let bar = LogSize { w: 600.0, h: 50.0 };
    let p = place_bar(d, d, bar);
    assert!(p.y >= 30.0 && p.y + bar.h <= 800.0, "{p:?}");
    assert!(p.x >= 0.0 && p.x + bar.w <= 1000.0, "{p:?}");
}

#[test]
fn the_bar_never_leaves_the_display_sideways() {
    let d = LogRect::new(-1000.0, 100.0, 1000.0, 800.0);
    let bar = LogSize { w: 600.0, h: 50.0 };
    let p = place_bar(d, LogRect::new(-1000.0, 150.0, 40.0, 40.0), bar);
    assert!(p.x >= -1000.0 && p.x + bar.w <= 0.0, "{p:?}");
    let p = place_bar(d, LogRect::new(-50.0, 150.0, 40.0, 40.0), bar);
    assert!(p.x >= -1000.0 && p.x + bar.w <= 0.0, "{p:?}");
}
