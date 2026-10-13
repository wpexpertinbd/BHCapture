//! The macOS recorder: ScreenCaptureKit + AVAssetWriter, via `rec.m`.

use std::ffi::CString;
use std::os::raw::c_char;

use crate::record::{RecordOptions, RecordState, RecordStatus};
use crate::CaptureError;

const ERRCAP: usize = 512;

#[repr(C)]
struct BhcRecordOptions {
    display_id: u32,
    x: f64, y: f64, w: f64, h: f64,
    fps: i32,
    show_cursor: i32,
    system_audio: i32,
    microphone: i32,
    path: *const c_char,
}

#[repr(C)]
#[derive(Default)]
struct BhcRecordStatus {
    state: i32,
    seconds: f64,
    frames: i64,
    width: i32,
    height: i32,
    level_system: f32,
    level_mic: f32,
}

#[repr(C)]
struct BhcRecorder { _private: [u8; 0] }

extern "C" {
    fn bhc_rec_start(opt: *const BhcRecordOptions, code: *mut i32, err: *mut c_char, errcap: i32) -> *mut BhcRecorder;
    fn bhc_rec_set_paused(r: *mut BhcRecorder, paused: i32);
    fn bhc_rec_status(r: *mut BhcRecorder, out: *mut BhcRecordStatus, err: *mut c_char, errcap: i32);
    fn bhc_rec_stop(r: *mut BhcRecorder, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_rec_mic_supported() -> i32;
    fn bhc_rec_mix_audio(src: *const c_char, dst: *const c_char, err: *mut c_char, errcap: i32) -> i32;
}

/// Mix every audio track of `src` into one stereo track, writing `dst` (video copied untouched).
/// Players such as browsers play only the FIRST audio track, so a recording with system audio and
/// the microphone on separate tracks would lose the microphone there.
pub fn mix_audio_tracks(src: &std::path::Path, dst: &std::path::Path) -> Result<(), CaptureError> {
    let s = CString::new(src.to_string_lossy().as_bytes()).map_err(|_| CaptureError::Backend("bad path".into()))?;
    let d = CString::new(dst.to_string_lossy().as_bytes()).map_err(|_| CaptureError::Backend("bad path".into()))?;
    let mut err = [0 as c_char; ERRCAP];
    let rc = unsafe { bhc_rec_mix_audio(s.as_ptr(), d.as_ptr(), err.as_mut_ptr(), ERRCAP as i32) };
    if rc == 0 { Ok(()) } else { Err(CaptureError::Backend(cstr(&err))) }
}

/// Whether the microphone can be recorded on this macOS (15 or later).
pub fn microphone_supported() -> bool { unsafe { bhc_rec_mic_supported() == 1 } }

fn cstr(buf: &[c_char]) -> String {
    let b: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// A recording in progress. Dropping it without [`stop`](MacRecorder::stop) still stops capture
/// and finishes the file, so a panic never leaves the screen being recorded.
pub struct MacRecorder { raw: *mut BhcRecorder }

// The shim serialises every call on its own queue.
unsafe impl Send for MacRecorder {}

impl MacRecorder {
    pub fn start(opt: &RecordOptions) -> Result<Self, CaptureError> {
        let path = CString::new(opt.path.to_string_lossy().as_bytes())
            .map_err(|_| CaptureError::Backend("the file name contains a NUL byte".into()))?;
        let o = BhcRecordOptions {
            display_id: opt.display.0,
            x: opt.region.origin.x, y: opt.region.origin.y, w: opt.region.size.w, h: opt.region.size.h,
            fps: opt.fps as i32,
            show_cursor: opt.show_cursor as i32,
            system_audio: opt.system_audio as i32,
            microphone: opt.microphone as i32,
            path: path.as_ptr(),
        };
        let mut code = 0i32;
        let mut err = [0 as c_char; ERRCAP];
        let raw = unsafe { bhc_rec_start(&o, &mut code, err.as_mut_ptr(), ERRCAP as i32) };
        if raw.is_null() {
            return Err(match code {
                -2 => CaptureError::PermissionDenied,
                -3 => CaptureError::Backend(cstr(&err)),
                -4 => CaptureError::DisplayNotFound(opt.display),
                _ => CaptureError::Backend(cstr(&err)),
            });
        }
        Ok(Self { raw })
    }

    pub fn set_paused(&self, paused: bool) { unsafe { bhc_rec_set_paused(self.raw, paused as i32) } }

    pub fn status(&self) -> RecordStatus {
        let mut s = BhcRecordStatus::default();
        let mut err = [0 as c_char; ERRCAP];
        unsafe { bhc_rec_status(self.raw, &mut s, err.as_mut_ptr(), ERRCAP as i32) };
        let state = match s.state { 2 => RecordState::Paused, 3 => RecordState::Failed, _ => RecordState::Recording };
        RecordStatus {
            state, seconds: s.seconds, frames: s.frames.max(0) as u64,
            width: s.width.max(0) as u32, height: s.height.max(0) as u32,
            level_system: s.level_system, level_mic: s.level_mic,
            error: if state == RecordState::Failed { Some(cstr(&err)) } else { None },
        }
    }

    /// Stop and finish the file. `Err` means the file is missing or not playable to the end; a
    /// recording cut short by a full disk may still have been kept up to its last 2 s fragment.
    pub fn stop(mut self) -> Result<(), CaptureError> { self.finish() }

    fn finish(&mut self) -> Result<(), CaptureError> {
        if self.raw.is_null() { return Ok(()); }
        let mut err = [0 as c_char; ERRCAP];
        let rc = unsafe { bhc_rec_stop(self.raw, err.as_mut_ptr(), ERRCAP as i32) };
        self.raw = std::ptr::null_mut();
        if rc == 0 { Ok(()) } else { Err(CaptureError::Backend(cstr(&err))) }
    }
}

impl Drop for MacRecorder {
    fn drop(&mut self) { let _ = self.finish(); }
}
