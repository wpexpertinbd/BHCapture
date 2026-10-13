//! Screen recording: what to record and how it is going. Portable; each OS backend implements it.

use std::path::PathBuf;

use bhcapture_core::geom::{DisplayId, LogRect};

/// What to record. The region is in the display's OWN logical points (origin = its top-left),
/// because a recording is taken from one display — a region straddling two is clipped to the one
/// that holds most of it by the caller.
#[derive(Clone, Debug)]
pub struct RecordOptions {
    pub display: DisplayId,
    pub region: LogRect,
    pub fps: u32,
    pub show_cursor: bool,
    pub system_audio: bool,
    pub microphone: bool,
    /// Run the microphone through the RNNoise filter (see `bhcapture_core::denoise`).
    pub noise_filter: bool,
    /// The `.mp4` to create. Must not already exist.
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordState {
    Recording,
    Paused,
    /// Stopped by an error (disk full, display unplugged…). Call `stop` to keep what was written.
    Failed,
}

#[derive(Clone, Debug)]
pub struct RecordStatus {
    pub state: RecordState,
    /// Recorded time, pauses excluded.
    pub seconds: f64,
    pub frames: u64,
    pub width: u32,
    pub height: u32,
    /// Peak 0..1 since the previous status call — for the level meters.
    pub level_system: f32,
    pub level_mic: f32,
    pub error: Option<String>,
}
