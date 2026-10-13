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
    noise_filter: i32,
    auto_level: i32,
    voice_clarity: i32,
    include_window: u32,
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
    fn bhc_video_frames(path: *const c_char, fps: f64, max_side: i32,
        cb: extern "C" fn(*const u8, i32, i32, f64, f64, *mut std::ffi::c_void) -> i32,
        ctx: *mut std::ffi::c_void, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_export_video(src: *const c_char, dst: *const c_char, kind: i32,
        progress: extern "C" fn(f64, *mut std::ffi::c_void) -> i32, ctx: *mut std::ffi::c_void,
        err: *mut c_char, errcap: i32) -> i32;
    fn bhc_watch_clicks(cb: extern "C" fn(f64, f64, i32, *mut std::ffi::c_void), ctx: *mut std::ffi::c_void);
    fn bhc_unwatch_clicks();
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

type ClickFn = Box<dyn Fn(f64, f64, bool) + Send + Sync>;
static CLICK_WATCHER: std::sync::Mutex<Option<ClickFn>> = std::sync::Mutex::new(None);

extern "C" fn click_trampoline(x: f64, y: f64, right: i32, _ctx: *mut std::ffi::c_void) {
    if let Some(f) = CLICK_WATCHER.lock().unwrap().as_ref() { f(x, y, right != 0); }
}

/// Call `f(x, y, right_button)` for every mouse-down anywhere (global top-left logical points).
/// For the click highlight. MAIN THREAD ONLY. Replaces any previous watcher.
pub fn watch_clicks(f: impl Fn(f64, f64, bool) + Send + Sync + 'static) {
    *CLICK_WATCHER.lock().unwrap() = Some(Box::new(f));
    unsafe { bhc_watch_clicks(click_trampoline, std::ptr::null_mut()) };
}

/// Stop reporting clicks. MAIN THREAD ONLY.
pub fn unwatch_clicks() {
    unsafe { bhc_unwatch_clicks() };
    *CLICK_WATCHER.lock().unwrap() = None;
}

type FrameFn<'a> = &'a mut dyn FnMut(&[u8], u32, u32, f64, f64) -> bool;

extern "C" fn frame_trampoline(rgba: *const u8, w: i32, h: i32, t: f64, dur: f64, ctx: *mut std::ffi::c_void) -> i32 {
    let f = unsafe { &mut *(ctx as *mut FrameFn) };
    let px = unsafe { std::slice::from_raw_parts(rgba, (w.max(0) as usize) * (h.max(0) as usize) * 4) };
    if f(px, w as u32, h as u32, t, dur) { 0 } else { 1 }
}

/// Decode a recording into RGBA frames at `fps`, longest side at most `max_side`. `f(rgba, w, h,
/// t, duration)` returns `false` to stop early. For GIF export.
pub fn video_frames(path: &std::path::Path, fps: f64, max_side: u32,
                    mut f: impl FnMut(&[u8], u32, u32, f64, f64) -> bool) -> Result<(), CaptureError> {
    let p = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| CaptureError::Backend("bad path".into()))?;
    let mut err = [0 as c_char; ERRCAP];
    let mut dynf: FrameFn = &mut f;
    let rc = unsafe { bhc_video_frames(p.as_ptr(), fps, max_side as i32, frame_trampoline,
                                       &mut dynf as *mut FrameFn as *mut std::ffi::c_void, err.as_mut_ptr(), ERRCAP as i32) };
    if rc == 0 { Ok(()) } else { Err(CaptureError::Backend(cstr(&err))) }
}

/// What [`export_video`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoExport { Mov, Hevc }

type ProgressFn<'a> = &'a mut dyn FnMut(f64) -> bool;

extern "C" fn progress_trampoline(p: f64, ctx: *mut std::ffi::c_void) -> i32 {
    let f = unsafe { &mut *(ctx as *mut ProgressFn) };
    if f(p) { 0 } else { 1 }
}

/// Re-wrap a recording as MOV, or re-encode it as HEVC MP4. `progress(0..1)` returns `false` to cancel.
pub fn export_video(src: &std::path::Path, dst: &std::path::Path, kind: VideoExport,
                    mut progress: impl FnMut(f64) -> bool) -> Result<(), CaptureError> {
    let s = CString::new(src.to_string_lossy().as_bytes()).map_err(|_| CaptureError::Backend("bad path".into()))?;
    let d = CString::new(dst.to_string_lossy().as_bytes()).map_err(|_| CaptureError::Backend("bad path".into()))?;
    let mut err = [0 as c_char; ERRCAP];
    let mut dynf: ProgressFn = &mut progress;
    let k = match kind { VideoExport::Mov => 0, VideoExport::Hevc => 1 };
    let rc = unsafe { bhc_export_video(s.as_ptr(), d.as_ptr(), k, progress_trampoline,
                                       &mut dynf as *mut ProgressFn as *mut std::ffi::c_void, err.as_mut_ptr(), ERRCAP as i32) };
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
            noise_filter: opt.noise_filter as i32,
            auto_level: opt.auto_level as i32,
            voice_clarity: opt.voice_clarity as i32,
            include_window: opt.include_window.unwrap_or(0),
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

// The microphone noise filter lives in portable core; `rec.m` calls it through these, one filter
// per channel, from its single sample queue.

#[no_mangle]
pub extern "C" fn bhc_denoise_new() -> *mut std::ffi::c_void {
    Box::into_raw(Box::new(bhcapture_core::denoise::Denoiser::new())) as *mut std::ffi::c_void
}

/// # Safety
/// `d` from `bhc_denoise_new`, not yet freed; `s` points to `n` writable floats.
#[no_mangle]
pub unsafe extern "C" fn bhc_denoise_process(d: *mut std::ffi::c_void, s: *mut f32, n: usize) {
    if d.is_null() || s.is_null() || n == 0 { return; }
    let d = &mut *(d as *mut bhcapture_core::denoise::Denoiser);
    d.process(std::slice::from_raw_parts_mut(s, n));
}

/// # Safety
/// `d` from `bhc_denoise_new`, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn bhc_denoise_free(d: *mut std::ffi::c_void) {
    if !d.is_null() { drop(Box::from_raw(d as *mut bhcapture_core::denoise::Denoiser)); }
}

// Automatic voice level, likewise in portable core.

#[no_mangle]
pub extern "C" fn bhc_level_new(sample_rate: u32) -> *mut std::ffi::c_void {
    Box::into_raw(Box::new(bhcapture_core::level::AutoLevel::new(sample_rate))) as *mut std::ffi::c_void
}

/// # Safety
/// `l` from `bhc_level_new`, not yet freed; `s` points to `n` writable floats.
#[no_mangle]
pub unsafe extern "C" fn bhc_level_process(l: *mut std::ffi::c_void, s: *mut f32, n: usize) {
    if l.is_null() || s.is_null() || n == 0 { return; }
    (*(l as *mut bhcapture_core::level::AutoLevel)).process(std::slice::from_raw_parts_mut(s, n));
}

/// # Safety
/// `l` from `bhc_level_new`, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn bhc_level_free(l: *mut std::ffi::c_void) {
    if !l.is_null() { drop(Box::from_raw(l as *mut bhcapture_core::level::AutoLevel)); }
}

// Voice EQ, likewise in portable core.

#[no_mangle]
pub extern "C" fn bhc_eq_new(sample_rate: u32) -> *mut std::ffi::c_void {
    Box::into_raw(Box::new(bhcapture_core::voice_eq::VoiceEq::new(sample_rate))) as *mut std::ffi::c_void
}

/// # Safety
/// `e` from `bhc_eq_new`, not yet freed; `s` points to `n` writable floats.
#[no_mangle]
pub unsafe extern "C" fn bhc_eq_process(e: *mut std::ffi::c_void, s: *mut f32, n: usize) {
    if e.is_null() || s.is_null() || n == 0 { return; }
    (*(e as *mut bhcapture_core::voice_eq::VoiceEq)).process(std::slice::from_raw_parts_mut(s, n));
}

/// # Safety
/// `e` from `bhc_eq_new`, freed at most once.
#[no_mangle]
pub unsafe extern "C" fn bhc_eq_free(e: *mut std::ffi::c_void) {
    if !e.is_null() { drop(Box::from_raw(e as *mut bhcapture_core::voice_eq::VoiceEq)); }
}
