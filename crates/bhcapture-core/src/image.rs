//! Pixel buffers and compositing.
//!
//! Lives in core, not in a capture backend, because composing a region that spans displays of
//! different scales is portable arithmetic — and it is the part most worth testing without a
//! screen attached.
//!
//! Everything is **BGRA8, premultiplied** because that is what both platform capture APIs hand
//! back (`kCVPixelFormatType_32BGRA` on macOS, the same byte order from Windows.Graphics.Capture),
//! so the common path involves no conversion at all.

use crate::geom::{PhysRect, Scale};

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    ZeroSized,
    /// `stride` is narrower than one row of pixels.
    StrideTooSmall { stride: usize, min: usize },
    /// The buffer cannot hold `height` rows of `stride` bytes.
    BufferTooSmall { got: usize, need: usize },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::ZeroSized => write!(f, "frame has zero width or height"),
            FrameError::StrideTooSmall { stride, min } =>
                write!(f, "stride {stride} is smaller than one row ({min} bytes)"),
            FrameError::BufferTooSmall { got, need } =>
                write!(f, "pixel buffer is {got} bytes, need {need}"),
        }
    }
}
impl std::error::Error for FrameError {}

/// An owned BGRA8 image.
#[derive(Clone)]
pub struct Frame {
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: usize,
    scale: Scale,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width).field("height", &self.height)
            .field("stride", &self.stride).field("scale", &self.scale.get())
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Frame {
    /// Validates rather than trusting the caller: a stride or length mistake from an FFI
    /// boundary otherwise shows up as a panic deep inside a blit, or as silent corruption.
    pub fn from_bgra(data: Vec<u8>, width: u32, height: u32, stride: usize, scale: Scale)
        -> Result<Self, FrameError>
    {
        if width == 0 || height == 0 { return Err(FrameError::ZeroSized); }
        let min = width as usize * 4;
        if stride < min { return Err(FrameError::StrideTooSmall { stride, min }); }
        let need = stride * height as usize;
        if data.len() < need { return Err(FrameError::BufferTooSmall { got: data.len(), need }); }
        Ok(Self { data, width, height, stride, scale })
    }

    /// Fully transparent frame, used as the canvas a composed region is built into.
    pub fn transparent(width: u32, height: u32, scale: Scale) -> Result<Self, FrameError> {
        if width == 0 || height == 0 { return Err(FrameError::ZeroSized); }
        let stride = width as usize * 4;
        Ok(Self { data: vec![0; stride * height as usize], width, height, stride, scale })
    }

    #[inline] pub fn width(&self) -> u32 { self.width }
    #[inline] pub fn height(&self) -> u32 { self.height }
    #[inline] pub fn stride(&self) -> usize { self.stride }
    #[inline] pub fn scale(&self) -> Scale { self.scale }
    #[inline] pub fn bytes(&self) -> &[u8] { &self.data }
    #[inline] pub fn bounds(&self) -> PhysRect { PhysRect::new(0, 0, self.width, self.height) }

    /// BGRA at a pixel, or `None` outside the frame.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height { return None; }
        let o = y as usize * self.stride + x as usize * 4;
        Some([self.data[o], self.data[o + 1], self.data[o + 2], self.data[o + 3]])
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, px: [u8; 4]) {
        if x >= self.width || y >= self.height { return; }
        let o = y as usize * self.stride + x as usize * 4;
        self.data[o..o + 4].copy_from_slice(&px);
    }

    /// Repack into a tightly-packed RGBA8 buffer, for PNG encoders and the clipboard.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            let (s, d) = (y * self.stride, y * w * 4);
            for x in 0..w {
                let (so, dof) = (s + x * 4, d + x * 4);
                out[dof]     = self.data[so + 2]; // R
                out[dof + 1] = self.data[so + 1]; // G
                out[dof + 2] = self.data[so];     // B
                out[dof + 3] = self.data[so + 3]; // A
            }
        }
        out
    }

    /// Copy `src_rect` of `src` into `dest` of `self`, resampling when the two differ in size.
    ///
    /// Both rectangles are clipped to their frames first, so an out-of-range request is a no-op
    /// rather than a panic — the capture backends and the window server do not always agree on a
    /// display's exact pixel size, and that disagreement must not crash a capture.
    pub fn blit(&mut self, dest: PhysRect, src: &Frame, src_rect: PhysRect) {
        let Some(d) = dest.intersection(&self.bounds()) else { return };
        let Some(s) = src_rect.intersection(&src.bounds()) else { return };
        if d.is_empty() || s.is_empty() { return; }

        if d.size == s.size {
            // Same scale: a straight row copy, which is the overwhelmingly common case
            // (single display, or several displays that share a scale factor).
            for row in 0..d.size.h {
                let so = (s.min_y() + row as i32) as usize * src.stride + s.min_x() as usize * 4;
                let dof = (d.min_y() + row as i32) as usize * self.stride + d.min_x() as usize * 4;
                let n = d.size.w as usize * 4;
                self.data[dof..dof + n].copy_from_slice(&src.data[so..so + n]);
            }
            return;
        }

        // Different scale: bilinear. Nearest-neighbour is visibly blocky exactly where it would
        // be noticed -- text on a 1x monitor upscaled into a 2x composition.
        let fx = s.size.w as f64 / d.size.w as f64;
        let fy = s.size.h as f64 / d.size.h as f64;
        for row in 0..d.size.h {
            let sy = ((row as f64 + 0.5) * fy - 0.5).max(0.0);
            let y0 = sy.floor() as u32;
            let y1 = (y0 + 1).min(s.size.h - 1);
            let wy = sy - y0 as f64;
            for col in 0..d.size.w {
                let sx = ((col as f64 + 0.5) * fx - 0.5).max(0.0);
                let x0 = sx.floor() as u32;
                let x1 = (x0 + 1).min(s.size.w - 1);
                let wx = sx - x0 as f64;

                let at = |px: u32, py: u32| -> [u8; 4] {
                    let o = (s.min_y() as u32 + py) as usize * src.stride
                          + (s.min_x() as u32 + px) as usize * 4;
                    [src.data[o], src.data[o + 1], src.data[o + 2], src.data[o + 3]]
                };
                let (p00, p10, p01, p11) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));

                let mut px = [0u8; 4];
                for c in 0..4 {
                    let top = p00[c] as f64 * (1.0 - wx) + p10[c] as f64 * wx;
                    let bot = p01[c] as f64 * (1.0 - wx) + p11[c] as f64 * wx;
                    px[c] = (top * (1.0 - wy) + bot * wy).round().clamp(0.0, 255.0) as u8;
                }
                let dof = (d.min_y() + row as i32) as usize * self.stride
                        + (d.min_x() + col as i32) as usize * 4;
                self.data[dof..dof + 4].copy_from_slice(&px);
            }
        }
    }
}
