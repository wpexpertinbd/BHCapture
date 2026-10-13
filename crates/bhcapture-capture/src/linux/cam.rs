//! Linux camera: Video4Linux2 (`v4l`), on a thread of our own. Most webcams offer MJPEG at full
//! rate and raw YUYV at a lower one; MJPEG is preferred and decoded here, YUYV converted directly.
//! No person mask here: background replacement runs the portable segmentation model in the shell.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;
use v4l::FourCC;

use crate::camera::{CameraDevice, CameraFrame, CameraPermission, FrameSink};
use crate::CaptureError;

fn be(e: impl std::fmt::Display) -> CaptureError { CaptureError::Backend(e.to_string()) }

/// Video capture nodes only (a webcam also exposes metadata nodes, which cannot stream pictures).
pub fn camera_devices() -> Vec<CameraDevice> {
    let mut v: Vec<CameraDevice> = v4l::context::enum_devices().into_iter().filter_map(|node| {
        let dev = Device::with_path(node.path()).ok()?;
        let caps = dev.query_caps().ok()?;
        if !caps.capabilities.contains(v4l::capability::Flags::VIDEO_CAPTURE) { return None; }
        // A node that lists no picture formats is a metadata node.
        if dev.enum_formats().map(|f| f.is_empty()).unwrap_or(true) { return None; }
        Some(CameraDevice { id: node.path().display().to_string(), name: caps.card })
    }).collect();
    v.sort_by(|a, b| a.id.cmp(&b.id));
    v
}

/// No per-app permission on Linux; a device the user may not open is reported when starting.
pub fn camera_permission() -> CameraPermission { CameraPermission::Allowed }
pub fn camera_request_permission() -> bool { true }

fn yuyv_to_bgra(src: &[u8], w: usize, h: usize, stride: usize, out: &mut [u8]) {
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    for y in 0..h {
        let row = &src[y * stride..];
        for x in (0..w).step_by(2) {
            let i = x * 2;
            if i + 3 >= row.len() { break; }
            let (y0, u, y1, v) = (row[i] as i32, row[i + 1] as i32 - 128, row[i + 2] as i32, row[i + 3] as i32 - 128);
            for (k, yy) in [(0, y0), (1, y1)] {
                let c = 298 * (yy - 16);
                let o = (y * w + x + k) * 4;
                if o + 3 >= out.len() { break; }
                out[o] = clamp((c + 516 * u + 128) >> 8);
                out[o + 1] = clamp((c - 100 * u - 208 * v + 128) >> 8);
                out[o + 2] = clamp((c + 409 * v + 128) >> 8);
                out[o + 3] = 255;
            }
        }
    }
}

pub struct LinuxCamera {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    segmentation: Arc<AtomicU8>,
}

impl LinuxCamera {
    pub fn start(id: &str, max_height: u32, _fps: u32, mut sink: FrameSink) -> Result<Self, CaptureError> {
        let path = if id.is_empty() {
            camera_devices().into_iter().next().map(|d| d.id).ok_or_else(|| be("no camera found"))?
        } else { id.to_string() };
        let dev = Device::with_path(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied { CaptureError::PermissionDenied }
            else { be(format!("the camera could not be opened: {e}")) }
        })?;
        // Ask for MJPEG at up to max_height (720p by default on most webcams); fall back to YUYV.
        let mut fmt = dev.format().map_err(be)?;
        let want_h = max_height.clamp(240, 1080);
        fmt.width = want_h * 16 / 9;
        fmt.height = want_h;
        fmt.fourcc = FourCC::new(b"MJPG");
        let mut fmt = dev.set_format(&fmt).map_err(be)?;
        if fmt.fourcc != FourCC::new(b"MJPG") {
            fmt.fourcc = FourCC::new(b"YUYV");
            fmt = dev.set_format(&fmt).map_err(be)?;
        }
        let (w, h, stride, fourcc) = (fmt.width as usize, fmt.height as usize, fmt.stride as usize, fmt.fourcc);
        if fourcc != FourCC::new(b"MJPG") && fourcc != FourCC::new(b"YUYV") {
            return Err(be(format!("the camera offers neither MJPEG nor YUYV (it offers {fourcc})")));
        }
        let mut stream = MmapStream::with_buffers(&dev, Type::VideoCapture, 4).map_err(be)?;
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::spawn(move || {
            let _dev = dev;
            let mut out = vec![0u8; w * h * 4];
            while !s2.load(Ordering::SeqCst) {
                let Ok((buf, meta)) = stream.next() else { break };
                let data = &buf[..(meta.bytesused as usize).min(buf.len())];
                if fourcc == FourCC::new(b"MJPG") {
                    // image's JPEG decoder (zune-jpeg, already in the build for screenshots).
                    let Ok(img) = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg) else { continue };
                    let rgb = img.to_rgb8();
                    let (iw, ih) = (rgb.width() as usize, rgb.height() as usize);
                    if out.len() != iw * ih * 4 { out = vec![0u8; iw * ih * 4]; }
                    for (o, p) in out.chunks_exact_mut(4).zip(rgb.as_raw().chunks_exact(3)) { o.copy_from_slice(&[p[2], p[1], p[0], 255]); }
                    sink(&CameraFrame { bgra: &out, width: iw as u32, height: ih as u32, stride: iw * 4, mask: None });
                } else {
                    yuyv_to_bgra(data, w, h, stride.max(w * 2), &mut out);
                    sink(&CameraFrame { bgra: &out, width: w as u32, height: h as u32, stride: w * 4, mask: None });
                }
            }
        });
        Ok(Self { stop, thread: Some(thread), segmentation: Arc::new(AtomicU8::new(0)) })
    }

    /// Remembered for the shell, which runs the segmentation model on Linux.
    pub fn set_segmentation(&self, level: u8) { self.segmentation.store(level, Ordering::SeqCst); }
}

impl Drop for LinuxCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // stream.next() returns within a frame; after the join no callback can run.
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn yuyv_grey_and_white_convert() {
        // Two pixels: Y=16 (black), Y=235 (white), neutral chroma.
        let src = [16u8, 128, 235, 128];
        let mut out = [0u8; 8];
        super::yuyv_to_bgra(&src, 2, 1, 4, &mut out);
        assert_eq!(&out[0..3], &[0, 0, 0]);
        assert_eq!(&out[4..7], &[255, 255, 255]);
    }
}
