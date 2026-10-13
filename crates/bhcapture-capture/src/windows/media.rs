//! Windows media helpers (placeholder while the backend is being built).
use crate::CaptureError;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoExport { Mov, Hevc }

pub fn export_video(_src: &Path, _dst: &Path, _kind: VideoExport, _progress: impl FnMut(f64) -> bool) -> Result<(), CaptureError> {
    Err(CaptureError::Backend("not built for Windows yet".into()))
}
pub fn mix_audio_tracks(_src: &Path, _dst: &Path) -> Result<(), CaptureError> {
    Err(CaptureError::Backend("nothing to mix".into()))
}
pub fn video_frames(_path: &Path, _fps: f64, _max_side: u32, _f: impl FnMut(&[u8], u32, u32, f64, f64) -> bool) -> Result<(), CaptureError> {
    Err(CaptureError::Backend("not built for Windows yet".into()))
}
