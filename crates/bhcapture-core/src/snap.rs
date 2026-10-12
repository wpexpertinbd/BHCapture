//! Snap-to-window: which window is under the pointer?
//!
//! Pure logic over data the shell supplies, so it can be tested without a window server.
//!
//! The answer is the **topmost** normal window containing the point, clipped to the display the
//! point is on. "Topmost" needs real front-to-back order: picking the first match from an
//! unordered list highlights a window that is actually hidden behind another one.

use crate::geom::{DisplayLayout, LogPoint, LogRect};

/// A window the user might want to snap to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub bounds: LogRect,
    /// OS window layer. `0` is an ordinary application window; the menu bar, Dock, tooltips,
    /// screen-saver and our own overlays sit on other layers and must not be offered.
    pub layer: i32,
    /// Front-to-back position among on-screen windows. **Lower is nearer the front.**
    /// `None` means the OS does not list it as on screen (minimised, other Space, hidden).
    pub z: Option<u32>,
}

/// Windows smaller than this on either side are almost always chrome — a hairline, a drag handle,
/// an invisible helper window — and snapping to one is never what the user meant.
pub const MIN_SNAP_SIDE: f64 = 24.0;

/// The rectangle to highlight for `p`, or `None` when nothing sensible is there.
pub fn snap_window(cands: &[Candidate], layout: &DisplayLayout, p: LogPoint) -> Option<LogRect> {
    let display = layout.display_at(p)?;

    let hit = cands.iter()
        .filter(|c| c.layer == 0)
        .filter(|c| c.z.is_some())
        .filter(|c| c.bounds.size.w >= MIN_SNAP_SIDE && c.bounds.size.h >= MIN_SNAP_SIDE)
        .filter(|c| c.bounds.contains(p))
        // Lowest z = frontmost. `min_by_key` keeps the first of equals, so ties are stable.
        .min_by_key(|c| c.z.unwrap())?;

    // A window dragged half off the edge would otherwise produce a selection reaching past the
    // display, i.e. into pixels that do not exist.
    hit.bounds.intersection(&display.bounds)
}
