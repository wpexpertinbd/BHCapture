//! Linux backend (decision 29: X11 and Wayland).
//!
//! This first step covers X11: displays from RandR, screenshots with `GetImage` on the root window,
//! the window list from `_NET_CLIENT_LIST_STACKING`. Under Wayland the same calls reach XWayland,
//! which reports the monitors correctly but CANNOT see native Wayland windows (they come out black);
//! the xdg-desktop-portal path for Wayland is the next step.
//!
//! ⚠️ X11 has no way to leave our own windows out of a capture (macOS filters them, Windows has
//! `WDA_EXCLUDEFROMCAPTURE`), so [`CAPTURE_SEES_OWN_WINDOWS`] tells the shell to take the selector's
//! frozen frame BEFORE it shows the overlay.

use std::sync::{Arc, Mutex};

use bhcapture_core::geom::{Display, DisplayId, DisplayLayout, LogRect, PhysSize, Scale};
use bhcapture_core::image::Frame;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

use crate::{CaptureError, Capturer, Targets, WindowId, WindowInfo};

mod rec;
pub use rec::{export_video, microphone_supported, mix_audio_tracks, unwatch_clicks, video_frames, watch_clicks,
              LinuxRecorder, VideoExport};

/// X11 captures include our own windows; the shell must freeze the screen before showing overlays.
pub const CAPTURE_SEES_OWN_WINDOWS: bool = true;

fn be(e: impl std::fmt::Display) -> CaptureError { CaptureError::Backend(e.to_string()) }

/// Whether this session is Wayland (the X11 calls then reach XWayland only).
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").map(|v| v.eq_ignore_ascii_case("wayland")).unwrap_or(false)
}

fn connect() -> Result<(RustConnection, Window), CaptureError> {
    let (c, screen) = x11rb::connect(None).map_err(|e| be(format!("cannot reach the X server: {e}")))?;
    let root = c.setup().roots[screen].root;
    Ok((c, root))
}

/// X11 has one scale for everything. GDK_SCALE (what GTK/WebKit use) is the honest source.
fn scale() -> f64 {
    std::env::var("GDK_SCALE").ok().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s >= 1.0).unwrap_or(1.0)
}

struct Monitor { id: u32, x: i32, y: i32, w: u32, h: u32 }

fn monitors(c: &RustConnection, root: Window) -> Result<Vec<Monitor>, CaptureError> {
    let r = c.randr_get_monitors(root, true).map_err(be)?.reply().map_err(be)?;
    let mut v: Vec<Monitor> = r.monitors.iter().enumerate().map(|(i, m)| Monitor {
        id: i as u32 + 1, x: m.x as i32, y: m.y as i32, w: m.width as u32, h: m.height as u32,
    }).collect();
    if v.is_empty() {
        // No RandR monitors (rare, e.g. some VNC servers): the whole root window is one display.
        let g = c.get_geometry(root).map_err(be)?.reply().map_err(be)?;
        v.push(Monitor { id: 1, x: 0, y: 0, w: g.width as u32, h: g.height as u32 });
    }
    Ok(v)
}

/// Physical rectangle of the root window as tightly packed BGRA, opaque.
pub(crate) fn grab(c: &RustConnection, root: Window, x: i32, y: i32, w: u32, h: u32) -> Result<Vec<u8>, CaptureError> {
    if w == 0 || h == 0 { return Err(CaptureError::RegionEmpty); }
    let img = c.get_image(ImageFormat::Z_PIXMAP, root, x as i16, y as i16, w as u16, h as u16, !0)
        .map_err(be)?.reply().map_err(|e| be(format!("GetImage failed: {e}")))?;
    let need = w as usize * h as usize * 4;
    if img.data.len() < need {
        return Err(be(format!("unexpected X image format (depth {}, {} bytes)", img.depth, img.data.len())));
    }
    // 24/32-bit TrueColor on little-endian X servers is B,G,R,X in memory -- already BGRA but for alpha.
    let mut px = img.data[..need].to_vec();
    for p in px.chunks_exact_mut(4) { p[3] = 255; }
    Ok(px)
}

fn atom(c: &RustConnection, name: &str) -> Option<Atom> {
    c.intern_atom(false, name.as_bytes()).ok()?.reply().ok().map(|r| r.atom)
}

fn prop_string(c: &RustConnection, w: Window, prop: Atom, ty: Atom) -> String {
    c.get_property(false, w, prop, ty, 0, 1024).ok().and_then(|ck| ck.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.value).trim_end_matches('\0').to_string()).unwrap_or_default()
}

/// Poll the display layout every 2 s and call `f` when it changes.
pub fn watch_displays(f: impl Fn() + Send + Sync + 'static) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let f = Arc::new(f);
    STARTED.call_once(move || {
        std::thread::spawn(move || {
            let key = || connect().ok().and_then(|(c, r)| monitors(&c, r).ok())
                .map(|m| m.iter().map(|m| (m.id, m.x, m.y, m.w, m.h)).collect::<Vec<_>>()).unwrap_or_default();
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
pub struct LinuxCapturer { show_cursor: Arc<Mutex<bool>> }

impl LinuxCapturer {
    pub fn new() -> Self { Self::default() }
    /// X11 cannot exclude windows from a capture; see [`CAPTURE_SEES_OWN_WINDOWS`].
    pub fn set_excluded_windows(&self, _ids: Vec<u32>) {}
    /// GetImage never includes the pointer; drawing it in comes with the XFixes step.
    pub fn set_show_cursor(&self, on: bool) { *self.show_cursor.lock().unwrap() = on; }
    /// No capture permission on X11. (Wayland asks through the portal when capturing.)
    pub fn has_permission() -> bool { true }
    pub fn request_permission() -> bool { true }
}

impl Capturer for LinuxCapturer {
    fn list_displays(&self) -> Result<DisplayLayout, CaptureError> {
        let (c, root) = connect()?;
        let s = scale();
        let ds = monitors(&c, root)?.into_iter().map(|m| Display {
            id: DisplayId(m.id),
            bounds: LogRect::new(m.x as f64 / s, m.y as f64 / s, m.w as f64 / s, m.h as f64 / s),
            physical: PhysSize { w: m.w, h: m.h },
            scale: Scale::new(s).unwrap_or(Scale::exact(1.0)),
        }).collect();
        Ok(DisplayLayout::new(ds))
    }

    fn list_targets(&self) -> Result<Targets, CaptureError> {
        let layout = self.list_displays()?;
        let (c, root) = connect()?;
        let s = scale();
        let mut windows = Vec::new();
        let (Some(stacking), Some(wname), Some(utf8)) =
            (atom(&c, "_NET_CLIENT_LIST_STACKING"), atom(&c, "_NET_WM_NAME"), atom(&c, "UTF8_STRING"))
        else { return Ok(Targets { layout, windows }) };
        let list = c.get_property(false, root, stacking, AtomEnum::WINDOW, 0, 4096).map_err(be)?.reply().map_err(be)?;
        let ids: Vec<u32> = list.value32().map(|it| it.collect()).unwrap_or_default();
        // _NET_CLIENT_LIST_STACKING is bottom-to-top; z_order counts from the front.
        for (z, w) in ids.iter().rev().enumerate() {
            let Ok(g) = c.get_geometry(*w).map_err(be).and_then(|ck| ck.reply().map_err(be)) else { continue };
            let Ok(t) = c.translate_coordinates(*w, root, 0, 0).map_err(be).and_then(|ck| ck.reply().map_err(be)) else { continue };
            let mut title = prop_string(&c, *w, wname, utf8);
            if title.is_empty() { title = prop_string(&c, *w, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()); }
            // WM_CLASS is "instance\0Class\0": the class is the app's name.
            let class = prop_string(&c, *w, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into());
            let app = class.split('\0').filter(|p| !p.is_empty()).last().unwrap_or("").to_string();
            windows.push(WindowInfo {
                id: WindowId(*w as u64),
                title, app,
                bounds: LogRect::new(t.dst_x as f64 / s, t.dst_y as f64 / s, g.width as f64 / s, g.height as f64 / s),
                on_screen: true,
                layer: 0,
                z_order: Some(z as u32),
            });
        }
        Ok(Targets { layout, windows })
    }

    fn capture_display(&self, id: DisplayId) -> Result<Frame, CaptureError> {
        let (c, root) = connect()?;
        let m = monitors(&c, root)?.into_iter().find(|m| m.id == id.0).ok_or(CaptureError::DisplayNotFound(id))?;
        let px = grab(&c, root, m.x, m.y, m.w, m.h)?;
        Frame::from_bgra(px, m.w, m.h, m.w as usize * 4, Scale::new(scale()).unwrap_or(Scale::exact(1.0)))
            .map_err(CaptureError::Frame)
    }

    fn capture_window(&self, id: WindowId) -> Result<Frame, CaptureError> {
        let (c, root) = connect()?;
        let w = id.0 as Window;
        let g = c.get_geometry(w).map_err(|_| CaptureError::WindowNotFound(id))?.reply().map_err(|_| CaptureError::WindowNotFound(id))?;
        let t = c.translate_coordinates(w, root, 0, 0).map_err(be)?.reply().map_err(be)?;
        let px = grab(&c, root, t.dst_x as i32, t.dst_y as i32, g.width as u32, g.height as u32)?;
        Frame::from_bgra(px, g.width as u32, g.height as u32, g.width as usize * 4, Scale::new(scale()).unwrap_or(Scale::exact(1.0)))
            .map_err(CaptureError::Frame)
    }
}
