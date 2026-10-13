//! Screen capture: one trait, per-OS backends behind `cfg()`.
//!
//! A backend only has to answer three questions — what can be captured, give me this display,
//! give me this window. [`Capturer::capture_region`] is a **provided** method implemented once in
//! portable code on top of those, so the mixed-DPI composition (the part most likely to be wrong)
//! exists in exactly one place and is tested without a screen attached.

use bhcapture_core::geom::{DisplayId, DisplayLayout, LogRect};
use bhcapture_core::image::{Frame, FrameError};

pub mod error;
pub mod snapshot;
pub mod record;
pub mod camera;
pub use error::CaptureError;
pub use snapshot::Snapshot;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod linux;

/// The current OS's backend under ONE set of names, so the shell never names an OS. Every item
/// here exists on every supported OS; what differs is only how it is done.
pub mod platform {
    #[cfg(target_os = "macos")]
    pub use crate::macos::{
        export_video, microphone_supported, mix_audio_tracks, unwatch_clicks, video_frames, watch_clicks,
        watch_displays, MacCapturer as PlatformCapturer, MacRecorder as Recorder, VideoExport,
        camera_devices, camera_permission, camera_request_permission, MacCamera as Camera,
    };
    #[cfg(windows)]
    pub use crate::windows::{
        export_video, microphone_supported, mix_audio_tracks, unwatch_clicks, video_frames, watch_clicks,
        watch_displays, VideoExport, WinCapturer as PlatformCapturer, WinRecorder as Recorder,
        camera_devices, camera_permission, camera_request_permission, WinCamera as Camera,
    };
    #[cfg(target_os = "linux")]
    pub use crate::linux::{
        export_video, microphone_supported, mix_audio_tracks, unwatch_clicks, video_frames, watch_clicks,
        watch_displays, LinuxCapturer as PlatformCapturer, LinuxRecorder as Recorder, VideoExport,
        camera_devices, camera_permission, camera_request_permission, LinuxCamera as Camera,
    };

    /// True where a capture cannot leave our own windows out (X11). The selector must then take its
    /// frozen frame BEFORE showing the overlay, or every screenshot would contain the dimmed selector.
    #[cfg(target_os = "linux")]
    pub const CAPTURE_SEES_OWN_WINDOWS: bool = crate::linux::CAPTURE_SEES_OWN_WINDOWS;
    #[cfg(not(target_os = "linux"))]
    pub const CAPTURE_SEES_OWN_WINDOWS: bool = false;

    /// Prepare one of OUR tool windows (selector overlay, recording bar): on macOS a non-activating
    /// panel; on Windows, kept out of captures. Returns false if it could not be done.
    ///
    /// # Safety
    /// `handle` is the live native window (`NSWindow*` / `HWND`); main thread.
    pub unsafe fn prepare_tool_window(handle: *mut std::ffi::c_void) -> bool {
        #[cfg(target_os = "macos")]
        { unsafe { crate::macos::make_overlay_panel(handle) == 1 } }
        #[cfg(windows)]
        { unsafe { crate::windows::set_excluded_from_capture(handle, true) } }
        #[cfg(not(any(target_os = "macos", windows)))]
        { let _ = handle; true }
    }

    /// Let a tool window be IN recordings (the click-highlight layer) or keep it out. Only Windows
    /// needs this per window; macOS includes it through the recorder's filter instead.
    ///
    /// # Safety
    /// As [`prepare_tool_window`].
    pub unsafe fn set_window_in_captures(handle: *mut std::ffi::c_void, included: bool) {
        #[cfg(windows)]
        { unsafe { crate::windows::set_excluded_from_capture(handle, !included); } }
        #[cfg(not(windows))]
        { let _ = (handle, included); }
    }

    /// The backend's id for one of our windows (CGWindowID / HWND).
    ///
    /// # Safety
    /// `handle` is a live native window.
    pub unsafe fn window_id(handle: *mut std::ffi::c_void) -> u32 {
        #[cfg(target_os = "macos")]
        { unsafe { crate::macos::cgwindow_id(handle) } }
        #[cfg(windows)]
        { unsafe { crate::windows::window_id(handle) } }
        #[cfg(not(any(target_os = "macos", windows)))]
        { let _ = handle; 0 }
    }

    /// macOS: make the menu bar respond after the app turns regular at run time. Elsewhere nothing.
    pub fn refresh_menu_bar() {
        #[cfg(target_os = "macos")]
        crate::macos::refresh_menu_bar();
    }
}

/// An OS window handle. Opaque: `CGWindowID` on macOS, `HWND` on Windows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub app: String,
    /// Position on the global logical desktop.
    pub bounds: LogRect,
    /// `false` for minimised or fully-occluded windows. Capturing one is usually a mistake,
    /// so the selector should offer them differently, not silently return an empty image.
    pub on_screen: bool,
    /// OS window layer; `0` is an ordinary application window. Menu bar, Dock, tooltips and
    /// overlays live elsewhere, so snap-to-window must not offer them.
    pub layer: i32,
    /// Front-to-back position among on-screen windows, lower = nearer the front. `None` when the
    /// OS does not list the window as on screen.
    pub z_order: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Targets {
    pub layout: DisplayLayout,
    pub windows: Vec<WindowInfo>,
}

pub trait Capturer {
    /// Just the displays.
    ///
    /// Separate from [`list_targets`](Capturer::list_targets) because enumerating windows is the
    /// expensive half and a region capture needs none of it. Measured warm on macOS 27 with 91
    /// windows open: `list_displays` ~29 ms, `list_targets` ~61 ms, so window enumeration adds
    /// ~31 ms the hotkey path would otherwise pay and discard. (`examples/listcost.rs`.)
    ///
    /// ⚠️ ~29 ms is still too slow to sit on a hotkey, so the overlay should hold a cached layout
    /// and use [`capture_region_in`](Capturer::capture_region_in) rather than listing per capture.
    fn list_displays(&self) -> Result<DisplayLayout, CaptureError>;

    /// Everything capturable right now, displays and windows.
    fn list_targets(&self) -> Result<Targets, CaptureError>;

    /// One whole display, in its own physical pixels.
    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError>;

    /// One window, including anything the compositor draws for it (shadow, rounded corners).
    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError>;

    /// An arbitrary logical region, which may span displays of **different scales**.
    ///
    /// Each display is read from its own buffer at its own scale, then resampled into one output
    /// at the highest scale touched — so dragging a selection off a Retina panel onto a 1080p
    /// monitor keeps the Retina half sharp instead of levelling both down.
    ///
    /// ⚠️ Spanning displays is **not atomic**: each display is captured in a separate call, so a
    /// region crossing a boundary samples the two at slightly different instants. Visible only on
    /// fast-moving content, and unavoidable without a compositor-level API.
    fn capture_region(&self, region: LogRect) -> Result<Frame, CaptureError> {
        if region.is_empty() {
            return Err(CaptureError::RegionEmpty);
        }
        let layout = self.list_displays()?;
        let out = layout
            .preferred_output_scale(region)
            .ok_or(CaptureError::RegionOffScreen)?;

        let whole = region.to_physical(out);
        if whole.is_empty() {
            return Err(CaptureError::RegionEmpty);
        }
        let pieces = layout.split_region(region, out);
        if pieces.is_empty() {
            return Err(CaptureError::RegionOffScreen);
        }

        let mut canvas = Frame::transparent(whole.size.w, whole.size.h, out).map_err(CaptureError::Frame)?;
        for p in pieces {
            let src = self.capture_display(p.display)?;
            canvas.blit(p.dest, &src, p.source);
        }
        Ok(canvas)
    }

    /// [`capture_region`](Capturer::capture_region) against a layout the caller already holds.
    ///
    /// The overlay knows the layout — it created one window per display from it — so re-reading
    /// it on every capture is wasted latency on the path where latency is most visible. The
    /// caller owns refreshing it on display reconfiguration.
    fn capture_region_in(&self, region: LogRect, layout: &DisplayLayout) -> Result<Frame, CaptureError> {
        if region.is_empty() {
            return Err(CaptureError::RegionEmpty);
        }
        let out = layout.preferred_output_scale(region).ok_or(CaptureError::RegionOffScreen)?;
        let whole = region.to_physical(out);
        if whole.is_empty() {
            return Err(CaptureError::RegionEmpty);
        }
        let pieces = layout.split_region(region, out);
        if pieces.is_empty() {
            return Err(CaptureError::RegionOffScreen);
        }
        let mut canvas = Frame::transparent(whole.size.w, whole.size.h, out).map_err(CaptureError::Frame)?;
        for p in pieces {
            let src = self.capture_display(p.display)?;
            canvas.blit(p.dest, &src, p.source);
        }
        Ok(canvas)
    }
}

impl From<FrameError> for CaptureError {
    fn from(e: FrameError) -> Self { CaptureError::Frame(e) }
}
