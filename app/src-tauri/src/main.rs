#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! BHCapture shell: overlay windows, global hotkey, and the region selector's state.
//!
//! The **selection lives in Rust**, not in the webview. Each display gets its own overlay window,
//! and a drag that crosses a display boundary is still one selection — so no single webview can
//! own it. Rust holds the global rectangle and broadcasts it; every overlay renders the part that
//! falls on its own display.

use std::sync::{Arc, Mutex};

use bhcapture_capture::{macos::MacCapturer, CaptureError, Capturer, Snapshot};
use bhcapture_core::geom::{DisplayId, DisplayLayout, LogPoint, LogRect};
use bhcapture_core::image::Frame;
use bhcapture_core::naming::{Fields, Pattern, Stamp, unique_path};
use bhcapture_core::settings::{LoadOutcome, Settings, SettingsStore};
use bhcapture_core::snap::{snap_window, Candidate};
use base64::Engine;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri_plugin_dialog::DialogExt;
use global_hotkey::{hotkey::{Code, HotKey, Modifiers}, GlobalHotKeyEvent, GlobalHotKeyManager};
use serde::Serialize;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;

const OVERLAY_PREFIX: &str = "overlay-";

const SETTINGS_WINDOW: &str = "settings";

struct AppState {
    capturer: MacCapturer,
    settings: Mutex<SettingsStore>,
    /// Cached so the hotkey path never pays ~29 ms to re-enumerate displays.
    /// Refreshed on display reconfiguration, not per capture.
    layout: Mutex<DisplayLayout>,
    drag: Mutex<Option<Drag>>,
    /// Frozen copy of every display, taken when the selector opens. Feeds the magnifier and the
    /// final crop, so the file holds exactly what the user saw while selecting.
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    /// Windows the pointer can snap to, in front-to-back order.
    cands: Mutex<Vec<Candidate>>,
    /// CGWindowIDs of our own overlays, never offered as snap targets.
    overlay_ids: Mutex<Vec<u32>>,
    /// Bumped each time the selector opens or closes, so a slow snapshot thread from a selector
    /// the user already dismissed cannot land its result on the next one.
    epoch: AtomicU64,
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

#[derive(Serialize, Clone)]
struct SettingsView {
    settings: Settings,
    /// Resolved default, so the UI can show what "default" actually means rather than blank.
    default_dir: String,
    path: String,
    /// False when the on-disk file could not be read. The UI must say so and must not pretend a
    /// save succeeded.
    writable: bool,
    notice: Option<String>,
}

fn settings_view(state: &AppState, notice: Option<String>) -> SettingsView {
    let st = state.settings.lock().unwrap();
    SettingsView {
        settings: st.get().clone(),
        default_dir: default_capture_dir().display().to_string(),
        path: st.path().display().to_string(),
        writable: st.is_writable(),
        notice,
    }
}

#[tauri::command]
fn settings_get(state: tauri::State<Arc<AppState>>) -> SettingsView {
    settings_view(&state, None)
}

#[tauri::command]
fn settings_save(state: tauri::State<Arc<AppState>>, next: Settings) -> Result<SettingsView, String> {
    let repaired = { state.settings.lock().unwrap().set(next) };
    // Report a refused save honestly rather than letting the UI claim it worked.
    state.settings.lock().unwrap().save().map_err(|e| e.to_string())?;
    apply_settings(&state);
    let notice = if repaired.is_empty() { None } else { Some(repaired.join("; ")) };
    Ok(settings_view(&state, notice))
}

/// Native folder picker for the output directory.
#[tauri::command]
async fn settings_pick_dir(app: tauri::AppHandle) -> Option<String> {
    app.dialog().file().set_title("Choose where captures are saved")
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.display().to_string())
}

#[tauri::command]
fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(SETTINGS_WINDOW) {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, SETTINGS_WINDOW, WebviewUrl::App("index.html".into()))
        .title("BHCapture Settings")
        .inner_size(560.0, 620.0)
        .resizable(true)
        .build().map_err(|e| e.to_string())?;
    Ok(())
}

/// Release the frozen frames (tens of MB on a Retina panel) and invalidate in-flight work.
fn release_frozen(state: &AppState) {
    state.epoch.fetch_add(1, Ordering::SeqCst);
    *state.snapshot.lock().unwrap() = None;
    state.cands.lock().unwrap().clear();
}

/// The one way the selector opens, so the hotkey, the tray and the tests behave identically.
fn open_selector(app: &tauri::AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    *state.drag.lock().unwrap() = None;
    release_frozen(&state);
    let epoch = state.epoch.load(Ordering::SeqCst);
    broadcast(app, None);
    show_overlays(app); // appear first: the freeze happens behind an already-visible overlay

    // Two independent jobs, each discarded if the selector has since closed or reopened.
    let st = state.clone();
    std::thread::spawn(move || {
        let layout = st.layout.lock().unwrap().clone();
        match Snapshot::take(&st.capturer, &layout) {
            Ok(snap) if st.epoch.load(Ordering::SeqCst) == epoch => {
                *st.snapshot.lock().unwrap() = Some(Arc::new(snap));
            }
            Ok(_) => {}
            Err(e) => eprintln!("[warn] could not freeze the screen ({e}); the magnifier is off and capture will be live"),
        }
    });
    let st = state.clone();
    std::thread::spawn(move || {
        match st.capturer.list_targets() {
            Ok(t) if st.epoch.load(Ordering::SeqCst) == epoch => {
                let own = st.overlay_ids.lock().unwrap().clone();
                let cands: Vec<Candidate> = t.windows.iter()
                    .filter(|w| !own.contains(&(w.id.0 as u32)))
                    .map(|w| Candidate { bounds: w.bounds, layer: w.layer, z: w.z_order })
                    .collect();
                *st.cands.lock().unwrap() = cands;
            }
            Ok(_) => {}
            Err(e) => eprintln!("[warn] could not list windows ({e}); snap-to-window is off"),
        }
    });
}

/// The window under the pointer, if any, for the highlight.
#[tauri::command]
fn snap_at(state: tauri::State<Arc<AppState>>, x: f64, y: f64) -> Option<[f64; 4]> {
    let layout = state.layout.lock().unwrap();
    let cands = state.cands.lock().unwrap();
    snap_window(&cands, &layout, LogPoint::new(x, y))
        .map(|r| [r.min_x(), r.min_y(), r.size.w, r.size.h])
}

#[derive(Serialize)]
struct Loupe {
    /// Side length in physical pixels.
    size: u32,
    /// Physical pixels per logical point on the display under the pointer.
    scale: f64,
    /// Tightly packed RGBA8, base64. 21x21 is ~1.7 KB, far cheaper than shipping JSON arrays.
    rgba: String,
}

/// Pixels around the pointer for the magnifier. `None` until the snapshot is ready, which the UI
/// treats as "not available yet", not as an error.
#[tauri::command]
fn loupe(state: tauri::State<Arc<AppState>>, x: f64, y: f64) -> Option<Loupe> {
    let snap = state.snapshot.lock().unwrap().clone()?;
    let f = snap.loupe(LogPoint::new(x, y), 10)?;
    Some(Loupe {
        size: f.width(),
        scale: f.scale().get(),
        rgba: base64::engine::general_purpose::STANDARD.encode(f.to_rgba8()),
    })
}

/// Select a whole rectangle in one step (a click on a snapped window). The corner that nudge moves
/// is the bottom-right, so the result can still be fine-tuned with the arrow keys.
#[tauri::command]
fn selection_set_rect(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>, x: f64, y: f64, w: f64, h: f64) {
    let (a, b) = (LogPoint::new(x, y), LogPoint::new(x + w, y + h));
    *state.drag.lock().unwrap() = Some(Drag { anchor: a, cursor: b });
    broadcast(&app, Some(LogRect::from_corners(a, b)));
}

fn hide_overlays(app: &tauri::AppHandle) {
    for (label, w) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) { let _ = w.hide(); }
    }
}

#[tauri::command]
fn selection_cancel(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>) {
    *state.drag.lock().unwrap() = None;
    release_frozen(&state);
    broadcast(&app, None);
    hide_overlays(&app);
}

/// `~/Library/Application Support/BHCapture/settings.json`.
///
/// A private file, not the Keychain (family rule): every new self-signed build looks like a
/// different app to the Keychain, so it would prompt after every update.
fn settings_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    home.join("Library").join("Application Support").join("BHCapture").join("settings.json")
}

/// Push settings that the capture backend owns into it.
fn apply_settings(state: &AppState) {
    let s = state.settings.lock().unwrap();
    state.capturer.set_show_cursor(s.get().include_cursor);
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

    // Prefer the frozen frames: the file then holds exactly what the user saw while selecting.
    // Falls back to a live capture if the freeze had not finished or failed.
    let frozen = state.snapshot.lock().unwrap().clone();
    release_frozen(state);
    let cropped = match &frozen {
        Some(snap) => snap.capture_region_in(rect, snap.layout()),
        None => {
            let layout = state.layout.lock().unwrap().clone();
            state.capturer.capture_region_in(rect, &layout)
        }
    };
    let frame = cropped.map_err(|e| match e {
        CaptureError::PermissionDenied =>
            "Screen Recording is not granted. Allow it in System Settings, then RELAUNCH BHCapture \
             — a running app never sees a new grant.".to_string(),
        other => other.to_string(),
    })?;

    // The name is built from the pattern; `{title}`/`{app}` are untrusted window metadata, so
    // core sanitises them to a single path component (see bhcapture_core::naming).
    let (pattern, out_dir, ask, clip) = {
        let st = state.settings.lock().unwrap();
        let s = st.get();
        (Pattern::parse(&s.pattern).unwrap_or_default(),
         s.output_dir.clone().unwrap_or_else(default_capture_dir),
         s.always_ask_where_to_save,
         s.copy_to_clipboard)
    };
    let stem = pattern.render(local_stamp(), &Fields {
        width: frame.width(), height: frame.height(), ..Default::default()
    });

    let path = if save_as || ask {
        app.dialog().file()
            .set_title("Save capture")
            .set_directory(&out_dir)
            .set_file_name(format!("{stem}.png"))
            .add_filter("PNG image", &["png"])
            .blocking_save_file()
            .ok_or_else(|| "cancelled".to_string())?
            .into_path().map_err(|e| e.to_string())?
    } else {
        let _ = std::fs::create_dir_all(&out_dir);
        unique_path(&out_dir, &stem, "png", &|p| p.exists())
    };

    write_png(&frame, &path)?;

    // Clipboard is a convenience on top of a file that already exists -- never fail the capture
    // for it, just say so.
    if clip {
        if let Err(e) = copy_to_clipboard(&frame) {
            eprintln!("[warn] capture saved but clipboard copy failed: {e}");
        }
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

    let (store, outcome) = SettingsStore::load(settings_path());
    match &outcome {
        LoadOutcome::Loaded { repaired } if !repaired.is_empty() =>
            println!("[settings] repaired: {}", repaired.join("; ")),
        LoadOutcome::Missing => println!("[settings] none yet; using defaults"),
        LoadOutcome::Unreadable(why) => eprintln!(
            "[settings] UNREADABLE ({why}). Running on defaults and REFUSING to save, so the \
             existing file is not destroyed. Fix or move {} and restart.", settings_path().display()),
        _ => {}
    }

    let state = Arc::new(AppState {
        capturer,
        settings: Mutex::new(store),
        layout: Mutex::new(layout),
        drag: Mutex::new(None),
        snapshot: Mutex::new(None),
        cands: Mutex::new(Vec::new()),
        overlay_ids: Mutex::new(Vec::new()),
        epoch: AtomicU64::new(0),
    });
    apply_settings(&state);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            overlay_info, selection_begin, selection_update,
            selection_nudge, selection_cancel, selection_commit,
            snap_at, loupe, selection_set_rect,
            settings_get, settings_save, settings_pick_dir, open_settings
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
            *state.overlay_ids.lock().unwrap() = overlay_ids.clone();
            state.capturer.set_excluded_windows(overlay_ids);

            // Tray: without it there is no way to reach settings, since every window is an
            // overlay that hides itself.
            let cap_i = MenuItem::with_id(app, "capture", "Capture Region", true, Some("Cmd+Shift+Ctrl+4"))?;
            let set_i = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit BHCapture", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&cap_i, &set_i, &quit_i])?;
            let tray_img = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            let tray = TrayIconBuilder::new()
                .icon(tray_img)
                // macOS recolours a template image with the rest of the menu bar; without this
                // the glyph stays black and disappears in dark mode.
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, ev| match ev.id().as_ref() {
                    "capture" => open_selector(app),
                    "settings" => { let _ = open_settings(app.clone()); }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            // Keep the TrayIcon alive: dropping it removes the icon from the menu bar.
            app.manage(tray);

            let manager = GlobalHotKeyManager::new()?;
            let hk = HotKey::new(
                Some(Modifiers::SUPER | Modifiers::SHIFT | Modifiers::CONTROL), Code::Digit4);
            manager.register(hk)?;
            std::mem::forget(manager); // must outlive setup()
            println!("BHCapture ready. Region capture: Cmd+Shift+Ctrl+4");

            let h = handle.clone();
            std::thread::spawn(move || {
                let rx = GlobalHotKeyEvent::receiver();
                while let Ok(ev) = rx.recv() {
                    if ev.state != global_hotkey::HotKeyState::Pressed { continue; }
                    open_selector(&h);
                }
            });
            #[cfg(feature = "demo")]
            {
            if std::env::args().any(|a| a == "--demo-snap") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    // Synthetic POINTER only: the events go into the real overlay webview, so the
                    // real React handlers, IPC and Rust commands all run. Nobody can move a
                    // physical mouse on the harness's behalf.
                    let js = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,\
                         clientX:{x},clientY:{y},button:0}}));}})()");
                    let wait_ready = |st: &Arc<AppState>| {
                        let t0 = std::time::Instant::now();
                        let (mut a, mut b) = (None, None);
                        while t0.elapsed() < std::time::Duration::from_secs(5) {
                            if a.is_none() && st.snapshot.lock().unwrap().is_some() { a = Some(t0.elapsed().as_millis()); }
                            if b.is_none() && !st.cands.lock().unwrap().is_empty() { b = Some(t0.elapsed().as_millis()); }
                            if a.is_some() && b.is_some() { break; }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        println!("[snap] frozen after {a:?} ms, window list after {b:?} ms (from overlay shown)");
                    };
                    let rect_of = |st: &Arc<AppState>| st.drag.lock().unwrap().map(|d| {
                        let r = d.rect(); (r.min_x(), r.min_y(), r.size.w, r.size.h) });

                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    open_selector(&h);
                    wait_ready(&st);
                    let w = h.get_webview_window("overlay-1").expect("overlay-1");

                    // --- hover: the highlight + loupe ---
                    let (hx, hy) = (676.0, 450.0);
                    let _ = w.eval(&js("mousemove", hx, hy));
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    {
                        let layout = st.layout.lock().unwrap().clone();
                        let cands = st.cands.lock().unwrap().clone();
                        println!("[snap] {} candidate windows; snap at ({hx},{hy}) = {:?}",
                            cands.len(), snap_window(&cands, &layout, LogPoint::new(hx, hy)));
                    }
                    println!("[snap] HOLD-HOVER");
                    std::thread::sleep(std::time::Duration::from_millis(5500));

                    // --- click on the highlighted window ---
                    let _ = w.eval(&js("mousedown", hx, hy));
                    let _ = w.eval(&js("mouseup", hx, hy));
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    println!("[snap] after a CLICK, selection = {:?}", rect_of(&st));
                    match commit(&h, &st, false) {
                        Ok(p) => println!("[snap] click-capture saved: {p}"),
                        Err(e) => println!("[snap] click-capture FAILED {e}"),
                    }

                    // --- a drag must stay freeform ---
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    open_selector(&h);
                    wait_ready(&st);
                    let _ = w.eval(&js("mousedown", 200.0, 200.0));
                    for i in 1..=8 {
                        let (x, y) = (200.0 + 237.0 * i as f64 / 8.0, 200.0 + 161.0 * i as f64 / 8.0);
                        let _ = w.eval(&js("mousemove", x, y));
                        std::thread::sleep(std::time::Duration::from_millis(40));
                    }
                    let _ = w.eval(&js("mouseup", 437.0, 361.0));
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    println!("[snap] after a DRAG, selection = {:?}", rect_of(&st));
                    println!("[snap] HOLD-DRAG");
                    std::thread::sleep(std::time::Duration::from_millis(4500));
                    match commit(&h, &st, false) {
                        Ok(p) => println!("[snap] drag-capture saved: {p}"),
                        Err(e) => println!("[snap] drag-capture FAILED {e}"),
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    h.exit(0);
                });
            }

            if std::env::args().any(|a| a == "--settings") {
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    let _ = open_settings(h.clone());
                    std::thread::sleep(std::time::Duration::from_millis(900));
                    if let Some(w) = h.get_webview_window(SETTINGS_WINDOW) {
                        let _ = w.set_focus();
                        #[cfg(target_os = "macos")]
                        if let Ok(ptr) = w.ns_window() {
                            // screencapture -l <id> grabs ONLY this window, so nothing can
                            // overlap it however busy the desktop is.
                            println!("[settings] CGWindowID={}", unsafe { bhcapture_capture::macos::cgwindow_id(ptr) });
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10000));
                    h.exit(0);
                });
            }

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
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running BHCapture");
}
