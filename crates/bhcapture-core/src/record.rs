//! Geometry for screen recording. Pure, so it is tested without a screen.

use crate::geom::{DisplayId, DisplayLayout, LogPoint, LogRect, LogSize};

/// Smallest region worth recording, in points per side. Anything smaller is almost certainly a
/// mis-click, and the encoder needs at least 2 pixels each way anyway.
pub const MIN_RECORD_SIDE: f64 = 8.0;

/// Where a recording of `region` comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RecordTarget {
    pub display: DisplayId,
    /// The part of the region on that display, GLOBAL logical points, snapped to whole points.
    pub global: LogRect,
    /// The same rectangle relative to the display's top-left, which is what the capture API takes.
    pub local: LogRect,
}

/// A recording is taken from ONE display. A selection straddling two is recorded from the display
/// holding most of it, clipped to that display -- the alternative, composing two live streams at two
/// scales, is not worth its cost for v1. `None` if the region is off every screen or too small.
pub fn record_target(layout: &DisplayLayout, region: LogRect) -> Option<RecordTarget> {
    let mut best: Option<(f64, DisplayId, LogRect, LogRect)> = None;
    for d in layout.displays() {
        if let Some(i) = region.intersection(&d.bounds) {
            let area = i.size.w * i.size.h;
            if best.map_or(true, |b| area > b.0) {
                best = Some((area, d.id, i, d.bounds));
            }
        }
    }
    let (_, display, i, b) = best?;
    // Whole points, never growing past the display: floor the start, floor the end.
    let x0 = i.min_x().floor().max(b.min_x());
    let y0 = i.min_y().floor().max(b.min_y());
    let x1 = i.max_x().round().min(b.max_x());
    let y1 = i.max_y().round().min(b.max_y());
    let (w, h) = (x1 - x0, y1 - y0);
    if w < MIN_RECORD_SIDE || h < MIN_RECORD_SIDE { return None; }
    Some(RecordTarget {
        display,
        global: LogRect::new(x0, y0, w, h),
        local: LogRect::new(x0 - b.min_x(), y0 - b.min_y(), w, h),
    })
}

/// Top-left for the control bar: centred under the region, else above it, else inside its bottom
/// edge -- and always fully on the display, clear of the menu bar. The bar is left out of the
/// recording by the capture filter, so overlapping the region is harmless; it is only a last resort
/// because it hides what the user is recording.
pub fn place_bar(display: LogRect, region: LogRect, bar: LogSize) -> LogPoint {
    const GAP: f64 = 12.0;
    const EDGE: f64 = 8.0;
    const MENU_BAR: f64 = 30.0;
    let cx = region.min_x() + region.size.w / 2.0 - bar.w / 2.0;
    let max_x = (display.max_x() - bar.w - EDGE).max(display.min_x() + EDGE);
    let x = cx.clamp(display.min_x() + EDGE, max_x);

    let below = region.max_y() + GAP;
    let above = region.min_y() - GAP - bar.h;
    let y = if below + bar.h <= display.max_y() - EDGE {
        below
    } else if above >= display.min_y() + MENU_BAR {
        above
    } else {
        (region.max_y() - GAP - bar.h).clamp(display.min_y() + MENU_BAR, (display.max_y() - bar.h - EDGE).max(display.min_y()))
    };
    LogPoint::new(x, y)
}
