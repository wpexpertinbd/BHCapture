//! macOS backend: ScreenCaptureKit via a C shim (see `sck.m`).

use std::os::raw::{c_char, c_void};
use std::sync::{Arc, Mutex};

use bhcapture_core::geom::{Display, DisplayId, DisplayLayout, LogRect, PhysSize, Scale};
use bhcapture_core::image::Frame;

use crate::{CaptureError, Capturer, Targets, WindowId, WindowInfo};

const ERRCAP: i32 = 512;
const TITLE_CAP: usize = 512;
const APP_CAP: usize = 256;
const MAX_DISPLAYS: i32 = 32;
const MAX_WINDOWS: i32 = 1024;

const BHC_OK: i32 = 0;
const BHC_ERR_PERMISSION: i32 = -2;
const BHC_ERR_UNSUPPORTED: i32 = -3;
const BHC_ERR_NOT_FOUND: i32 = -4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BhcDisplayInfo {
    display_id: u32,
    x: i32, y: i32,
    pt_width: i32, pt_height: i32,
    px_width: i32, px_height: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BhcWindowInfo {
    window_id: u64,
    x: i32, y: i32,
    pt_width: i32, pt_height: i32,
    on_screen: i32,
    layer: i32,
    z_order: i32,
    title: [c_char; TITLE_CAP],
    app: [c_char; APP_CAP],
}

impl Default for BhcWindowInfo {
    fn default() -> Self {
        Self { window_id: 0, x: 0, y: 0, pt_width: 0, pt_height: 0, on_screen: 0,
               layer: 0, z_order: -1, title: [0; TITLE_CAP], app: [0; APP_CAP] }
    }
}

#[repr(C)]
#[derive(Default)]
struct BhcImage {
    data: *mut u8,
    width: i32,
    height: i32,
    stride: i32,
    scale: f64,
}

extern "C" {
    fn bhc_preflight_permission() -> i32;
    fn bhc_request_permission() -> i32;
    fn bhc_list_displays(out: *mut BhcDisplayInfo, cap: i32, n: *mut i32, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_list_windows(out: *mut BhcWindowInfo, cap: i32, n: *mut i32, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_capture_display(id: u32, exclude: *const u32, exclude_n: i32, show_cursor: i32, out: *mut BhcImage, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_cgwindow_id(nswindow: *mut c_void) -> u32;
    fn bhc_capture_window(id: u64, out: *mut BhcImage, err: *mut c_char, errcap: i32) -> i32;
    fn bhc_image_free(img: *mut BhcImage);
}

fn cstr(buf: &[c_char]) -> String {
    let b: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// Turn a shim status code into a typed error. `PermissionDenied` must stay distinguishable:
/// it is the only failure whose remedy is "grant it, then relaunch" rather than "try again".
fn classify(rc: i32, err: &[c_char], not_found: CaptureError) -> CaptureError {
    match rc {
        BHC_ERR_PERMISSION => CaptureError::PermissionDenied,
        BHC_ERR_UNSUPPORTED => CaptureError::UnsupportedOsVersion { needs: "macOS 14 or later" },
        BHC_ERR_NOT_FOUND => not_found,
        _ => CaptureError::Backend(cstr(err)),
    }
}

/// Copy the shim's buffer into a Rust-owned `Frame` and release the C allocation.
///
/// The copy is deliberate: the shim allocates with `calloc`, and handing that pointer to
/// `Vec::from_raw_parts` would free it with Rust's allocator, which is undefined behaviour.
/// A 2704x1756 frame is ~19 MB, so this costs a couple of milliseconds; worth revisiting only
/// if capture latency ever becomes the bottleneck.
fn take_frame(img: &mut BhcImage) -> Result<Frame, CaptureError> {
    let (w, h, stride) = (img.width as u32, img.height as u32, img.stride as usize);
    let scale = Scale::new(img.scale).unwrap_or(Scale::exact(1.0));
    let data = unsafe { std::slice::from_raw_parts(img.data, stride * h as usize) }.to_vec();
    unsafe { bhc_image_free(img) };
    Frame::from_bgra(data, w, h, stride, scale).map_err(CaptureError::Frame)
}

/// CGWindowID of an `NSWindow` pointer, as Tauri's `ns_window()` returns.
///
/// # Safety
/// `ptr` must be a live `NSWindow*`.
pub unsafe fn cgwindow_id(ptr: *mut c_void) -> u32 { bhc_cgwindow_id(ptr) }

#[derive(Debug, Default, Clone)]
pub struct MacCapturer {
    /// CGWindowIDs left out of every display capture — our own overlay windows.
    excluded: Arc<Mutex<Vec<u32>>>,
    /// Whether the pointer appears in captures. Off by default: most screenshots are of content,
    /// not of where the pointer happened to be.
    show_cursor: Arc<Mutex<bool>>,
}

impl MacCapturer {
    pub fn new() -> Self { Self::default() }

    /// Windows to omit from captures. The selector registers its overlays here so it never
    /// photographs itself.
    pub fn set_excluded_windows(&self, ids: Vec<u32>) {
        *self.excluded.lock().unwrap() = ids;
    }

    pub fn set_show_cursor(&self, on: bool) {
        *self.show_cursor.lock().unwrap() = on;
    }

    /// Whether Screen Recording is granted. Never prompts.
    pub fn has_permission() -> bool { unsafe { bhc_preflight_permission() == 1 } }

    /// Raise the system prompt when the user has not yet decided.
    ///
    /// ⚠️ The grant does **not** take effect in this process. A running process never observes a
    /// newly granted Screen Recording permission (verified in Phase 0a), so callers must relaunch
    /// — polling [`has_permission`] after this returns will loop forever.
    pub fn request_permission() -> bool { unsafe { bhc_request_permission() == 1 } }
}

impl Capturer for MacCapturer {
    fn list_displays(&self) -> Result<DisplayLayout, CaptureError> {
        let mut dbuf = [BhcDisplayInfo::default(); MAX_DISPLAYS as usize];
        let mut n = 0i32;
        let mut err = [0 as c_char; ERRCAP as usize];
        let rc = unsafe { bhc_list_displays(dbuf.as_mut_ptr(), MAX_DISPLAYS, &mut n, err.as_mut_ptr(), ERRCAP) };
        if rc != BHC_OK {
            return Err(classify(rc, &err, CaptureError::Backend("no displays".into())));
        }

        let displays: Vec<Display> = dbuf.iter().take(n as usize).map(|d| {
            // Derive scale from the two sizes the OS reports rather than assuming 1x or 2x:
            // 1.25x and 1.5x modes are real, and a scaled Retina mode is not a round number.
            let scale = if d.pt_width > 0 && d.px_width > 0 {
                Scale::new(d.px_width as f64 / d.pt_width as f64).unwrap_or(Scale::exact(1.0))
            } else {
                Scale::exact(1.0)
            };
            Display {
                id: DisplayId(d.display_id),
                bounds: LogRect::new(d.x as f64, d.y as f64, d.pt_width as f64, d.pt_height as f64),
                physical: PhysSize { w: d.px_width.max(0) as u32, h: d.px_height.max(0) as u32 },
                scale,
            }
        }).collect();

        Ok(DisplayLayout::new(displays))
    }

    fn list_targets(&self) -> Result<Targets, CaptureError> {
        let layout = self.list_displays()?;
        let mut wbuf = vec![BhcWindowInfo::default(); MAX_WINDOWS as usize];
        let mut wn = 0i32;
        let mut werr = [0 as c_char; ERRCAP as usize];
        let wrc = unsafe { bhc_list_windows(wbuf.as_mut_ptr(), MAX_WINDOWS, &mut wn, werr.as_mut_ptr(), ERRCAP) };
        if wrc != BHC_OK {
            return Err(classify(wrc, &werr, CaptureError::Backend("no windows".into())));
        }
        let windows: Vec<WindowInfo> = wbuf.iter().take(wn as usize).map(|w| WindowInfo {
            id: WindowId(w.window_id),
            title: cstr(&w.title),
            app: cstr(&w.app),
            bounds: LogRect::new(w.x as f64, w.y as f64, w.pt_width as f64, w.pt_height as f64),
            on_screen: w.on_screen == 1,
            layer: w.layer,
            z_order: if w.z_order >= 0 { Some(w.z_order as u32) } else { None },
        }).collect();
        Ok(Targets { layout, windows })
    }

    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError> {
        let mut img = BhcImage::default();
        let mut err = [0 as c_char; ERRCAP as usize];
        let skip = self.excluded.lock().unwrap().clone();
        let cursor = i32::from(*self.show_cursor.lock().unwrap());
        let rc = unsafe {
            bhc_capture_display(id.0, skip.as_ptr(), skip.len() as i32, cursor, &mut img, err.as_mut_ptr(), ERRCAP)
        };
        if rc != BHC_OK {
            return Err(classify(rc, &err, CaptureError::DisplayNotFound(id)));
        }
        take_frame(&mut img)
    }

    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError> {
        let mut img = BhcImage::default();
        let mut err = [0 as c_char; ERRCAP as usize];
        let rc = unsafe { bhc_capture_window(id.0, &mut img, err.as_mut_ptr(), ERRCAP) };
        if rc != BHC_OK {
            return Err(classify(rc, &err, CaptureError::WindowNotFound(id)));
        }
        take_frame(&mut img)
    }
}
