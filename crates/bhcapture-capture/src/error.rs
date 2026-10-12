use bhcapture_core::geom::DisplayId;
use bhcapture_core::image::FrameError;
use crate::WindowId;

#[derive(Debug)]
pub enum CaptureError {
    /// Screen Recording has not been granted to **this** app.
    ///
    /// Distinct from every other failure on purpose. The caller must send the user to System
    /// Settings and then **relaunch** — a running process never observes a newly granted
    /// Screen Recording permission (verified in Phase 0a), so polling for it hangs forever.
    PermissionDenied,
    /// The OS is older than the API this backend needs.
    UnsupportedOsVersion { needs: &'static str },
    DisplayNotFound(DisplayId),
    WindowNotFound(WindowId),
    /// The region does not overlap any display.
    RegionOffScreen,
    /// The region has zero or negative extent.
    RegionEmpty,
    Frame(FrameError),
    /// Anything the platform reported that we cannot classify.
    Backend(String),
}

impl CaptureError {
    /// Whether retrying unchanged could plausibly succeed. `PermissionDenied` is deliberately
    /// **not** retryable: the fix requires a relaunch, not another attempt.
    pub fn is_retryable(&self) -> bool {
        matches!(self, CaptureError::Backend(_))
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::PermissionDenied =>
                write!(f, "Screen Recording permission has not been granted to this app"),
            CaptureError::UnsupportedOsVersion { needs } =>
                write!(f, "this capture backend needs {needs}"),
            CaptureError::DisplayNotFound(d) => write!(f, "display {} not found", d.0),
            CaptureError::WindowNotFound(w) => write!(f, "window {} not found", w.0),
            CaptureError::RegionOffScreen => write!(f, "region does not overlap any display"),
            CaptureError::RegionEmpty => write!(f, "region has zero size"),
            CaptureError::Frame(e) => write!(f, "frame error: {e}"),
            CaptureError::Backend(m) => write!(f, "capture backend error: {m}"),
        }
    }
}
impl std::error::Error for CaptureError {}
