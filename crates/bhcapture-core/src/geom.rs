//! Coordinate math for multi-monitor, mixed-DPI capture.
//!
//! # Why two coordinate spaces exist, as separate types
//!
//! The overlay draws in **logical points** — what the window system and the webview use, where a
//! Retina display 2704 pixels wide is 1352 points wide. The capture backend works in **physical
//! device pixels** — what ScreenCaptureKit and Windows.Graphics.Capture actually hand back.
//!
//! On a single-scale machine the two differ by a constant and mixing them up produces a bug you
//! will not notice. Attach a 1x monitor to a 2x laptop and the same mistake silently halves or
//! doubles a selection. PLAN.md calls this out as where every screenshot tool breaks, so the two
//! spaces are **different Rust types** here: there is no arithmetic that accepts both, and every
//! conversion has to name a [`Scale`]. Mixing them is a compile error, not a runtime surprise.
//!
//! Physical coordinates are integers because a pixel is not divisible. Logical coordinates are
//! `f64` because window systems genuinely place windows at fractional points.

/// Device pixels per logical point. Always finite and strictly positive.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Scale(f64);

impl Scale {
    /// Rejects zero, negative, NaN and infinity — every one of which silently produces
    /// nonsense rectangles rather than an error further down.
    pub fn new(v: f64) -> Option<Self> {
        if v.is_finite() && v > 0.0 { Some(Scale(v)) } else { None }
    }
    /// For the common literals that are known-good at the call site.
    pub fn exact(v: f64) -> Self {
        Self::new(v).expect("scale must be finite and > 0")
    }
    #[inline]
    pub fn get(self) -> f64 { self.0 }
}

// ---------------------------------------------------------------------------
// Logical points — the space the overlay and the window system use.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct LogPoint { pub x: f64, pub y: f64 }

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct LogSize { pub w: f64, pub h: f64 }

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct LogRect { pub origin: LogPoint, pub size: LogSize }

impl LogPoint {
    pub fn new(x: f64, y: f64) -> Self { Self { x, y } }
}

impl LogRect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { origin: LogPoint::new(x, y), size: LogSize { w, h } }
    }

    /// Build from two drag corners in any order. A drag up-and-left is just as valid as
    /// down-and-right, and every selector has to cope with all four directions.
    pub fn from_corners(a: LogPoint, b: LogPoint) -> Self {
        let (x0, x1) = if a.x <= b.x { (a.x, b.x) } else { (b.x, a.x) };
        let (y0, y1) = if a.y <= b.y { (a.y, b.y) } else { (b.y, a.y) };
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }

    #[inline] pub fn min_x(&self) -> f64 { self.origin.x }
    #[inline] pub fn min_y(&self) -> f64 { self.origin.y }
    #[inline] pub fn max_x(&self) -> f64 { self.origin.x + self.size.w }
    #[inline] pub fn max_y(&self) -> f64 { self.origin.y + self.size.h }
    #[inline] pub fn is_empty(&self) -> bool { self.size.w <= 0.0 || self.size.h <= 0.0 }

    pub fn contains(&self, p: LogPoint) -> bool {
        p.x >= self.min_x() && p.x < self.max_x() && p.y >= self.min_y() && p.y < self.max_y()
    }

    /// Overlap of two rectangles, or `None` when they do not overlap.
    pub fn intersection(&self, other: &LogRect) -> Option<LogRect> {
        let x0 = self.min_x().max(other.min_x());
        let y0 = self.min_y().max(other.min_y());
        let x1 = self.max_x().min(other.max_x());
        let y1 = self.max_y().min(other.max_y());
        if x1 > x0 && y1 > y0 { Some(LogRect::new(x0, y0, x1 - x0, y1 - y0)) } else { None }
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, other: &LogRect) -> LogRect {
        let x0 = self.min_x().min(other.min_x());
        let y0 = self.min_y().min(other.min_y());
        let x1 = self.max_x().max(other.max_x());
        let y1 = self.max_y().max(other.max_y());
        LogRect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn translated(&self, dx: f64, dy: f64) -> LogRect {
        LogRect::new(self.origin.x + dx, self.origin.y + dy, self.size.w, self.size.h)
    }

    /// Convert to device pixels.
    ///
    /// **Both edges are scaled and then rounded — the width is derived afterwards.** Rounding the
    /// origin and the size independently is the classic off-by-one here: it lets two rectangles
    /// that share an edge in logical space end up overlapping or leaving a one-pixel seam in
    /// physical space. See `rounding_is_edge_consistent` in the tests.
    pub fn to_physical(&self, s: Scale) -> PhysRect {
        let k = s.get();
        let x0 = (self.min_x() * k).round() as i32;
        let y0 = (self.min_y() * k).round() as i32;
        let x1 = (self.max_x() * k).round() as i32;
        let y1 = (self.max_y() * k).round() as i32;
        PhysRect::from_edges(x0, y0, x1, y1)
    }
}

// ---------------------------------------------------------------------------
// Physical pixels — the space the capture backends work in.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PhysPoint { pub x: i32, pub y: i32 }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PhysSize { pub w: u32, pub h: u32 }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PhysRect { pub origin: PhysPoint, pub size: PhysSize }

impl PhysRect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { origin: PhysPoint { x, y }, size: PhysSize { w, h } }
    }

    /// Clamps a negative extent to zero rather than panicking on an `as u32` cast, which would
    /// otherwise turn an inverted rectangle into a ~4-billion-pixel one.
    pub fn from_edges(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
    }

    #[inline] pub fn min_x(&self) -> i32 { self.origin.x }
    #[inline] pub fn min_y(&self) -> i32 { self.origin.y }
    #[inline] pub fn max_x(&self) -> i32 { self.origin.x + self.size.w as i32 }
    #[inline] pub fn max_y(&self) -> i32 { self.origin.y + self.size.h as i32 }
    #[inline] pub fn is_empty(&self) -> bool { self.size.w == 0 || self.size.h == 0 }
    #[inline] pub fn area(&self) -> u64 { self.size.w as u64 * self.size.h as u64 }

    pub fn intersection(&self, other: &PhysRect) -> Option<PhysRect> {
        let x0 = self.min_x().max(other.min_x());
        let y0 = self.min_y().max(other.min_y());
        let x1 = self.max_x().min(other.max_x());
        let y1 = self.max_y().min(other.max_y());
        if x1 > x0 && y1 > y0 { Some(PhysRect::from_edges(x0, y0, x1, y1)) } else { None }
    }

    pub fn to_logical(&self, s: Scale) -> LogRect {
        let k = s.get();
        LogRect::new(
            self.min_x() as f64 / k, self.min_y() as f64 / k,
            self.size.w as f64 / k, self.size.h as f64 / k,
        )
    }
}

// ---------------------------------------------------------------------------
// Displays
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct DisplayId(pub u32);

/// One display as the shell discovered it.
///
/// `bounds` places the display on the global logical desktop. On Windows a monitor left of or
/// above the primary has a **negative** origin, and on macOS a display above the primary does too
/// — so nothing here may assume the desktop starts at (0, 0).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Display {
    pub id: DisplayId,
    pub bounds: LogRect,
    /// Native pixel size, straight from the OS. Not necessarily `bounds * scale` exactly —
    /// the OS rounds — so this is authoritative for clamping.
    pub physical: PhysSize,
    pub scale: Scale,
}

impl Display {
    /// This display's own capture space: origin (0,0) at its top-left, size in native pixels.
    /// Capture backends hand back a buffer in exactly this space.
    pub fn capture_space(&self) -> PhysRect {
        PhysRect::new(0, 0, self.physical.w, self.physical.h)
    }
}

/// One display's share of a region that may span several displays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegionPiece {
    pub display: DisplayId,
    /// Where to read, in **that display's own** physical pixels (origin at its top-left).
    pub source: PhysRect,
    /// Where it belongs in the composed output, in **output** physical pixels
    /// (origin at the region's top-left).
    pub dest: PhysRect,
}

#[derive(Clone, Debug, Default)]
pub struct DisplayLayout {
    displays: Vec<Display>,
}

impl DisplayLayout {
    pub fn new(displays: Vec<Display>) -> Self { Self { displays } }
    pub fn displays(&self) -> &[Display] { &self.displays }
    pub fn get(&self, id: DisplayId) -> Option<&Display> {
        self.displays.iter().find(|d| d.id == id)
    }

    /// Bounding box of the whole desktop, or `None` when there are no displays.
    pub fn virtual_bounds(&self) -> Option<LogRect> {
        let mut it = self.displays.iter();
        let first = it.next()?.bounds;
        Some(it.fold(first, |acc, d| acc.union(&d.bounds)))
    }

    /// Which display contains a point. `None` for a point in a gap between displays.
    pub fn display_at(&self, p: LogPoint) -> Option<&Display> {
        self.displays.iter().find(|d| d.bounds.contains(p))
    }

    /// The scale a region should be composed at: the **highest** among the displays it touches,
    /// so that spanning onto a 1x monitor never throws away the Retina half's detail.
    pub fn preferred_output_scale(&self, region: LogRect) -> Option<Scale> {
        self.displays.iter()
            .filter(|d| region.intersection(&d.bounds).is_some())
            .map(|d| d.scale)
            .max_by(|a, b| a.get().partial_cmp(&b.get()).unwrap())
    }

    /// Split a logical region into per-display read instructions.
    ///
    /// This is the whole mixed-DPI problem in one function: each display is read **at its own
    /// scale** from its own capture buffer, and the pieces are placed into one output image at a
    /// single chosen scale. Returned in a stable order (by display id) so output is reproducible.
    pub fn split_region(&self, region: LogRect, out: Scale) -> Vec<RegionPiece> {
        if region.is_empty() { return Vec::new(); }
        let mut pieces: Vec<RegionPiece> = self.displays.iter().filter_map(|d| {
            let overlap = region.intersection(&d.bounds)?;

            // Read coordinates are relative to the display's own top-left, at its own scale...
            let local = overlap.translated(-d.bounds.min_x(), -d.bounds.min_y());
            let source = local.to_physical(d.scale).intersection(&d.capture_space())?;

            // ...while write coordinates are relative to the region's top-left, at the output scale.
            let dest = overlap.translated(-region.min_x(), -region.min_y()).to_physical(out);
            if source.is_empty() || dest.is_empty() { return None; }
            Some(RegionPiece { display: d.id, source, dest })
        }).collect();
        pieces.sort_by_key(|p| p.display);
        pieces
    }
}
