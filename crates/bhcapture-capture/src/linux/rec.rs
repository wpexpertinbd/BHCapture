//! Linux recording (next step: X11 frames + PulseAudio/PipeWire audio -> OpenH264 + Opus -> MP4).
//! Until then every entry point says plainly that it is not available, rather than failing oddly.

use std::path::Path;

use crate::record::{RecordOptions, RecordState, RecordStatus};
use crate::CaptureError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoExport { Mov, Hevc }

fn not_yet() -> CaptureError { CaptureError::Backend("recording is not available on Linux yet".into()) }

pub fn microphone_supported() -> bool { true }

pub struct LinuxRecorder { _private: () }

impl LinuxRecorder {
    pub fn start(_opt: &RecordOptions) -> Result<Self, CaptureError> { Err(not_yet()) }
    pub fn set_paused(&self, _paused: bool) {}
    pub fn status(&self) -> RecordStatus {
        RecordStatus { state: RecordState::Failed, seconds: 0.0, frames: 0, width: 0, height: 0,
                       level_system: 0.0, level_mic: 0.0, error: Some("not available".into()) }
    }
    pub fn stop(self) -> Result<(), CaptureError> { Ok(()) }
}

pub fn export_video(_src: &Path, _dst: &Path, _kind: VideoExport, _progress: impl FnMut(f64) -> bool) -> Result<(), CaptureError> { Err(not_yet()) }
pub fn mix_audio_tracks(_src: &Path, _dst: &Path) -> Result<(), CaptureError> { Err(CaptureError::Backend("nothing to mix".into())) }
pub fn video_frames(_p: &Path, _fps: f64, _max: u32, _f: impl FnMut(&[u8], u32, u32, f64, f64) -> bool) -> Result<(), CaptureError> { Err(not_yet()) }
pub fn watch_clicks(_f: impl Fn(f64, f64, bool) + Send + Sync + 'static) {}
pub fn unwatch_clicks() {}
