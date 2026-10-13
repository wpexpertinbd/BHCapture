//! Windows camera: Media Foundation (the same API Windows' own Camera app sits on). Frames come
//! from a source reader with video processing on, converted to 32-bit BGR(X) by MF itself, on a
//! thread of our own. No person mask here: background replacement on Windows runs the portable
//! segmentation model in the shell (decision 35).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::camera::{CameraDevice, CameraFrame, CameraPermission, FrameSink};
use crate::CaptureError;

fn be(e: impl std::fmt::Display) -> CaptureError { CaptureError::Backend(e.to_string()) }

/// E_ACCESSDENIED: Settings › Privacy & security › Camera is off for desktop apps.
const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;

struct Mf;
impl Mf {
    fn start() -> Result<Self, CaptureError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(be)?;
        }
        Ok(Mf)
    }
}
impl Drop for Mf { fn drop(&mut self) { unsafe { let _ = MFShutdown(); } } }

unsafe fn string_attr(a: &IMFActivate, key: &GUID) -> String {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    if unsafe { a.GetAllocatedString(key, &mut p, &mut len) }.is_err() || p.is_null() { return String::new(); }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// Every video capture device, as (activate, symbolic link, friendly name).
fn devices() -> Result<Vec<(IMFActivate, String, String)>, CaptureError> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(be)?;
        let attrs = attrs.ok_or_else(|| be("no attributes"))?;
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(be)?;
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut list, &mut count).map_err(be)?;
        let mut out = Vec::new();
        if !list.is_null() {
            for i in 0..count as usize {
                if let Some(a) = (*list.add(i)).take() {
                    let link = string_attr(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK);
                    let name = string_attr(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
                    out.push((a, link, name));
                }
            }
            CoTaskMemFree(Some(list as *const _));
        }
        Ok(out)
    }
}

pub fn camera_devices() -> Vec<CameraDevice> {
    let Ok(_mf) = Mf::start() else { return Vec::new() };
    devices().map(|d| d.into_iter().map(|(_, id, name)| CameraDevice { id, name }).collect()).unwrap_or_default()
}

/// Windows has no per-app prompt for desktop apps, only the system switch; whether it is on is
/// learned when the camera is opened (an access-denied error).
pub fn camera_permission() -> CameraPermission { CameraPermission::Allowed }
pub fn camera_request_permission() -> bool { true }

fn unpack(v: u64) -> (u32, u32) { ((v >> 32) as u32, v as u32) }

/// The native format with the most pixels no taller than `max_height` (ties: the higher rate).
unsafe fn best_native_type(r: &IMFSourceReader, max_height: u32) -> Option<IMFMediaType> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut best: Option<(u64, u32, IMFMediaType)> = None;
    for i in 0.. {
        let Ok(t) = (unsafe { r.GetNativeMediaType(stream, i) }) else { break };
        let (w, h) = unpack(unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0));
        let (num, den) = unpack(unsafe { t.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(0));
        let fps = if den > 0 { num / den } else { 0 };
        if h == 0 || h > max_height || fps < 15 { continue; }
        let px = w as u64 * h as u64;
        if best.as_ref().map_or(true, |(bp, bf, _)| px > *bp || (px == *bp && fps > *bf)) { best = Some((px, fps, t)); }
    }
    best.map(|b| b.2)
}

pub struct WinCamera {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    segmentation: Arc<AtomicU8>,
}

impl WinCamera {
    pub fn start(id: &str, max_height: u32, _fps: u32, mut sink: FrameSink) -> Result<Self, CaptureError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), CaptureError>>();
        let (id, s2) = (id.to_string(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut run = || -> Result<(), CaptureError> {
                let _mf = Mf::start()?;
                let devs = devices()?;
                let (act, _, _) = devs.iter().find(|(_, l, _)| !id.is_empty() && *l == id)
                    .or_else(|| devs.first()).ok_or_else(|| be("no camera found"))?;
                let source: IMFMediaSource = unsafe { act.ActivateObject() }.map_err(|e| {
                    if e.code().0 == E_ACCESSDENIED { CaptureError::PermissionDenied } else { be(format!("the camera could not be opened: {e}")) }
                })?;
                let reader = unsafe {
                    let mut attrs: Option<IMFAttributes> = None;
                    MFCreateAttributes(&mut attrs, 1).map_err(be)?;
                    let attrs = attrs.ok_or_else(|| be("no attributes"))?;
                    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(be)?;
                    MFCreateSourceReaderFromMediaSource(&source, &attrs).map_err(be)?
                };
                let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
                unsafe {
                    if let Some(native) = best_native_type(&reader, max_height.max(240)) {
                        let _ = reader.SetCurrentMediaType(stream, None, &native);
                    }
                    let out = MFCreateMediaType().map_err(be)?;
                    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(be)?;
                    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(be)?;
                    reader.SetCurrentMediaType(stream, None, &out).map_err(|e| be(format!("the camera format is not supported: {e}")))?;
                }
                // Size and row order of what the reader delivers; read again whenever it changes.
                let current = |r: &IMFSourceReader| -> Result<(u32, u32, bool), CaptureError> {
                    let cur = unsafe { r.GetCurrentMediaType(stream) }.map_err(be)?;
                    let (w, h) = unpack(unsafe { cur.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(be)?);
                    // RGB32 is bottom-up unless MF_MT_DEFAULT_STRIDE says otherwise.
                    let stride = unsafe { cur.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.map(|v| v as i32).unwrap_or(-((w * 4) as i32));
                    if w == 0 || h == 0 { return Err(be("the camera reported no frame size")); }
                    Ok((w, h, stride < 0))
                };
                let (mut w, mut h, mut bottom_up) = current(&reader)?;
                let _ = tx.send(Ok(()));
                let mut frame = vec![0u8; w as usize * h as usize * 4];
                while !s2.load(Ordering::SeqCst) {
                    let (mut flags, mut sample) = (0u32, None::<IMFSample>);
                    let r = unsafe { reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)) };
                    if r.is_err() || flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 { break; }
                    if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                        let Ok(n) = current(&reader) else { break };
                        (w, h, bottom_up) = n;
                        frame = vec![0u8; w as usize * h as usize * 4];
                    }
                    let Some(sample) = sample else { continue };
                    let Ok(buf) = (unsafe { sample.GetBufferByIndex(0) }) else { continue };
                    let row_bytes = w as usize * 4;
                    // A 2D buffer knows its pitch (negative = bottom-up rows): copy top-down, only
                    // when the locked region really holds h rows of that pitch.
                    if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                        let (mut p, mut pitch) = (std::ptr::null_mut::<u8>(), 0i32);
                        if unsafe { b2.Lock2D(&mut p, &mut pitch) }.is_ok() {
                            let len = unsafe { b2.GetContiguousLength() }.unwrap_or(0) as usize;
                            let ok = !p.is_null() && pitch.unsigned_abs() as usize >= row_bytes
                                && pitch.unsigned_abs() as usize * (h as usize - 1) + row_bytes <= len;
                            if ok {
                                for y in 0..h as isize {
                                    let row = unsafe { p.offset(y * pitch as isize) };
                                    let dst = &mut frame[y as usize * row_bytes..(y as usize + 1) * row_bytes];
                                    unsafe { std::ptr::copy_nonoverlapping(row, dst.as_mut_ptr(), row_bytes) };
                                }
                            }
                            let _ = unsafe { b2.Unlock2D() };
                            if ok { sink(&CameraFrame { bgra: &frame, width: w, height: h, stride: row_bytes, mask: None }); }
                        }
                        continue;
                    }
                    let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
                    if unsafe { buf.Lock(&mut p, None, Some(&mut len)) }.is_ok() {
                        let ok = !p.is_null() && len as usize >= frame.len();
                        if ok {
                            for y in 0..h as usize {
                                let src_row = if bottom_up { h as usize - 1 - y } else { y };
                                unsafe { std::ptr::copy_nonoverlapping(p.add(src_row * row_bytes), frame.as_mut_ptr().add(y * row_bytes), row_bytes) };
                            }
                        }
                        let _ = unsafe { buf.Unlock() };
                        if ok { sink(&CameraFrame { bgra: &frame, width: w, height: h, stride: row_bytes, mask: None }); }
                    }
                }
                unsafe { let _ = source.Shutdown(); }
                Ok(())
            };
            if let Err(e) = run() { let _ = tx.send(Err(e)); }
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread), segmentation: Arc::new(AtomicU8::new(0)) }),
            Ok(Err(e)) => { let _ = thread.join(); Err(e) }
            Err(_) => { stop.store(true, Ordering::SeqCst); Err(be("the camera did not start")) }
        }
    }

    /// Remembered for the shell, which runs the segmentation model on Windows.
    pub fn set_segmentation(&self, level: u8) { self.segmentation.store(level, Ordering::SeqCst); }
}

impl Drop for WinCamera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // ReadSample normally returns within a frame. A stalled camera must not hang stopping
        // (and with it the app): wait a moment, then let the thread finish on its own -- it owns
        // its sink, so nothing it touches is freed here (audit 2026-10-13).
        if let Some(t) = self.thread.take() {
            let t0 = std::time::Instant::now();
            while !t.is_finished() && t0.elapsed() < std::time::Duration::from_millis(1500) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if t.is_finished() { let _ = t.join(); }
        }
    }
}
