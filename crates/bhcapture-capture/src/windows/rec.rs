//! Windows recorder (placeholder while the backend is being built).
use crate::record::{RecordOptions, RecordState, RecordStatus};
use crate::CaptureError;

pub fn microphone_supported() -> bool { true }

pub struct WinRecorder { _private: () }

impl WinRecorder {
    pub fn start(_opt: &RecordOptions) -> Result<Self, CaptureError> {
        Err(CaptureError::Backend("recording is not built for Windows yet".into()))
    }
    pub fn set_paused(&self, _paused: bool) {}
    pub fn status(&self) -> RecordStatus {
        RecordStatus { state: RecordState::Failed, seconds: 0.0, frames: 0, width: 0, height: 0,
                       level_system: 0.0, level_mic: 0.0, error: Some("not built".into()) }
    }
    pub fn stop(self) -> Result<(), CaptureError> { Ok(()) }
}

pub fn watch_clicks(_f: impl Fn(f64, f64, bool) + Send + Sync + 'static) {}
pub fn unwatch_clicks() {}
