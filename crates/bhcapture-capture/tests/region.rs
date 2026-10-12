//! End-to-end tests for the provided `capture_region`, driven by a fake backend.
//!
//! This is the composition path a real backend never has to implement, so it is the one place
//! where a mixed-DPI bug would live. Synthetic displays carry known solid colours, so the
//! composed output can be checked pixel by pixel with no screen attached.

use std::cell::RefCell;
use std::collections::HashMap;

use bhcapture_capture::*;
use bhcapture_core::geom::*;
use bhcapture_core::image::Frame;

fn sc(v: f64) -> Scale { Scale::exact(v) }
const BLUE: [u8; 4] = [255, 0, 0, 255];   // BGRA
const RED:  [u8; 4] = [0, 0, 255, 255];

struct Fake {
    targets: Targets,
    pixels: HashMap<DisplayId, [u8; 4]>,
    fail_with: Option<fn() -> CaptureError>,
    calls: RefCell<Vec<DisplayId>>,
}

impl Capturer for Fake {
    fn list_targets(&self) -> Result<Targets, CaptureError> {
        if let Some(f) = self.fail_with { return Err(f()); }
        Ok(self.targets.clone())
    }
    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError> {
        self.calls.borrow_mut().push(id);
        let d = self.targets.layout.get(id).ok_or(CaptureError::DisplayNotFound(id))?;
        let px = *self.pixels.get(&id).unwrap();
        let mut f = Frame::transparent(d.physical.w, d.physical.h, d.scale).unwrap();
        for y in 0..d.physical.h { for x in 0..d.physical.w { f.set_pixel(x, y, px); } }
        Ok(f)
    }
    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError> {
        Err(CaptureError::WindowNotFound(id))
    }
}

/// 100x100pt Retina panel at the origin (RED), 100x100pt 1x monitor to its left (BLUE).
fn fake() -> Fake {
    let d1 = Display { id: DisplayId(1), bounds: LogRect::new(0.0, 0.0, 100.0, 100.0),
                       physical: PhysSize { w: 200, h: 200 }, scale: sc(2.0) };
    let d2 = Display { id: DisplayId(2), bounds: LogRect::new(-100.0, 0.0, 100.0, 100.0),
                       physical: PhysSize { w: 100, h: 100 }, scale: sc(1.0) };
    Fake {
        targets: Targets { layout: DisplayLayout::new(vec![d1, d2]), windows: vec![] },
        pixels: HashMap::from([(DisplayId(1), RED), (DisplayId(2), BLUE)]),
        fail_with: None,
        calls: RefCell::new(Vec::new()),
    }
}

#[test]
fn section0_rig_selfcheck() {
    let f = fake();
    let frame = f.capture_display(DisplayId(1)).unwrap();
    assert_eq!((frame.width(), frame.height()), (200, 200), "Retina panel in physical pixels");
    assert_eq!(frame.pixel(0, 0), Some(RED));
    let frame2 = f.capture_display(DisplayId(2)).unwrap();
    assert_eq!((frame2.width(), frame2.height()), (100, 100));
    assert_eq!(frame2.pixel(0, 0), Some(BLUE));
}

#[test]
fn region_on_one_display_is_an_exact_crop() {
    let f = fake();
    let out = f.capture_region(LogRect::new(10.0, 10.0, 50.0, 25.0)).unwrap();
    // 50x25 logical at 2x -> 100x50 physical.
    assert_eq!((out.width(), out.height()), (100, 50));
    assert_eq!(out.scale().get(), 2.0);
    for y in 0..50 { for x in 0..100 {
        assert_eq!(out.pixel(x, y), Some(RED), "pixel ({x},{y})");
    } }
    assert_eq!(*f.calls.borrow(), vec![DisplayId(1)], "must not read the untouched display");
}

#[test]
fn region_spanning_two_scales_composes_with_the_seam_in_the_right_place() {
    let f = fake();
    // 50pt on the 1x monitor, 50pt on the 2x panel.
    let out = f.capture_region(LogRect::new(-50.0, 0.0, 100.0, 50.0)).unwrap();
    assert_eq!((out.width(), out.height()), (200, 100), "composed at 2x, the sharpest touched");

    for y in [0, 25, 49] {
        for x in [0, 50, 99] {
            assert_eq!(out.pixel(x, y), Some(BLUE), "left half should be the 1x monitor ({x},{y})");
        }
        for x in [100, 150, 199] {
            assert_eq!(out.pixel(x, y), Some(RED), "right half should be the 2x panel ({x},{y})");
        }
    }
    // Every pixel written: a gap would leave transparent canvas showing through.
    for y in 0..100 { for x in 0..200 {
        assert_ne!(out.pixel(x, y).unwrap()[3], 0, "unwritten pixel at ({x},{y})");
    } }
    let mut calls = f.calls.borrow().clone();
    calls.sort();
    assert_eq!(calls, vec![DisplayId(1), DisplayId(2)], "each display read exactly once");
}

#[test]
fn output_scale_is_the_highest_touched_not_the_lowest() {
    let f = fake();

    // Entirely on the 1x monitor -> composed at 1x. (On its own this case cannot tell
    // "highest" from "lowest", since both are 1.0 -- hence the spanning case below.)
    let out = f.capture_region(LogRect::new(-80.0, 0.0, 40.0, 20.0)).unwrap();
    assert_eq!((out.width(), out.height()), (40, 20));
    assert_eq!(out.scale().get(), 1.0);

    // Spanning 1x and 2x: this is the case that discriminates. Choosing the lowest scale
    // would give 1x and a 60x20 output, silently halving the Retina half's detail.
    let span = f.capture_region(LogRect::new(-20.0, 0.0, 60.0, 20.0)).unwrap();
    assert_eq!(span.scale().get(), 2.0, "must compose at the SHARPEST scale touched");
    assert_eq!((span.width(), span.height()), (120, 40));
}

#[test]
fn degenerate_and_offscreen_regions_are_distinguishable_errors() {
    let f = fake();
    assert!(matches!(f.capture_region(LogRect::new(0.0, 0.0, 0.0, 10.0)), Err(CaptureError::RegionEmpty)));
    assert!(matches!(f.capture_region(LogRect::new(0.0, 0.0, -5.0, 10.0)), Err(CaptureError::RegionEmpty)));
    assert!(matches!(f.capture_region(LogRect::new(9000.0, 9000.0, 10.0, 10.0)), Err(CaptureError::RegionOffScreen)));
    assert!(f.calls.borrow().is_empty(), "no display should be read for a region with no work");
}

#[test]
fn permission_denied_propagates_unchanged_and_is_not_retryable() {
    let mut f = fake();
    f.fail_with = Some(|| CaptureError::PermissionDenied);
    let err = f.capture_region(LogRect::new(0.0, 0.0, 10.0, 10.0)).unwrap_err();
    assert!(matches!(err, CaptureError::PermissionDenied),
        "must not be flattened into a generic backend error -- the UI has to tell the user to relaunch");
    // Retrying cannot help: a running process never sees a new Screen Recording grant (Phase 0a).
    assert!(!err.is_retryable());
    assert!(CaptureError::Backend("transient".into()).is_retryable());
}
