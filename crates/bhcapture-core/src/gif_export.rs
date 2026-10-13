//! Animated GIF from a recording's frames. Portable: the shell decodes the video (each OS its own
//! way) and pushes RGBA frames with their timestamps; this does the rest.
//!
//! A screen recording is mostly STILL, and that is where the size goes or is saved:
//! - each frame is compared with what the viewer is ALREADY SEEING, ignoring per-channel differences
//!   up to `NOISE` (decoded video never repeats a frame bit-for-bit -- with exact comparison a real
//!   12.8 s recording wrote 153 of 154 frames and came to 20 MB);
//! - no visible change: the frame on screen simply lasts longer;
//! - a change: only the RECTANGLE that changed is stored, drawn over the previous frame.
//! Delays come from the real timestamps (GIF counts in 1/100 s).

use std::io::Write;

/// Frame rate for exports: smooth enough for UI motion, small enough to share.
pub const DEFAULT_FPS: f64 = 12.0;
/// Longest side, in pixels.
pub const DEFAULT_MAX_SIDE: u32 = 960;
/// NeuQuant speed (1 = best, 30 = fastest). 10 is the crate's balanced default.
const QUANT_SPEED: i32 = 10;
/// Per-channel difference treated as the same colour. Video decoding noise is a few levels; anything
/// a viewer would notice is far above this.
pub const NOISE: u8 = 12;

#[derive(Debug)]
pub enum GifError { Io(String), Encode(String), BadFrame(String) }

impl std::fmt::Display for GifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Io(e) | Self::Encode(e) | Self::BadFrame(e) => f.write_str(e) }
    }
}

/// A changed region waiting to be written: its rectangle, its pixels, and when it appeared.
struct Pending { x: u16, y: u16, w: u16, h: u16, rgba: Vec<u8>, start: f64 }

pub struct GifWriter<W: Write> {
    enc: gif::Encoder<W>,
    w: u16,
    h: u16,
    /// What the viewer sees once every written (and the pending) frame is drawn.
    canvas: Option<Vec<u8>>,
    held: Option<Pending>,
    frames_in: u64,
    frames_out: u64,
}

impl<W: Write> GifWriter<W> {
    pub fn new(out: W, width: u32, height: u32) -> Result<Self, GifError> {
        let (w, h) = (u16::try_from(width).map_err(|_| GifError::BadFrame("too wide for GIF".into()))?,
                      u16::try_from(height).map_err(|_| GifError::BadFrame("too tall for GIF".into()))?);
        if w == 0 || h == 0 { return Err(GifError::BadFrame("empty frame".into())); }
        let mut enc = gif::Encoder::new(out, w, h, &[]).map_err(|e| GifError::Encode(e.to_string()))?;
        enc.set_repeat(gif::Repeat::Infinite).map_err(|e| GifError::Encode(e.to_string()))?;
        Ok(Self { enc, w, h, canvas: None, held: None, frames_in: 0, frames_out: 0 })
    }

    /// Add one RGBA frame shown from `at` seconds.
    pub fn push(&mut self, rgba: &[u8], at: f64) -> Result<(), GifError> {
        if rgba.len() != self.w as usize * self.h as usize * 4 {
            return Err(GifError::BadFrame(format!("frame is {} bytes, expected {}x{} RGBA", rgba.len(), self.w, self.h)));
        }
        self.frames_in += 1;
        let (w, h) = (self.w as usize, self.h as usize);
        let Some(canvas) = self.canvas.as_mut() else {
            self.canvas = Some(rgba.to_vec());
            self.held = Some(Pending { x: 0, y: 0, w: self.w, h: self.h, rgba: rgba.to_vec(), start: at });
            return Ok(());
        };
        // Bounding box of VISIBLE change against what is on screen.
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        for y in 0..h {
            let row = y * w * 4;
            for x in 0..w {
                let i = row + x * 4;
                let d = (0..3).map(|c| rgba[i + c].abs_diff(canvas[i + c])).max().unwrap_or(0);
                if d > NOISE {
                    x0 = x0.min(x); x1 = x1.max(x); y0 = y0.min(y); y1 = y1.max(y);
                }
            }
        }
        if x1 < x0 { return Ok(()); } // nothing visible changed: the frame on screen lasts longer
        let (rw, rh) = (x1 - x0 + 1, y1 - y0 + 1);
        let mut part = Vec::with_capacity(rw * rh * 4);
        for y in y0..=y1 {
            let a = (y * w + x0) * 4;
            part.extend_from_slice(&rgba[a..a + rw * 4]);
            canvas[a..a + rw * 4].copy_from_slice(&rgba[a..a + rw * 4]);
        }
        if let Some(prev) = self.held.take() { self.write(prev, at)?; }
        self.held = Some(Pending { x: x0 as u16, y: y0 as u16, w: rw as u16, h: rh as u16, rgba: part, start: at });
        Ok(())
    }

    /// Write the last frame, lasting until `end` seconds, and close the file.
    pub fn finish(mut self, end: f64) -> Result<(u64, u64), GifError> {
        if let Some(prev) = self.held.take() { let e = end.max(prev.start + 0.01); self.write(prev, e)?; }
        Ok((self.frames_in, self.frames_out))
    }

    fn write(&mut self, mut p: Pending, end: f64) -> Result<(), GifError> {
        let mut f = gif::Frame::from_rgba_speed(p.w, p.h, &mut p.rgba, QUANT_SPEED);
        f.left = p.x;
        f.top = p.y;
        // Drawn over what is already there: that is what makes a partial frame work.
        f.dispose = gif::DisposalMethod::Keep;
        // 1/100 s units; at least 2 (many viewers treat 0-1 as "as fast as possible").
        f.delay = (((end - p.start) * 100.0).round() as i64).clamp(2, u16::MAX as i64) as u16;
        self.enc.write_frame(&f).map_err(|e| GifError::Io(e.to_string()))?;
        self.frames_out += 1;
        Ok(())
    }
}

/// Output size for a `w`x`h` video: longest side at most `max_side`, aspect kept, never upscaled.
pub fn gif_size(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let long = w.max(h).max(1);
    if long <= max_side { return (w.max(1), h.max(1)); }
    let k = max_side as f64 / long as f64;
    (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1))
}
