//! Camera capture (decision 34): one API, per-OS backends. Native on every OS -- never the
//! webview's getUserMedia, whose permission handling differs per engine and fails silently
//! (Cap reached the same conclusion; research 2026-10-13).
//!
//! A backend delivers BGRA frames, plus a person mask where it has one, to a callback on its own
//! thread. What to do with them (crop, enhance, background) is `bhcapture_core::camera`.

/// One camera.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CameraDevice {
    /// Stable id to start it with.
    pub id: String,
    pub name: String,
}

/// Camera access as the OS sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraPermission {
    /// Never asked: starting the camera will show the OS prompt.
    Unknown,
    Allowed,
    /// Denied or restricted: only the user can change it, in the OS settings.
    Denied,
}

/// One frame. Borrowed: copy what you need before returning.
pub struct CameraFrame<'a> {
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// One byte per pixel (255 = person) at its own size, when segmentation is on and worked.
    pub mask: Option<(&'a [u8], u32, u32, usize)>,
}

pub type FrameSink = Box<dyn FnMut(&CameraFrame) + Send>;
