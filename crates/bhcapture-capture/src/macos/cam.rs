//! AVFoundation camera + Vision person segmentation (cam.m), wrapped.

use std::ffi::{c_char, c_void, CString};

use crate::camera::{CameraDevice, CameraFrame, CameraPermission, FrameSink};
use crate::CaptureError;

type FrameFn = extern "C" fn(*mut c_void, *const u8, i32, i32, i32, *const u8, i32, i32, i32);

unsafe extern "C" {
    fn bhc_cam_list(out: *mut c_char, cap: i32) -> i32;
    fn bhc_cam_permission() -> i32;
    fn bhc_cam_request_permission() -> i32;
    fn bhc_cam_start(id: *const c_char, max_height: i32, fps: i32, f: FrameFn, ctx: *mut c_void,
                     err: *mut c_char, errcap: i32) -> *mut c_void;
    fn bhc_cam_set_segmentation(c: *mut c_void, level: i32);
    fn bhc_cam_stop(c: *mut c_void);
}

pub fn camera_devices() -> Vec<CameraDevice> {
    let mut buf = vec![0u8; 16 * 1024];
    unsafe { bhc_cam_list(buf.as_mut_ptr() as *mut c_char, buf.len() as i32) };
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).lines().filter_map(|l| {
        let (id, name) = l.split_once('\t')?;
        Some(CameraDevice { id: id.to_string(), name: name.to_string() })
    }).collect()
}

pub fn camera_permission() -> CameraPermission {
    match unsafe { bhc_cam_permission() } { 1 => CameraPermission::Allowed, 0 => CameraPermission::Unknown, _ => CameraPermission::Denied }
}

/// Shows the system prompt the first time; blocks until answered. Not on the main thread.
pub fn camera_request_permission() -> bool { unsafe { bhc_cam_request_permission() == 1 } }

extern "C" fn on_frame(ctx: *mut c_void, bgra: *const u8, w: i32, h: i32, stride: i32,
                       mask: *const u8, mw: i32, mh: i32, mstride: i32) {
    if ctx.is_null() || bgra.is_null() || w <= 0 || h <= 0 || stride < w * 4 { return; }
    // SAFETY: cam.m passes a locked pixel buffer of h rows x stride bytes, valid for this call.
    let px = unsafe { std::slice::from_raw_parts(bgra, stride as usize * h as usize) };
    let mask = if !mask.is_null() && mw > 0 && mh > 0 && mstride >= mw {
        Some((unsafe { std::slice::from_raw_parts(mask, mstride as usize * mh as usize) }, mw as u32, mh as u32, mstride as usize))
    } else { None };
    let sink = unsafe { &mut *(ctx as *mut FrameSink) };
    sink(&CameraFrame { bgra: px, width: w as u32, height: h as u32, stride: stride as usize, mask });
}

pub struct MacCamera {
    handle: *mut c_void,
    sink: *mut FrameSink,
}
// The handle is only touched through cam.m's thread-safe calls.
unsafe impl Send for MacCamera {}

impl MacCamera {
    /// `id` empty = the default camera. `max_height` caps the format (1080 = Full HD).
    pub fn start(id: &str, max_height: u32, fps: u32, sink: FrameSink) -> Result<Self, CaptureError> {
        if camera_permission() != CameraPermission::Allowed && !camera_request_permission() {
            return Err(CaptureError::PermissionDenied);
        }
        let cid = CString::new(id).unwrap_or_default();
        let sink = Box::into_raw(Box::new(sink));
        let mut err = vec![0u8; 512];
        let h = unsafe { bhc_cam_start(cid.as_ptr(), max_height as i32, fps as i32, on_frame, sink as *mut c_void,
                                        err.as_mut_ptr() as *mut c_char, err.len() as i32) };
        if h.is_null() {
            drop(unsafe { Box::from_raw(sink) });
            let end = err.iter().position(|b| *b == 0).unwrap_or(0);
            return Err(CaptureError::Backend(String::from_utf8_lossy(&err[..end]).into_owned()));
        }
        Ok(Self { handle: h, sink })
    }

    /// 0 off, 1 fast, 2 balanced.
    pub fn set_segmentation(&self, level: u8) { unsafe { bhc_cam_set_segmentation(self.handle, level as i32) } }
}

impl Drop for MacCamera {
    fn drop(&mut self) {
        // bhc_cam_stop drains the queue: no callback can still be using the sink after it.
        unsafe { bhc_cam_stop(self.handle); drop(Box::from_raw(self.sink)); }
    }
}
