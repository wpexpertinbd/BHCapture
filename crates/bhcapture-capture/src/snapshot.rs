//! A frozen copy of every display, taken when the selector opens.
//!
//! It exists for two reasons that point the same way:
//!
//! * the **magnifier** needs pixels under the cursor *now*, hundreds of times a second, and a live
//!   capture costs ~115 ms;
//! * the saved file should contain **exactly what the user saw while selecting**. Capturing again
//!   at Enter races whatever is changing on screen — a video, a clock, a notification — and the
//!   crop can differ from the frame they framed.
//!
//! `Snapshot` implements [`Capturer`], so the existing `capture_region_in` composes a crop from it
//! with the same mixed-DPI code a live capture uses.

use std::collections::HashMap;

use bhcapture_core::geom::{DisplayId, DisplayLayout, LogPoint, PhysRect};
use bhcapture_core::image::Frame;

use crate::{CaptureError, Capturer, Targets, WindowId};

pub struct Snapshot {
    layout: DisplayLayout,
    frames: HashMap<DisplayId, Frame>,
}

impl Snapshot {
    /// Capture every display in `layout` through `source`.
    ///
    /// All-or-nothing: a snapshot missing one display would crop a region spanning it from
    /// nothing, and the error would surface far from its cause.
    pub fn take(source: &dyn Capturer, layout: &DisplayLayout) -> Result<Self, CaptureError> {
        let mut frames = HashMap::new();
        for d in layout.displays() {
            frames.insert(d.id, source.capture_display(d.id)?);
        }
        Ok(Self { layout: layout.clone(), frames })
    }

    pub fn layout(&self) -> &DisplayLayout { &self.layout }

    /// A `(2*radius+1)`-pixel square of **physical** pixels centred on logical point `p`, for the
    /// magnifier. Pixels falling outside the display are left transparent rather than clamped, so
    /// the loupe shows a genuine edge instead of smearing the last row outward.
    pub fn loupe(&self, p: LogPoint, radius: u32) -> Option<Frame> {
        let d = self.layout.display_at(p)?;
        let src = self.frames.get(&d.id)?;

        let k = d.scale.get();
        let cx = ((p.x - d.bounds.min_x()) * k).floor() as i32;
        let cy = ((p.y - d.bounds.min_y()) * k).floor() as i32;
        let side = radius * 2 + 1;
        let want = PhysRect::new(cx - radius as i32, cy - radius as i32, side, side);

        let mut out = Frame::transparent(side, side, d.scale).ok()?;
        // Clip to the frame ourselves and place by offset. `blit` resamples when the clipped
        // source is smaller than the destination, which here would stretch the edge of the
        // display across the loupe instead of leaving the missing part blank.
        let have = want.intersection(&src.bounds())?;
        let dest = PhysRect::new(have.min_x() - want.min_x(), have.min_y() - want.min_y(),
                                 have.size.w, have.size.h);
        out.blit(dest, src, have);
        Some(out)
    }
}

impl Capturer for Snapshot {
    fn list_displays(&self) -> Result<DisplayLayout, CaptureError> { Ok(self.layout.clone()) }

    fn list_targets(&self) -> Result<Targets, CaptureError> {
        Ok(Targets { layout: self.layout.clone(), windows: Vec::new() })
    }

    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError> {
        self.frames.get(&id).cloned().ok_or(CaptureError::DisplayNotFound(id))
    }

    /// A snapshot holds displays, not windows.
    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError> {
        Err(CaptureError::WindowNotFound(id))
    }
}
