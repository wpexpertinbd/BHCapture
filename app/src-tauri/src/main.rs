#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! BHCapture shell: overlay windows, global hotkey, and the region selector's state.
//!
//! The **selection lives in Rust**, not in the webview. Each display gets its own overlay window,
//! and a drag that crosses a display boundary is still one selection — so no single webview can
//! own it. Rust holds the global rectangle and broadcasts it; every overlay renders the part that
//! falls on its own display.

use std::sync::{Arc, Mutex};

use bhcapture_capture::{macos::MacCapturer, CaptureError, Capturer};
use bhcapture_core::geom::{DisplayId, DisplayLayout, LogPoint, LogRect};
use bhcapture_core::image::Frame;
use global_hotkey::{hotkey::{Code, HotKey, Modifiers}, GlobalHotKeyEvent, GlobalHotKeyManager};
use serde::Serialize;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const OVERLAY_PREFIX: &str = "overlay-";

struct AppState {
    capturer: MacCapturer,
    /// Cached so the hotkey path never pays ~29 ms to re-enumerate displays.
    /// Refreshed on display reconfiguration, not per capture.
    layout: Mutex<DisplayLayout>,
    drag: Mutex<Option<Drag>>,
}

#[derive(Clone, Copy)]
struct Drag { anchor: LogPoint, cursor: LogPoint }

impl Drag {
    fn rect(&self) -> LogRect { LogRect::from_corners(self.anchor, self.cursor) }
}

/// What one overlay needs to map its local mouse coordinates into the global desktop.
#[derive(Serialize, Clone)]
struct OverlayInfo {
    display_id: u32,
    origin_x: f64,
    origin_y: f64,
    width: f64,
    height: f64,
    scale: f64,
}

#[derive(Serialize, Clone)]
struct SelectionPayload {
    /// Global logical rect, or `None` when there is no selection.
    rect: Option<[f64; 4]>,
}

fn broadcast(app: &tauri::AppHandle, rect: Option<LogRect>) {
    let payload = SelectionPayload {
        rect: rect.map(|r| [r.min_x(), r.min_y(), r.size.w, r.size.h]),
    };
    let _ = app.emit("selection", payload);
}

#[tauri::command]
fn overlay_info(window: tauri::Window, state: tauri::State<Arc<AppState>>) -> Result<OverlayInfo, String> {
    let id: u32 = window.label().strip_prefix(OVERLAY_PREFIX)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("window {} is not an overlay", window.label()))?;
    let layout = state.layout.lock().unwrap();
    let d = layout.get(DisplayId(id)).ok_or_else(|| format!("display {id} is gone"))?;
    Ok(OverlayInfo {
        display_id: id,
        origin_x: d.bounds.min_x(), origin_y: d.bounds.min_y(),
        width: d.bounds.size.w, height: d.bounds.size.h,
        scale: d.scale.get(),
    })
}

#[tauri::command]
fn selection_begin(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>, x: f64, y: f64) {
    let p = LogPoint::new(x, y);
    *state.drag.lock().unwrap() = Some(Drag { anchor: p, cursor: p });
    broadcast(&app, None); // a fresh press clears any previous rectangle
}

#[tauri::command]
fn selection_update(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>, x: f64, y: f64) {
    let mut g = state.drag.lock().unwrap();
    if let Some(d) = g.as_mut() {
        d.cursor = LogPoint::new(x, y);
        let r = d.rect();
        drop(g);
        broadcast(&app, Some(r));
    }
}

/// Arrow-key nudge of the moving corner, for pixel-accurate edges without the mouse.
#[tauri::command]
fn selection_nudge(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>, dx: f64, dy: f64) {
    let mut g = state.drag.lock().unwrap();
    if let Some(d) = g.as_mut() {
        d.cursor = LogPoint::new(d.cursor.x + dx, d.cursor.y + dy);
        let r = d.rect();
        drop(g);
        broadcast(&app, Some(r));
    }
}

fn hide_overlays(app: &tauri::AppHandle) {
    for (label, w) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) { let _ = w.hide(); }
    }
}

#[tauri::command]
fn selection_cancel(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>) {
    *state.drag.lock().unwrap() = None;
    broadcast(&app, None);
    hide_overlays(&app);
}

fn save_png(frame: &Frame) -> Result<String, String> {
    let dir = dirs_desktop();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // TODO(Phase 1): configurable naming pattern + collision handling live in core.
    let name = format!("BHCapture-{}.png", now_stamp());
    let path = dir.join(name);
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), frame.width(), frame.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().map_err(|e| e.to_string())?
        .write_image_data(&frame.to_rgba8()).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

fn dirs_desktop() -> std::path::PathBuf {
    std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join("Desktop"))
        .unwrap_or_else(|_| std::env::temp_dir())
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // Plain epoch seconds for now; the real pattern (date/time, counters) is core's job.
    format!("{secs}")
}

#[tauri::command]
fn selection_commit(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>) -> Result<String, String> {
    commit(&app, &state)
}

fn commit(app: &tauri::AppHandle, state: &Arc<AppState>) -> Result<String, String> {
    let drag = state.drag.lock().unwrap().take();
    let Some(drag) = drag else { return Err("no selection".into()) };
    let rect = drag.rect();

    // Hide before capturing anyway: the overlays are excluded from the capture by window id, but
    // the user should not keep staring at a scrim while the file is written.
    broadcast(app, None);
    hide_overlays(app);

    let layout = state.layout.lock().unwrap().clone();
    let frame = state.capturer.capture_region_in(rect, &layout).map_err(|e| match e {
        CaptureError::PermissionDenied =>
            "Screen Recording is not granted. Allow it in System Settings, then RELAUNCH BHCapture \
             — a running app never sees a new grant.".to_string(),
        other => other.to_string(),
    })?;
    save_png(&frame)
}

fn show_overlays(app: &tauri::AppHandle) {
    for (label, w) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) {
            let _ = w.show();
            let _ = w.set_focus(); // the overlay must have key focus for Esc/Enter/arrows
        }
    }
}

fn main() {
    let capturer = MacCapturer::new();
    let layout = capturer.list_displays().unwrap_or_default();
    let state = Arc::new(AppState {
        capturer,
        layout: Mutex::new(layout),
        drag: Mutex::new(None),
    });

    tauri::Builder::default()
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            overlay_info, selection_begin, selection_update,
            selection_nudge, selection_cancel, selection_commit
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let displays: Vec<_> = state.layout.lock().unwrap().displays().to_vec();

            // Pre-create every overlay hidden AT LAUNCH. Creating a webview window on
            // hotkey-press is visibly slow; spike 0c measured ~25 ms for a pre-created one.
            let mut overlay_ids = Vec::new();
            for d in &displays {
                let w = WebviewWindowBuilder::new(
                        app, format!("{OVERLAY_PREFIX}{}", d.id.0), WebviewUrl::App("index.html".into()))
                    .position(d.bounds.min_x(), d.bounds.min_y())
                    .inner_size(d.bounds.size.w, d.bounds.size.h)
                    .decorations(false)
                    .transparent(true)
                    .always_on_top(true)
                    .visible(false)
                    .resizable(false)
                    .shadow(false)
                    .title("BHCapture selector")
                    .build()?;

                // Register the overlay with the capturer so it is excluded from captures.
                // Without this the selector photographs its own scrim.
                #[cfg(target_os = "macos")]
                if let Ok(ptr) = w.ns_window() {
                    let id = unsafe { bhcapture_capture::macos::cgwindow_id(ptr) };
                    if id != 0 { overlay_ids.push(id); }
                }
            }
            state.capturer.set_excluded_windows(overlay_ids);

            let manager = GlobalHotKeyManager::new()?;
            let hk = HotKey::new(
                Some(Modifiers::SUPER | Modifiers::SHIFT | Modifiers::CONTROL), Code::Digit4);
            manager.register(hk)?;
            std::mem::forget(manager); // must outlive setup()
            println!("BHCapture ready. Region capture: Cmd+Shift+Ctrl+4");

            let (h, st) = (handle.clone(), state.clone());
            std::thread::spawn(move || {
                let rx = GlobalHotKeyEvent::receiver();
                while let Ok(ev) = rx.recv() {
                    if ev.state != global_hotkey::HotKeyState::Pressed { continue; }
                    *st.drag.lock().unwrap() = None;
                    broadcast(&h, None);
                    show_overlays(&h);
                }
            });
            // --demo drives the REAL command path with no keyboard, so the overlay and the
            // self-exclusion can be verified without a human pressing a global hotkey.
            if std::env::args().any(|a| a == "--demo") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    println!("[demo] showing overlays");
                    show_overlays(&h);
                    std::thread::sleep(std::time::Duration::from_millis(900));

                    let (ax, ay, bx, by) = (260.0, 180.0, 900.0, 600.0);
                    *st.drag.lock().unwrap() = Some(Drag {
                        anchor: LogPoint::new(ax, ay), cursor: LogPoint::new(bx, by) });
                    broadcast(&h, Some(LogRect::from_corners(
                        LogPoint::new(ax, ay), LogPoint::new(bx, by))));
                    println!("[demo] selection {ax},{ay} -> {bx},{by}; holding 7s so the UI can be photographed");
                    std::thread::sleep(std::time::Duration::from_millis(7000));

                    match commit(&h, &st) {
                        Ok(p) => println!("[demo] SAVED {p}"),
                        Err(e) => println!("[demo] FAILED {e}"),
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    h.exit(0);
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running BHCapture");
}
