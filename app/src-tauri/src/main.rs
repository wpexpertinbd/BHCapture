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
use bhcapture_core::naming::{Fields, Pattern, Stamp, unique_path};
use tauri_plugin_dialog::DialogExt;
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

/// Default output folder: `~/Pictures/BHCapture`, created on demand.
///
/// Falls back to the home directory, then to the temp dir, rather than failing a capture the user
/// has already framed — losing their selection over a missing folder is the worse outcome.
fn default_capture_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let dir = home.as_ref()
        .map(|h| h.join("Pictures").join("BHCapture"))
        .unwrap_or_else(std::env::temp_dir);
    if std::fs::create_dir_all(&dir).is_ok() {
        return dir;
    }
    home.unwrap_or_else(std::env::temp_dir)
}

/// Local wall-clock time. Core does no platform discovery, so the shell resolves the time zone
/// and hands it components -- `localtime_r` rather than UTC, because a file named with UTC is
/// wrong for everyone not on it.
fn local_stamp() -> Stamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    Stamp {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

fn write_png(frame: &Frame, path: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), frame.width(), frame.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().map_err(|e| e.to_string())?
        .write_image_data(&frame.to_rgba8()).map_err(|e| e.to_string())?;
    Ok(())
}

/// Best-effort clipboard copy. A clipboard failure must never lose a capture that is already on
/// disk, so this reports rather than propagates.
fn copy_to_clipboard(frame: &Frame) -> Result<(), String> {
    let img = arboard::ImageData {
        width: frame.width() as usize,
        height: frame.height() as usize,
        bytes: std::borrow::Cow::Owned(frame.to_rgba8()),
    };
    arboard::Clipboard::new().map_err(|e| e.to_string())?
        .set_image(img).map_err(|e| e.to_string())
}

/// `save_as` opens a native dialog so this one capture can go anywhere; otherwise it lands in the
/// default folder. The default is a default, not a cage.
#[tauri::command]
async fn selection_commit(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, save_as: bool)
    -> Result<String, String>
{
    commit(&app, &state, save_as)
}

fn commit(app: &tauri::AppHandle, state: &Arc<AppState>, save_as: bool) -> Result<String, String> {
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

    // The name is built from the pattern; `{title}`/`{app}` are untrusted window metadata, so
    // core sanitises them to a single path component (see bhcapture_core::naming).
    let stem = Pattern::default().render(local_stamp(), &Fields {
        width: frame.width(), height: frame.height(), ..Default::default()
    });

    let path = if save_as {
        let start = default_capture_dir();
        app.dialog().file()
            .set_title("Save capture")
            .set_directory(&start)
            .set_file_name(format!("{stem}.png"))
            .add_filter("PNG image", &["png"])
            .blocking_save_file()
            .ok_or_else(|| "cancelled".to_string())?
            .into_path().map_err(|e| e.to_string())?
    } else {
        let dir = default_capture_dir();
        unique_path(&dir, &stem, "png", &|p| p.exists())
    };

    write_png(&frame, &path)?;

    // Clipboard is a convenience on top of a file that already exists -- never fail the capture
    // for it, just say so.
    if let Err(e) = copy_to_clipboard(&frame) {
        eprintln!("[warn] capture saved but clipboard copy failed: {e}");
    }
    Ok(path.display().to_string())
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
        .plugin(tauri_plugin_dialog::init())
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

                    // Deliberately awkward, non-round, non-square sizes: the selection is
                    // FREEFORM. A fixed rect in a demo is only because no script can hold a mouse.
                    let cases: [(f64, f64, f64, f64); 4] = [
                        (137.0, 211.0, 538.0, 447.0),   // 401 x 236
                        (12.0, 33.0, 1297.0, 70.0),     // 1285 x 37  -- very wide, very thin
                        (700.0, 100.0, 733.0, 812.0),   // 33 x 712   -- very tall, very thin
                        (3.0, 5.0, 1349.0, 871.0),      // 1346 x 866 -- nearly the whole screen
                    ];
                    for (ax, ay, bx, by) in cases {
                        show_overlays(&h);
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        *st.drag.lock().unwrap() = Some(Drag {
                            anchor: LogPoint::new(ax, ay), cursor: LogPoint::new(bx, by) });
                        let r = LogRect::from_corners(LogPoint::new(ax, ay), LogPoint::new(bx, by));
                        broadcast(&h, Some(r));
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        let want = r.to_physical(bhcapture_core::geom::Scale::exact(2.0));
                        match commit(&h, &st, false) {
                            Ok(p) => println!("[demo] {}x{} pt -> want {}x{} px :: {p}",
                                r.size.w, r.size.h, want.size.w, want.size.h, p = p),
                            Err(e) => println!("[demo] FAILED {e}"),
                        }
                        std::thread::sleep(std::time::Duration::from_millis(400));
                    }

                    // Burst with no delay: these land in the SAME second, so the pattern yields an
                    // identical stem and the collision path must kick in rather than overwrite.
                    println!("[demo] -- collision burst --");
                    for _ in 0..3 {
                        show_overlays(&h);
                        *st.drag.lock().unwrap() = Some(Drag {
                            anchor: LogPoint::new(50.0, 50.0), cursor: LogPoint::new(250.0, 150.0) });
                        match commit(&h, &st, false) {
                            Ok(p) => println!("[demo] burst -> {p}"),
                            Err(e) => println!("[demo] burst FAILED {e}"),
                        }
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
