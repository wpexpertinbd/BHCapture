use std::cell::RefCell;
use bhcapture_capture::*;
use bhcapture_core::geom::*;
use bhcapture_core::image::Frame;

fn sc(v: f64) -> Scale { Scale::exact(v) }

/// A source whose pixel at (x,y) encodes its own coordinates, so a crop can be checked exactly:
/// B = x%256, G = y%256, R = display id.
struct Coords { layout: DisplayLayout, calls: RefCell<u32> }
impl Capturer for Coords {
    fn list_displays(&self) -> Result<DisplayLayout, CaptureError> { Ok(self.layout.clone()) }
    fn list_targets(&self) -> Result<Targets, CaptureError> { unreachable!() }
    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError> {
        *self.calls.borrow_mut() += 1;
        let d = self.layout.get(id).ok_or(CaptureError::DisplayNotFound(id))?;
        let mut f = Frame::transparent(d.physical.w, d.physical.h, d.scale).unwrap();
        for y in 0..d.physical.h { for x in 0..d.physical.w {
            f.set_pixel(x, y, [(x % 256) as u8, (y % 256) as u8, id.0 as u8, 255]);
        } }
        Ok(f)
    }
    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError> { Err(CaptureError::WindowNotFound(id)) }
}

fn rig() -> (Coords, Snapshot) {
    let layout = DisplayLayout::new(vec![
        Display { id: DisplayId(1), bounds: LogRect::new(0.0, 0.0, 100.0, 100.0),
                  physical: PhysSize { w: 200, h: 200 }, scale: sc(2.0) },
        Display { id: DisplayId(2), bounds: LogRect::new(-100.0, 0.0, 100.0, 100.0),
                  physical: PhysSize { w: 100, h: 100 }, scale: sc(1.0) },
    ]);
    let src = Coords { layout: layout.clone(), calls: RefCell::new(0) };
    let snap = Snapshot::take(&src, &layout).unwrap();
    (src, snap)
}

#[test]
fn section0_rig_selfcheck() {
    let (_, snap) = rig();
    let f = snap.capture_display(DisplayId(1)).unwrap();
    assert_eq!(f.pixel(7, 9), Some([7, 9, 1, 255]));
}

#[test]
fn a_crop_from_the_snapshot_never_touches_the_live_source_again() {
    let (src, snap) = rig();
    assert_eq!(*src.calls.borrow(), 2, "one capture per display, taken once");
    let _ = snap.capture_region_in(LogRect::new(10.0, 10.0, 20.0, 20.0), snap.layout()).unwrap();
    let _ = snap.capture_region_in(LogRect::new(-50.0, 0.0, 80.0, 40.0), snap.layout()).unwrap();
    assert_eq!(*src.calls.borrow(), 2, "cropping must reuse the frozen frames, not recapture");
}

#[test]
fn a_snapshot_crop_equals_the_frame_the_user_saw() {
    let (_, snap) = rig();
    let crop = snap.capture_region_in(LogRect::new(10.0, 20.0, 30.0, 15.0), snap.layout()).unwrap();
    // 10pt,20pt at 2x = physical (20,40).
    assert_eq!((crop.width(), crop.height()), (60, 30));
    assert_eq!(crop.pixel(0, 0), Some([20, 40, 1, 255]));
    assert_eq!(crop.pixel(59, 29), Some([79, 69, 1, 255]));
}

#[test]
fn loupe_is_centred_on_the_pixel_under_the_pointer() {
    let (_, snap) = rig();
    // Logical (50.0, 30.0) on the 2x display = physical (100, 60).
    let l = snap.loupe(LogPoint::new(50.0, 30.0), 10).unwrap();
    assert_eq!((l.width(), l.height()), (21, 21));
    assert_eq!(l.pixel(10, 10), Some([100, 60, 1, 255]), "centre pixel is not the one under the pointer");
    assert_eq!(l.pixel(0, 0), Some([90, 50, 1, 255]));
    assert_eq!(l.pixel(20, 20), Some([110, 70, 1, 255]));
}

#[test]
fn loupe_uses_the_scale_of_the_display_under_the_pointer() {
    let (_, snap) = rig();
    // Display 2 is 1x: logical (-40, 25) is local (60, 25) physical.
    let l = snap.loupe(LogPoint::new(-40.0, 25.0), 5).unwrap();
    assert_eq!(l.pixel(5, 5), Some([60, 25, 2, 255]));
    assert_eq!(l.scale().get(), 1.0);
}

#[test]
fn loupe_at_a_display_edge_shows_a_real_edge_not_a_smear() {
    let (_, snap) = rig();
    let l = snap.loupe(LogPoint::new(0.0, 0.0), 4).unwrap(); // top-left corner of display 1
    assert_eq!(l.pixel(4, 4), Some([0, 0, 1, 255]), "centre must still be the corner pixel");
    assert_eq!(l.pixel(5, 5), Some([1, 1, 1, 255]), "pixels inside must be un-stretched");
    // Everything left of / above the display is transparent, not a repeated edge.
    assert_eq!(l.pixel(3, 4).unwrap()[3], 0, "outside the display must be transparent");
    assert_eq!(l.pixel(4, 3).unwrap()[3], 0);
    assert_eq!(l.pixel(0, 0).unwrap()[3], 0);
}

#[test]
fn loupe_in_a_gap_is_none() {
    let (_, snap) = rig();
    assert!(snap.loupe(LogPoint::new(500.0, 500.0), 5).is_none());
}

#[test]
fn a_snapshot_has_no_windows() {
    let (_, snap) = rig();
    assert!(matches!(snap.capture_window(WindowId(1)), Err(CaptureError::WindowNotFound(_))));
}

#[test]
fn loupe_picks_the_pixel_the_pointer_is_inside_not_the_nearest_one() {
    let (_, snap) = rig();
    // Logical 50.3 on a 2x display is physical 100.6: inside pixel 100 (which spans [100,101)),
    // though nearer to 101. Rounding would centre the loupe on a pixel the pointer is not over.
    // The earlier cases all used whole-pixel positions, where floor and round agree.
    let l = snap.loupe(LogPoint::new(50.3, 30.3), 3).unwrap();
    assert_eq!(l.pixel(3, 3), Some([100, 60, 1, 255]));
}
