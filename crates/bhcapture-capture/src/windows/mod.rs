//! Windows backend: GDI screen capture, Media Foundation recording, WASAPI audio.
//!
//! Coordinates: Tauri runs the process per-monitor DPI aware, so Win32 hands out PHYSICAL desktop
//! pixels. BHCapture's core works in a global LOGICAL space with a scale per display, so each
//! display's logical bounds are its physical bounds divided by its OWN scale. With every display at
//! the same scale that is exact; with mixed scales neighbouring displays can overlap or leave a gap
//! in logical space (Windows has no single logical desktop), which only matters for a selection
//! dragged ACROSS such a boundary.
//!
//! Our own windows are kept out of captures with `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
//! (Windows 10 2004+), applied to each tool window -- the equivalent of macOS's excluded-window list.

use std::sync::{Arc, Mutex};

use bhcapture_core::geom::{Display, DisplayId, DisplayLayout, LogRect, PhysSize, Scale};
use bhcapture_core::image::Frame;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
    DIB_RGB_COLORS, HDC, HGDIOBJ, HMONITOR, MONITORINFO, MONITORINFOEXW, SRCCOPY,
};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, EnumWindows, GetCursorInfo, GetIconInfo, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetWindowDisplayAffinity, CURSORINFO, CURSOR_SHOWING,
    DI_NORMAL, GWL_EXSTYLE, ICONINFO, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

use crate::{CaptureError, Capturer, Targets, WindowId, WindowInfo};

mod rec;
mod media;
pub use rec::{microphone_supported, WinRecorder};
pub use media::{export_video, mix_audio_tracks, video_frames, VideoExport};
pub use rec::{unwatch_clicks, watch_clicks};

/// One monitor as Win32 reports it.
struct Monitor { id: u32, phys: RECT, scale: f64 }

/// Display ids come from the device name (`\\.\DISPLAY2` -> 2), so they stay the same from one
/// enumeration to the next -- the overlay windows are labelled with them.
fn device_number(name: &[u16]) -> u32 {
    let s = String::from_utf16_lossy(name);
    let digits: String = s.trim_end_matches('\0').chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>()
        .into_iter().rev().collect();
    digits.parse().unwrap_or(1)
}

fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _dc: HDC, _r: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) }.as_bool() {
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            out.push(Monitor { id: device_number(&mi.szDevice), phys: mi.monitorInfo.rcMonitor, scale: dx.max(96) as f64 / 96.0 });
        }
        BOOL(1)
    }
    let mut v: Vec<Monitor> = Vec::new();
    unsafe { let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize)); }
    v
}

/// The display a physical point is on, for its scale.
fn scale_at(mons: &[Monitor], x: i32, y: i32) -> f64 {
    mons.iter().find(|m| x >= m.phys.left && x < m.phys.right && y >= m.phys.top && y < m.phys.bottom)
        .or_else(|| mons.first()).map_or(1.0, |m| m.scale)
}

/// Copy a physical-pixel rectangle of the screen into tightly packed BGRA, optionally with the
/// pointer drawn in. GDI leaves alpha at 0, so it is set opaque.
pub(crate) fn grab(x: i32, y: i32, w: i32, h: i32, cursor: bool) -> Result<Vec<u8>, CaptureError> {
    if w <= 0 || h <= 0 { return Err(CaptureError::RegionEmpty); }
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() { return Err(CaptureError::Backend("GetDC failed".into())); }
        let mem = CreateCompatibleDC(Some(screen));
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w, biHeight: -h, // negative = top-down rows
            biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = match CreateDIBSection(Some(screen), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) => b,
            Err(e) => { let _ = DeleteDC(mem); ReleaseDC(None, screen); return Err(CaptureError::Backend(e.to_string())); }
        };
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        // CAPTUREBLT includes layered windows; WDA_EXCLUDEFROMCAPTURE windows are still left out.
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT);
        if ok.is_ok() && cursor { draw_cursor(mem, x, y); }
        let mut out = Vec::new();
        if ok.is_ok() && !bits.is_null() {
            let n = (w as usize) * (h as usize) * 4;
            out = std::slice::from_raw_parts(bits as *const u8, n).to_vec();
            for px in out.chunks_exact_mut(4) { px[3] = 255; }
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        ok.map_err(|e| CaptureError::Backend(format!("BitBlt failed: {e}")))?;
        Ok(out)
    }
}

/// Draw the current pointer into `dc`, whose origin is screen point (`ox`, `oy`).
unsafe fn draw_cursor(dc: HDC, ox: i32, oy: i32) {
    let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    if unsafe { GetCursorInfo(&mut ci) }.is_err() || ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() { return; }
    let mut ii = ICONINFO::default();
    let (hx, hy) = if unsafe { GetIconInfo(ci.hCursor.into(), &mut ii) }.is_ok() {
        if !ii.hbmMask.is_invalid() { let _ = unsafe { DeleteObject(HGDIOBJ(ii.hbmMask.0)) }; }
        if !ii.hbmColor.is_invalid() { let _ = unsafe { DeleteObject(HGDIOBJ(ii.hbmColor.0)) }; }
        (ii.xHotspot as i32, ii.yHotspot as i32)
    } else { (0, 0) };
    let _ = unsafe { DrawIconEx(dc, ci.ptScreenPos.x - ox - hx, ci.ptScreenPos.y - oy - hy, ci.hCursor.into(), 0, 0, 0, None, DI_NORMAL) };
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32) }.ok()?;
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

fn process_name(hwnd: HWND) -> String {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { return String::new() };
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    let r = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    unsafe { let _ = CloseHandle(h); }
    if r.is_err() { return String::new(); }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Keep one of OUR windows out of every capture (screenshots and recordings), or let it back in.
///
/// # Safety
/// `hwnd` must be a live window handle owned by this process.
pub unsafe fn set_excluded_from_capture(hwnd: *mut std::ffi::c_void, excluded: bool) -> bool {
    unsafe { SetWindowDisplayAffinity(HWND(hwnd), if excluded { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE }) }.is_ok()
}

/// The id this backend uses for one of our windows (the HWND value).
///
/// # Safety
/// `hwnd` must be a window handle.
pub unsafe fn window_id(hwnd: *mut std::ffi::c_void) -> u32 { hwnd as usize as u32 }

/// Poll the display layout every 2 s and call `f` when it changes. (A hidden window for
/// WM_DISPLAYCHANGE would be immediate; 2 s is plenty for plugging in a monitor.)
pub fn watch_displays(f: impl Fn() + Send + Sync + 'static) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let f = Arc::new(f);
    STARTED.call_once(move || {
        std::thread::spawn(move || {
            let key = || monitors().iter().map(|m| (m.id, m.phys.left, m.phys.top, m.phys.right, m.phys.bottom, (m.scale * 100.0) as i32)).collect::<Vec<_>>();
            let mut last = key();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let now = key();
                if now != last { last = now; f(); }
            }
        });
    });
}

#[derive(Debug, Default, Clone)]
pub struct WinCapturer {
    show_cursor: Arc<Mutex<bool>>,
}

impl WinCapturer {
    pub fn new() -> Self { Self::default() }
    /// Windows uses per-window display affinity instead of an exclusion list; kept for the shared API.
    pub fn set_excluded_windows(&self, _ids: Vec<u32>) {}
    pub fn set_show_cursor(&self, on: bool) { *self.show_cursor.lock().unwrap() = on; }
    /// Windows has no screen-capture permission.
    pub fn has_permission() -> bool { true }
    pub fn request_permission() -> bool { true }
}

impl Capturer for WinCapturer {
    fn list_displays(&self) -> Result<DisplayLayout, CaptureError> {
        let ds = monitors().into_iter().map(|m| {
            let (pw, ph) = ((m.phys.right - m.phys.left) as u32, (m.phys.bottom - m.phys.top) as u32);
            Display {
                id: DisplayId(m.id),
                bounds: LogRect::new(m.phys.left as f64 / m.scale, m.phys.top as f64 / m.scale,
                                     pw as f64 / m.scale, ph as f64 / m.scale),
                physical: PhysSize { w: pw, h: ph },
                scale: Scale::new(m.scale).unwrap_or(Scale::exact(1.0)),
            }
        }).collect::<Vec<_>>();
        if ds.is_empty() { return Err(CaptureError::Backend("no displays".into())); }
        Ok(DisplayLayout::new(ds))
    }

    fn list_targets(&self) -> Result<Targets, CaptureError> {
        let layout = self.list_displays()?;
        let mons = monitors();
        unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
            unsafe { (*(data.0 as *mut Vec<HWND>)).push(h) };
            BOOL(1)
        }
        let mut hwnds: Vec<HWND> = Vec::new();
        unsafe { let _ = EnumWindows(Some(cb), LPARAM(&mut hwnds as *mut _ as isize)); }
        let mut windows = Vec::new();
        // EnumWindows lists top-level windows front to back: that order IS the z-order.
        for (z, h) in hwnds.into_iter().enumerate() {
            if !unsafe { IsWindowVisible(h) }.as_bool() { continue; }
            let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 { continue; }
            let mut cloaked = 0u32;
            let _ = unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4) };
            if cloaked != 0 { continue; } // hidden UWP / other-desktop windows
            let Some(r) = window_rect(h) else { continue };
            let n = unsafe { GetWindowTextLengthW(h) };
            let mut buf = vec![0u16; (n.max(0) as usize) + 1];
            let got = unsafe { GetWindowTextW(h, &mut buf) };
            let title = String::from_utf16_lossy(&buf[..got.max(0) as usize]);
            let s = scale_at(&mons, (r.left + r.right) / 2, (r.top + r.bottom) / 2);
            let minimised = unsafe { IsIconic(h) }.as_bool();
            windows.push(WindowInfo {
                id: WindowId(h.0 as usize as u64),
                title,
                app: process_name(h),
                bounds: LogRect::new(r.left as f64 / s, r.top as f64 / s, (r.right - r.left) as f64 / s, (r.bottom - r.top) as f64 / s),
                on_screen: !minimised,
                // Always-on-top windows are not ordinary snap targets, like macOS's floating layers.
                layer: if ex & WS_EX_TOPMOST.0 != 0 { 3 } else { 0 },
                z_order: if minimised { None } else { Some(z as u32) },
            });
        }
        Ok(Targets { layout, windows })
    }

    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError> {
        let m = monitors().into_iter().find(|m| m.id == id.0).ok_or(CaptureError::DisplayNotFound(id))?;
        let (w, h) = (m.phys.right - m.phys.left, m.phys.bottom - m.phys.top);
        let px = grab(m.phys.left, m.phys.top, w, h, *self.show_cursor.lock().unwrap())?;
        Frame::from_bgra(px, w as u32, h as u32, w as usize * 4, Scale::new(m.scale).unwrap_or(Scale::exact(1.0)))
            .map_err(CaptureError::Frame)
    }

    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError> {
        let hwnd = HWND(id.0 as usize as *mut std::ffi::c_void);
        let r = window_rect(hwnd).ok_or(CaptureError::WindowNotFound(id))?;
        let s = scale_at(&monitors(), (r.left + r.right) / 2, (r.top + r.bottom) / 2);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        // From the screen: what is visible of the window, which is what the user selected.
        let px = grab(r.left, r.top, w, h, false)?;
        Frame::from_bgra(px, w as u32, h as u32, w as usize * 4, Scale::new(s).unwrap_or(Scale::exact(1.0)))
            .map_err(CaptureError::Frame)
    }
}

/// Local wall-clock time (year, month, day, hour, minute, second), for file names.
pub fn local_time() -> (i32, u32, u32, u32, u32, u32) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (t.wYear as i32, t.wMonth as u32, t.wDay as u32, t.wHour as u32, t.wMinute as u32, t.wSecond as u32)
}
