#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! BHCapture shell: overlay windows, global hotkey, and the region selector's state.
//!
//! The **selection lives in Rust**, not in the webview. Each display gets its own overlay window,
//! and a drag that crosses a display boundary is still one selection — so no single webview can
//! own it. Rust holds the global rectangle and broadcasts it; every overlay renders the part that
//! falls on its own display.

use std::sync::{Arc, Mutex};

mod recording;

use bhcapture_capture::{macos::MacCapturer, CaptureError, Capturer, Snapshot};
use bhcapture_core::geom::{DisplayId, DisplayLayout, LogPoint, LogRect};
use bhcapture_core::image::Frame;
use bhcapture_core::naming::{Fields, Pattern, Stamp, unique_path};
use bhcapture_core::settings::{LoadOutcome, Settings, SettingsStore};
use bhcapture_core::snap::{snap_window, Candidate};
use bhcapture_core::hotkey::{Chord, Key, DEFAULT_RECORD_HOTKEY, DEFAULT_REGION_HOTKEY};
use bhcapture_core::export::{self, ImageFormat, ALL_FORMATS};
use base64::Engine;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri_plugin_dialog::DialogExt;
use global_hotkey::{hotkey::{Code, HotKey, Modifiers}, GlobalHotKeyEvent, GlobalHotKeyManager};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32};
use serde::Serialize;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;

const OVERLAY_PREFIX: &str = "overlay-";

const SETTINGS_WINDOW: &str = "settings";
const EDITOR_PREFIX: &str = "editor-";

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
    /// Captures open in editor windows, by editor id. Nothing is on disk until the user saves.
    editors: Mutex<std::collections::HashMap<u64, EditorDoc>>,
    next_editor: AtomicU64,
}

/// One capture being edited. The pixels stay in memory until the window closes; a save writes the
/// EDITED image, which the editor renders and sends back.
struct EditorDoc {
    frame: Frame,
    saved_to: Option<std::path::PathBuf>,
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
    nudge(&app, &state, dx, dy);
}

fn nudge(app: &tauri::AppHandle, state: &AppState, dx: f64, dy: f64) {
    let mut g = state.drag.lock().unwrap();
    if let Some(d) = g.as_mut() {
        d.cursor = LogPoint::new(d.cursor.x + dx, d.cursor.y + dy);
        let r = d.rect();
        drop(g);
        broadcast(app, Some(r));
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
    // Refuse BEFORE applying anything: an unreadable settings file means saving is refused, and the
    // running app must not quietly adopt values that were never saved (found by the Codex review).
    if !state.settings.lock().unwrap().is_writable() {
        return Err(state.settings.lock().unwrap().save().err().map(|e| e.to_string())
            .unwrap_or_else(|| "settings cannot be saved".into()));
    }
    let previous = state.settings.lock().unwrap().get().clone();
    let mut notes = { state.settings.lock().unwrap().set(next) };

    // The shortcut takes effect NOW (sync commands run on the main thread, where the hotkey API
    // lives). If the OS will not give us the new chord, keep the old one and say so, rather than
    // saving a setting that does nothing.
    let wanted = state.settings.lock().unwrap().get().region_hotkey.clone();
    let mut hotkey_changed = false;
    if wanted != previous.region_hotkey {
        match set_region_hotkey(&wanted) {
            Ok(_) => hotkey_changed = true,
            Err(e) => {
                notes.push(format!("{e}. Kept {}.", previous.region_hotkey));
                let mut st = state.settings.lock().unwrap();
                let mut s = st.get().clone();
                s.region_hotkey = previous.region_hotkey.clone();
                st.set(s);
            }
        }
    }

    let wanted = state.settings.lock().unwrap().get().record_hotkey.clone();
    let mut record_changed = false;
    if wanted != previous.record_hotkey {
        match set_record_hotkey(&wanted) {
            Ok(_) => record_changed = true,
            Err(e) => {
                notes.push(format!("{e}. Kept {}.", previous.record_hotkey));
                let mut st = state.settings.lock().unwrap();
                let mut s = st.get().clone();
                s.record_hotkey = previous.record_hotkey.clone();
                st.set(s);
            }
        }
    }

    // Report a refused save honestly, and roll back everything applied above, so what is running
    // always matches what is on disk.
    if let Err(e) = state.settings.lock().unwrap().save() {
        state.settings.lock().unwrap().set(previous.clone());
        if hotkey_changed { let _ = set_region_hotkey(&previous.region_hotkey); }
        if record_changed { let _ = set_record_hotkey(&previous.record_hotkey); }
        return Err(e.to_string());
    }
    apply_settings(&state);
    let notice = if notes.is_empty() { None } else { Some(notes.join("; ")) };
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

/// Show the Dock icon only while a real window (settings, and later the editor) is open.
///
/// BHCapture lives in the menu bar and should not occupy the Dock while it sits in the background.
/// But while the user is working in a window, a Dock icon and an app menu are what they expect, so
/// the app switches between macOS "accessory" and "regular" modes to match. Overlays do not count:
/// they are a transient tool surface, and a Dock icon bouncing in for every capture would be noise.
///
/// The decision is derived from the windows that actually exist rather than tracked in a flag, so
/// it cannot drift out of step with reality. `closing` names a window that is mid-destroy and must
/// not be counted.
#[cfg(target_os = "macos")]
static DOCK_REGULAR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn sync_dock(app: &tauri::AppHandle, closing: Option<&str>) {
    #[cfg(target_os = "macos")]
    {
        let any_window = app.webview_windows().iter().any(|(label, w)| {
            !label.starts_with(OVERLAY_PREFIX)
                && label != recording::HUD   // the recording bar is a tool surface, like the overlays
                && Some(label.as_str()) != closing
                && w.is_visible().unwrap_or(false)
        });
        let policy = if any_window {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };
        let was_regular = DOCK_REGULAR.swap(any_window, std::sync::atomic::Ordering::SeqCst);
        let _ = app.set_activation_policy(policy);
        if any_window && !was_regular {
            // Without this the File/Edit menus draw but do not open (macOS bug, see the shim).
            let _ = app.run_on_main_thread(bhcapture_capture::macos::refresh_menu_bar);
        }
    }
    #[cfg(not(target_os = "macos"))]
    { let _ = (app, closing); }
}

#[tauri::command]
fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(SETTINGS_WINDOW) {
        let _ = w.show();
        sync_dock(&app, None);
        let _ = w.set_focus();
        return Ok(());
    }
    let w = WebviewWindowBuilder::new(&app, SETTINGS_WINDOW, WebviewUrl::App("index.html".into()))
        .title("BHCapture Settings")
        .inner_size(560.0, 620.0)
        .resizable(true)
        .build().map_err(|e| e.to_string())?;
    // Regular mode first, then focus: an accessory app cannot reliably take focus for a window.
    sync_dock(&app, None);
    let _ = w.set_focus();
    Ok(())
}

/// Release the frozen frames (tens of MB on a Retina panel) and invalidate in-flight work.
fn release_frozen(state: &AppState) {
    state.epoch.fetch_add(1, Ordering::SeqCst);
    *state.snapshot.lock().unwrap() = None;
    state.cands.lock().unwrap().clear();
}

/// The one way the selector opens, so the hotkey, the tray and the tests behave identically.
/// `video` picks what Enter does: open the capture in the editor, or set up a recording.
fn open_selector(app: &tauri::AppHandle, video: bool) {
    recording::VIDEO_MODE.store(video, Ordering::SeqCst);
    let _ = app.emit("selector-mode", if video { "video" } else { "image" });
    let state = app.state::<Arc<AppState>>().inner().clone();
    *state.drag.lock().unwrap() = None;
    release_frozen(&state);
    let epoch = state.epoch.load(Ordering::SeqCst);
    broadcast(app, None);
    show_overlays(app); // appear first; also arms the selector keys, in the same main-thread step
    for (label, w) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) {
            println!("[selector] {label} shown: visible={:?} at {:?} size {:?}",
                w.is_visible(), w.outer_position().ok().map(|p| (p.x, p.y)),
                w.inner_size().ok().map(|s| (s.width, s.height)));
        }
    }

    // Two independent jobs, each discarded if the selector has since closed or reopened.
    let st = state.clone();
    let app_for_freeze = app.clone();
    std::thread::spawn(move || {
        let layout = st.layout.lock().unwrap().clone();
        match Snapshot::take(&st.capturer, &layout) {
            Ok(snap) if st.epoch.load(Ordering::SeqCst) == epoch => {
                *st.snapshot.lock().unwrap() = Some(Arc::new(snap));
                // A pointer that has not moved since the overlay opened would otherwise show no
                // magnifier until its next movement.
                let _ = app_for_freeze.emit("frozen-ready", ());
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
    // Every pointer move lands here, so it doubles as "where is the mouse" for the F key.
    *LAST_POINTER.lock().unwrap() = Some(LogPoint::new(x, y));
    let snap = state.snapshot.lock().unwrap().clone()?;
    let f = snap.loupe(LogPoint::new(x, y), 10)?;
    Some(Loupe {
        size: f.width(),
        scale: f.scale().get(),
        rgba: base64::engine::general_purpose::STANDARD.encode(f.to_rgba8()),
    })
}

/// Last pointer position the selector saw, so F (full screen) picks the display the mouse is on.
static LAST_POINTER: Mutex<Option<LogPoint>> = Mutex::new(None);

/// Full screen in one step: select the whole of one display and go straight on, exactly as if it
/// had been dragged and Enter pressed -- the editor for a screenshot, the control bar for a
/// recording. `None` = the display under the mouse (the F key), else the first display.
fn select_full_display(app: &tauri::AppHandle, display: Option<DisplayId>) {
    let st = app.state::<Arc<AppState>>().inner().clone();
    let bounds = {
        let layout = st.layout.lock().unwrap();
        let d = match display {
            Some(id) => layout.get(id),
            None => LAST_POINTER.lock().unwrap().and_then(|p| layout.display_at(p)),
        }.or_else(|| layout.displays().first());
        match d { Some(d) => d.bounds, None => return }
    };
    let (a, b) = (LogPoint::new(bounds.min_x(), bounds.min_y()), LogPoint::new(bounds.max_x(), bounds.max_y()));
    *st.drag.lock().unwrap() = Some(Drag { anchor: a, cursor: b });
    broadcast(app, Some(bounds));
    run_commit(app, false);
}

/// The selector's Full screen button: the display that overlay sits on.
#[tauri::command]
fn selection_fullscreen(app: tauri::AppHandle, display_id: u32) {
    select_full_display(&app, Some(DisplayId(display_id)));
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
    // Hide and disarm in ONE main-thread step (see `apply_selector_keys`): whenever the selector
    // goes away its keys go with it, so Esc/Enter/arrows work normally again everywhere.
    let a = app.clone();
    let _ = app.run_on_main_thread(move || {
        for (label, w) in a.webview_windows() {
            if label.starts_with(OVERLAY_PREFIX) { let _ = w.hide(); }
        }
        apply_selector_keys(false);
    });
}

#[tauri::command]
fn selection_cancel(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>) {
    cancel_selector(&app, &state);
}

fn cancel_selector(app: &tauri::AppHandle, state: &AppState) {
    *state.drag.lock().unwrap() = None;
    release_frozen(state);
    broadcast(app, None);
    hide_overlays(app);
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
    // Test builds write to a throwaway folder so a test run can NEVER touch the user's real
    // captures in ~/Pictures/BHCapture, and cleaning up after a test is a safe `rm -rf` of a
    // folder that only ever held test output. (Learned the hard way: an earlier cleanup deleted
    // real captures from the default folder.)
    #[cfg(feature = "demo")]
    if let Ok(dir) = std::env::var("BHC_TEST_OUT") {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let dir = home.as_ref()
        .map(|h| h.join("Pictures").join("BHCapture"))
        .unwrap_or_else(std::env::temp_dir);
    if std::fs::create_dir_all(&dir).is_ok() {
        // Our own default folder is private. A folder the user picks keeps whatever mode they gave it.
        #[cfg(unix)]
        { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)); }
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

/// Why a capture did not happen. Serialised to the overlay as `{kind, message}`, so the UI can pick
/// the right treatment: a permission problem needs buttons, a cancelled dialog needs nothing.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
enum CommitError {
    /// Screen Recording is not granted to this app.
    Permission(String),
    /// There is nothing sensible to capture (no selection, or an area outside every screen).
    Empty(String),
    /// The user dismissed the Save dialog. Not a failure, so the UI shows nothing.
    Cancelled(String),
    Failed(String),
}

impl CommitError {
    fn message(&self) -> &str {
        match self { Self::Permission(m) | Self::Empty(m) | Self::Cancelled(m) | Self::Failed(m) => m }
    }
}
impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.message()) }
}

const NOTHING_SELECTED: &str = "Drag to select an area, or click a window, then press Enter.";

/// Enter (or ⇧Enter) in the selector: open the capture in the EDITOR. Nothing is written to disk
/// here (decision 11): the user saves, saves as, or copies from the editor.
#[tauri::command]
async fn selection_commit(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, save_as: bool)
    -> Result<String, CommitError>
{
    commit(&app, &state, save_as)
}

/// Crop the selection and open it in a new editor window. Returns the editor's window label.
///
/// Nothing destructive happens before the crop succeeds: a failure leaves the overlay, the
/// selection and the frozen frame intact, so the user sees the problem where they are.
fn commit(app: &tauri::AppHandle, state: &Arc<AppState>, _save_as: bool) -> Result<String, CommitError> {
    let rect = match *state.drag.lock().unwrap() {
        Some(d) => d.rect(),
        None => return Err(CommitError::Empty(NOTHING_SELECTED.into())),
    };
    if rect.is_empty() {
        return Err(CommitError::Empty(NOTHING_SELECTED.into()));
    }
    if recording::VIDEO_MODE.load(Ordering::SeqCst) {
        return recording::prepare(app, state, rect);
    }

    // Test hook so the permission UI can be exercised without revoking a real grant.
    #[cfg(feature = "demo")]
    if std::env::var("BHC_FORCE_ERROR").as_deref() == Ok("permission") {
        return Err(CommitError::Permission(PERMISSION_MESSAGE.into()));
    }

    // Prefer the frozen frames: the capture then holds exactly what the user saw while selecting.
    // Falls back to a live capture if the freeze had not finished or failed.
    let frozen = state.snapshot.lock().unwrap().clone();
    let cropped = match &frozen {
        Some(snap) => snap.capture_region_in(rect, snap.layout()),
        None => {
            let layout = state.layout.lock().unwrap().clone();
            state.capturer.capture_region_in(rect, &layout)
        }
    };
    let frame = cropped.map_err(|e| match e {
        CaptureError::PermissionDenied => CommitError::Permission(PERMISSION_MESSAGE.into()),
        CaptureError::RegionEmpty | CaptureError::RegionOffScreen =>
            CommitError::Empty("That area is outside every screen. Drag inside a display.".into()),
        other => CommitError::Failed(other.to_string()),
    })?;

    // Cropped. The selector's job is done.
    *state.drag.lock().unwrap() = None;
    release_frozen(state);
    broadcast(app, None);
    hide_overlays(app);

    open_editor(app, state, frame).map_err(CommitError::Failed)
}

/// Where a save goes, and in which format: the default folder with a generated, collision-free name,
/// or a native Save dialog. The webview never supplies a path -- it only says "save" or "save as".
///
/// In Save As the extension the user types wins (`shot.jpg` is a JPEG even if PNG was selected), and a
/// name with no extension we write gets the selected format's.
fn choose_save_path(app: &tauri::AppHandle, state: &AppState, width: u32, height: u32, save_as: bool,
                    format: ImageFormat, parent: Option<&tauri::WebviewWindow>)
                    -> Result<(std::path::PathBuf, ImageFormat), CommitError> {
    // The name is built from the pattern; `{title}`/`{app}` are untrusted window metadata, so
    // core sanitises them to a single path component (see bhcapture_core::naming).
    let (pattern, out_dir, ask) = {
        let st = state.settings.lock().unwrap();
        let s = st.get();
        (Pattern::parse(&s.pattern).unwrap_or_default(),
         s.output_dir.clone().unwrap_or_else(default_capture_dir),
         s.always_ask_where_to_save)
    };
    let stem = pattern.render(local_stamp(), &Fields { width, height, ..Default::default() });

    if save_as || ask {
        let mut d = app.dialog().file()
            .set_title("Save capture")
            .set_directory(&out_dir)
            .set_file_name(format!("{stem}.{}", format.extension()));
        // The selected format first, so it is the dialog's default; every other format after it.
        d = d.add_filter(format.label(), format.extensions());
        for f in ALL_FORMATS.iter().filter(|f| **f != format) {
            d = d.add_filter(f.label(), f.extensions());
        }
        if let Some(w) = parent { d = d.set_parent(w); }
        match d.blocking_save_file().map(|p| p.into_path()) {
            Some(Ok(mut p)) => {
                let chosen = match ImageFormat::from_path(&p) {
                    Some(f) => f,
                    None => {
                        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        p.set_file_name(format!("{name}.{}", format.extension()));
                        format
                    }
                };
                Ok((p, chosen))
            }
            Some(Err(e)) => Err(CommitError::Failed(e.to_string())),
            None => Err(CommitError::Cancelled("Save cancelled.".into())),
        }
    } else {
        let _ = std::fs::create_dir_all(&out_dir);
        Ok((unique_path(&out_dir, &stem, format.extension(), &|p| p.exists()), format))
    }
}

// ---------------------------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------------------------

/// Open `frame` in a new editor window. Windows are created on the main thread; this waits for it.
/// Must not be called ON the main thread (every caller is a command or worker thread).
fn open_editor(app: &tauri::AppHandle, state: &Arc<AppState>, frame: Frame) -> Result<String, String> {
    let id = state.next_editor.fetch_add(1, Ordering::SeqCst) + 1;
    let (w, h, scale) = (frame.width(), frame.height(), frame.scale().get());
    state.editors.lock().unwrap().insert(id, EditorDoc { frame, saved_to: None });

    // Show the capture at 1:1 (in points) when it fits, otherwise fit 85% of the main display.
    let (dw, dh) = state.layout.lock().unwrap().displays().first()
        .map(|d| (d.bounds.size.w, d.bounds.size.h)).unwrap_or((1440.0, 900.0));
    let (iw, ih) = (w as f64 / scale, h as f64 / scale);
    let ww = (iw + 48.0).clamp(820.0_f64.min(dw * 0.85), dw * 0.85);
    let wh = (ih + 128.0).clamp(520.0_f64.min(dh * 0.85), dh * 0.85);

    let label = format!("{EDITOR_PREFIX}{id}");
    let (a, l) = (app.clone(), label.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    let queued = app.run_on_main_thread(move || {
        let r = WebviewWindowBuilder::new(&a, &l, WebviewUrl::App("index.html".into()))
            .title(format!("BHCapture — {w} × {h}"))
            .inner_size(ww, wh)
            .min_inner_size(640.0, 420.0)
            .center()
            .resizable(true)
            .build();
        let _ = tx.send(match r {
            Ok(win) => {
                // A real window: the Dock icon appears (decision 10), and regular mode lets it focus.
                sync_dock(&a, None);
                let _ = win.set_focus();
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        });
    });
    let built = if queued.is_ok() {
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or_else(|_| Err("timed out opening the editor".into()))
    } else {
        Err("could not reach the main thread".into())
    };
    if let Err(e) = built {
        state.editors.lock().unwrap().remove(&id); // do not keep pixels nobody can see
        return Err(format!("Could not open the editor: {e}"));
    }
    println!("[editor] opened {label}: {w}x{h} px");
    Ok(label)
}

fn editor_id_of(label: &str) -> Result<u64, String> {
    label.strip_prefix(EDITOR_PREFIX).and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("{label} is not an editor window"))
}

#[derive(Serialize)]
struct EditorInfo { id: u64, width: u32, height: u32, scale: f64, saved_to: Option<String>, format: ImageFormat }

#[tauri::command]
fn editor_info(window: tauri::Window, state: tauri::State<Arc<AppState>>) -> Result<EditorInfo, String> {
    let id = editor_id_of(window.label())?;
    let format = state.settings.lock().unwrap().get().image_format;
    let docs = state.editors.lock().unwrap();
    let d = docs.get(&id).ok_or("this capture is no longer open")?;
    Ok(EditorInfo { id, width: d.frame.width(), height: d.frame.height(), scale: d.frame.scale().get(),
                    saved_to: d.saved_to.as_ref().map(|p| p.display().to_string()), format })
}

/// The capture's pixels as raw RGBA8, sent as binary rather than a multi-megabyte JSON array.
#[tauri::command]
fn editor_pixels(window: tauri::Window, state: tauri::State<Arc<AppState>>) -> Result<tauri::ipc::Response, String> {
    let id = editor_id_of(window.label())?;
    let docs = state.editors.lock().unwrap();
    let d = docs.get(&id).ok_or("this capture is no longer open")?;
    Ok(tauri::ipc::Response::new(d.frame.to_rgba8()))
}

/// Largest image the editor may hand back. Far above any real screen (an 8K display at 2x is
/// ~530 MP), it only stops a runaway allocation.
const MAX_EDITOR_PNG: usize = 1 << 30;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The editor's rendered image, checked before anything is done with it.
fn png_from_request(request: &tauri::ipc::Request<'_>) -> Result<(Vec<u8>, u32, u32), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected the image as raw PNG bytes".into());
    };
    if bytes.len() > MAX_EDITOR_PNG { return Err("image is too large".into()); }
    if !bytes.starts_with(PNG_SIGNATURE) { return Err("not a PNG image".into()); }
    let info = png::Decoder::new(std::io::Cursor::new(bytes.as_slice())).read_info()
        .map_err(|e| format!("not a valid PNG: {e}"))?;
    let (w, h) = (info.info().width, info.info().height);
    Ok((bytes.clone(), w, h))
}

/// Write a capture readable only by this user (0600). A screenshot can hold anything that was on
/// screen; under a usual 022 umask `fs::write` would make it world-readable (0644) wherever it is
/// saved, so a capture saved to a shared folder would be readable by other accounts (Codex review).
/// The user can still share the file deliberately; it is just never exposed by default.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; o.mode(0o600); }
    let mut f = o.open(path)?;
    f.write_all(bytes)?;
    // An existing file keeps its old mode on open(), so set it explicitly too.
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)); }
    f.sync_all()
}

/// Decode the editor's PNG into straight RGBA8.
fn decode_png_rgba(png: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("unexpected PNG format".into());
    }
    buf.truncate(info.buffer_size());
    Ok((buf, info.width, info.height))
}

fn rgba_to_clipboard(rgba: Vec<u8>, w: u32, h: u32) -> Result<(), String> {
    arboard::Clipboard::new().map_err(|e| e.to_string())?
        .set_image(arboard::ImageData { width: w as usize, height: h as usize, bytes: std::borrow::Cow::Owned(rgba) })
        .map_err(|e| e.to_string())
}

/// Save the EDITED capture. The body is a lossless PNG the editor rendered; headers say
/// `x-save-as: 1` for a dialog and `x-format: jpeg|png|webp|tiff|bmp|gif`. The path is always chosen
/// here, never by the webview, and the file is re-encoded here in the format chosen.
#[tauri::command]
async fn editor_save(app: tauri::AppHandle, window: tauri::WebviewWindow, state: tauri::State<'_, Arc<AppState>>,
                     request: tauri::ipc::Request<'_>) -> Result<String, CommitError> {
    let id = editor_id_of(window.label()).map_err(CommitError::Failed)?;
    let save_as = request.headers().get("x-save-as").map_or(false, |v| v.as_bytes() == b"1");
    let (default_format, quality, clip) = {
        let st = state.settings.lock().unwrap();
        let s = st.get();
        (s.image_format, s.jpeg_quality, s.copy_to_clipboard)
    };
    let wanted = request.headers().get("x-format").and_then(|v| v.to_str().ok())
        .and_then(ImageFormat::parse).unwrap_or(default_format);
    let (png, w, h) = png_from_request(&request).map_err(CommitError::Failed)?;
    if !state.editors.lock().unwrap().contains_key(&id) {
        return Err(CommitError::Failed("this capture is no longer open".into()));
    }

    let (path, format) = choose_save_path(&app, &state, w, h, save_as, wanted, Some(&window))?;
    let (rgba, rw, rh) = decode_png_rgba(&png).map_err(CommitError::Failed)?;
    let bytes = export::encode(&rgba, rw, rh, format, quality).map_err(|e| CommitError::Failed(e.to_string()))?;
    write_private(&path, &bytes).map_err(|e| CommitError::Failed(format!("Could not save the file: {e}")))?;
    if let Some(d) = state.editors.lock().unwrap().get_mut(&id) { d.saved_to = Some(path.clone()); }

    // Clipboard is a convenience on top of a file that already exists -- never fail the save for it.
    if clip {
        if let Err(e) = rgba_to_clipboard(rgba, rw, rh) { eprintln!("[warn] saved but clipboard copy failed: {e}"); }
    }
    println!("[editor] {} saved {w}x{h} as {} ({} bytes) to {}", window.label(), format.label(), bytes.len(), path.display());
    Ok(path.display().to_string())
}

#[tauri::command]
async fn editor_copy(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let (png, _, _) = png_from_request(&request)?;
    let (rgba, w, h) = decode_png_rgba(&png)?;
    rgba_to_clipboard(rgba, w, h)
}

/// Close an editor for good (the page has already asked about unsaved changes).
#[tauri::command]
fn editor_close(window: tauri::WebviewWindow, state: tauri::State<Arc<AppState>>) -> Result<(), String> {
    let id = editor_id_of(window.label())?;
    state.editors.lock().unwrap().remove(&id);
    window.destroy().map_err(|e| e.to_string())
}


const PERMISSION_MESSAGE: &str = "BHCapture is not allowed to record the screen yet. \
    Allow it in System Settings, then relaunch BHCapture. A running app never sees a new grant.";

/// Reports whether an overlay's web view really has keyboard focus. Only the test harness calls it;
/// it exists so focus can be asserted from INSIDE the page rather than inferred from outside.
#[tauri::command]
fn report_overlay_focus(window: tauri::Window, focused: bool) {
    println!("[focus] {} document.hasFocus() = {focused}", window.label());
}

/// Opens System Settings at Screen & System Audio Recording.
#[tauri::command]
fn open_privacy_settings() {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

/// Quit and start again. A newly granted Screen Recording permission only takes effect in a NEW
/// process (verified in Phase 0a), so "allow it, then relaunch" has to be one click, not a chore.
///
/// Goes through LaunchServices (`open`) rather than re-executing the binary, so the new process is
/// a normal app launch. The bundle path is passed as an ARGUMENT to the shell, never interpolated
/// into the command string.
#[tauri::command]
fn relaunch_app(app: tauri::AppHandle) {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bundle) = exe.ancestors().find(|p| p.extension().map_or(false, |e| e == "app")) {
            let _ = std::process::Command::new("/bin/sh")
                .arg("-c").arg("sleep 1.5; /usr/bin/open \"$0\"").arg(bundle).spawn();
            app.exit(0);
            return;
        }
    }
    app.restart(); // unbundled dev run
}

// ---------------------------------------------------------------------------------------------
// Selector keys.
//
// While the selector is open, Esc / Enter / ⇧Enter / the arrows are registered as GLOBAL hotkeys.
//
// Why: opened from a hotkey, the overlay is the key window inside this (inactive) app, its page
// reports document.hasFocus() = true, and our code handles Esc correctly when it is delivered
// (posted straight to our PID it dismisses the overlay at once) -- yet macOS routes real keystrokes
// to the FRONTMOST app (e.g. Chrome), so Esc never arrived until the user clicked. Four activation
// strategies, deferring the focus, and a non-activating panel were all tried. Global hotkeys are
// delivered to this app whichever app is in front, the way the capture shortcut already is.
//
// They exist only while the selector is open and are removed the moment it closes, so Esc and
// Enter work normally everywhere else. They are also removed while the Save dialog is up, or the
// dialog could never receive its own Enter/Esc.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum SelectorKey { Cancel, Save { dialog: bool }, Nudge(f64, f64), FullScreen }

/// `GlobalHotKeyManager` holds Carbon handles that must only be touched on the main thread. It is
/// created there and only ever used inside `run_on_main_thread` closures, which makes this wrapper's
/// `Send`/`Sync` claim true in practice.
struct MainThreadOnly<T>(T);
unsafe impl<T> Send for MainThreadOnly<T> {}
unsafe impl<T> Sync for MainThreadOnly<T> {}

static HOTKEY_MANAGER: Mutex<Option<MainThreadOnly<GlobalHotKeyManager>>> = Mutex::new(None);
static SELECTOR_KEYS_ARMED: AtomicBool = AtomicBool::new(false);

fn selector_keys() -> &'static [(HotKey, SelectorKey)] {
    static KEYS: OnceLock<Vec<(HotKey, SelectorKey)>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut v = Vec::new();
        v.push((HotKey::new(None, Code::Escape), SelectorKey::Cancel));
        // F = full screen. Plain F is only taken while the selector is open, like Esc and Enter.
        v.push((HotKey::new(None, Code::KeyF), SelectorKey::FullScreen));
        for code in [Code::Enter, Code::NumpadEnter] {
            v.push((HotKey::new(None, code), SelectorKey::Save { dialog: false }));
            v.push((HotKey::new(Some(Modifiers::SHIFT), code), SelectorKey::Save { dialog: true }));
        }
        for (code, dx, dy) in [(Code::ArrowLeft, -1.0, 0.0), (Code::ArrowRight, 1.0, 0.0),
                               (Code::ArrowUp, 0.0, -1.0), (Code::ArrowDown, 0.0, 1.0)] {
            v.push((HotKey::new(None, code), SelectorKey::Nudge(dx, dy)));
            v.push((HotKey::new(Some(Modifiers::SHIFT), code), SelectorKey::Nudge(dx * 10.0, dy * 10.0)));
        }
        v
    })
}

/// Arm or disarm on the CURRENT thread, which must be the main thread. Show/hide call this inside
/// the same main-thread step as the window change, so the two can never be reordered: queuing
/// "show" and "arm" separately let a cancel slip between them and leave Esc/Enter captured
/// system-wide behind a hidden selector (found by the Codex review).
fn apply_selector_keys(on: bool) {
    if SELECTOR_KEYS_ARMED.swap(on, Ordering::SeqCst) != on {
        if let Some(m) = HOTKEY_MANAGER.lock().unwrap().as_ref() {
            for (hk, _) in selector_keys() {
                let r = if on { m.0.register(*hk) } else { m.0.unregister(*hk) };
                if let Err(e) = r { eprintln!("[keys] {} {:?} failed: {e}", if on { "register" } else { "unregister" }, hk); }
            }
        }
    }
}

/// Run a capture off the event thread (it crops, writes a file and may open a dialog) and report a
/// failure to the overlay, which is where the user is looking.
fn run_commit(app: &tauri::AppHandle, save_as: bool) {
    let a = app.clone();
    std::thread::spawn(move || {
        let st = a.state::<Arc<AppState>>().inner().clone();
        if let Err(e) = commit(&a, &st, save_as) {
            // A dismissed Save dialog is the user's own choice, not something to announce.
            if !matches!(e, CommitError::Cancelled(_)) { let _ = a.emit("commit-error", e); }
        }
    });
}

fn handle_selector_key(app: &tauri::AppHandle, key: SelectorKey) {
    let st = app.state::<Arc<AppState>>().inner().clone();
    match key {
        SelectorKey::Cancel => cancel_selector(app, &st),
        SelectorKey::Save { dialog } => run_commit(app, dialog),
        SelectorKey::Nudge(dx, dy) => nudge(app, &st, dx, dy),
        SelectorKey::FullScreen => select_full_display(app, None),
    }
}

// ---------------------------------------------------------------------------------------------
// The capture shortcut, from settings.
// ---------------------------------------------------------------------------------------------

/// One user-set global shortcut: its registered id (so the event loop knows which press is which),
/// the chord, and the tray item whose label shows it.
struct HotkeySlot {
    id: AtomicU32,
    key: Mutex<Option<HotKey>>,
    tray: &'static Mutex<Option<MainThreadOnly<MenuItem<tauri::Wry>>>>,
    what: &'static str,
}
/// The tray's "Capture Region" item, so its shortcut label follows the setting.
static TRAY_CAPTURE_ITEM: Mutex<Option<MainThreadOnly<MenuItem<tauri::Wry>>>> = Mutex::new(None);
static REGION_SLOT: HotkeySlot = HotkeySlot { id: AtomicU32::new(0), key: Mutex::new(None), tray: &TRAY_CAPTURE_ITEM, what: "capture" };
static RECORD_SLOT: HotkeySlot = HotkeySlot { id: AtomicU32::new(0), key: Mutex::new(None), tray: &recording::TRAY_RECORD_ITEM, what: "record" };

fn to_platform_hotkey(c: &Chord) -> HotKey {
    let mut m = Modifiers::empty();
    if c.cmd { m |= Modifiers::SUPER; }
    if c.ctrl { m |= Modifiers::CONTROL; }
    if c.alt { m |= Modifiers::ALT; }
    if c.shift { m |= Modifiers::SHIFT; }
    let code = match c.key {
        Key::Digit(d) => [Code::Digit0, Code::Digit1, Code::Digit2, Code::Digit3, Code::Digit4,
                          Code::Digit5, Code::Digit6, Code::Digit7, Code::Digit8, Code::Digit9][d as usize],
        Key::F(n) => [Code::F1, Code::F2, Code::F3, Code::F4, Code::F5, Code::F6,
                      Code::F7, Code::F8, Code::F9, Code::F10, Code::F11, Code::F12][(n - 1) as usize],
        Key::Letter(ch) => [Code::KeyA, Code::KeyB, Code::KeyC, Code::KeyD, Code::KeyE, Code::KeyF,
                            Code::KeyG, Code::KeyH, Code::KeyI, Code::KeyJ, Code::KeyK, Code::KeyL,
                            Code::KeyM, Code::KeyN, Code::KeyO, Code::KeyP, Code::KeyQ, Code::KeyR,
                            Code::KeyS, Code::KeyT, Code::KeyU, Code::KeyV, Code::KeyW, Code::KeyX,
                            Code::KeyY, Code::KeyZ][(ch as u8 - b'A') as usize],
    };
    HotKey::new(if m.is_empty() { None } else { Some(m) }, code)
}

/// Swap the capture shortcut to `spec`. MAIN THREAD ONLY (setup and sync commands run there).
///
/// Registers the new chord before releasing the old one, so a chord another app already holds
/// leaves the user with their previous, working shortcut rather than with none.
fn set_region_hotkey(spec: &str) -> Result<String, String> { set_slot_hotkey(&REGION_SLOT, spec) }
fn set_record_hotkey(spec: &str) -> Result<String, String> { set_slot_hotkey(&RECORD_SLOT, spec) }

fn set_slot_hotkey(slot: &HotkeySlot, spec: &str) -> Result<String, String> {
    let chord = Chord::parse(spec).map_err(|e| e.to_string())?;
    let hk = to_platform_hotkey(&chord);
    let guard = HOTKEY_MANAGER.lock().unwrap();
    let m = guard.as_ref().ok_or("hotkey manager not ready")?;
    let mut current = slot.key.lock().unwrap();
    if current.map(|c| c.id()) == Some(hk.id()) { return Ok(chord.display()); }
    m.0.register(hk).map_err(|e| format!(
        "{} could not be registered (another app may already use it): {e}", chord.display()))?;
    if let Some(old) = current.take() { let _ = m.0.unregister(old); }
    *current = Some(hk);
    slot.id.store(hk.id(), Ordering::SeqCst);
    if let Some(item) = slot.tray.lock().unwrap().as_ref() {
        let _ = item.0.set_accelerator(Some(chord.display().as_str()));
    }
    println!("[hotkey] {} shortcut is {}", slot.what, chord.display());
    Ok(chord.display())
}

// ---------------------------------------------------------------------------------------------
// The menu bar. Shown by macOS only while the app is in regular mode, i.e. while an editor or the
// Settings window is open (decision 10), so it never appears for a quick capture.
// ---------------------------------------------------------------------------------------------

/// The File > Format check items, so their ticks can follow the focused editor.
static FORMAT_ITEMS: Mutex<Vec<MainThreadOnly<(ImageFormat, CheckMenuItem<tauri::Wry>)>>> = Mutex::new(Vec::new());

fn build_app_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let about = PredefinedMenuItem::about(app, Some("About BHCapture"), None)?;
    let settings = MenuItem::with_id(app, "app:settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
    let app_menu = Submenu::with_items(app, "BHCapture", true, &[
        &about, &PredefinedMenuItem::separator(app)?, &settings, &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::hide(app, None)?, &PredefinedMenuItem::hide_others(app, None)?,
        &PredefinedMenuItem::separator(app)?, &PredefinedMenuItem::quit(app, None)?,
    ])?;

    let mut checks = Vec::new();
    let mut format_items: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
    for f in ALL_FORMATS {
        let it = CheckMenuItem::with_id(app, format!("ed:format:{}", f.extension()), f.label(), true,
                                        *f == ImageFormat::Jpeg, None::<&str>)?;
        checks.push(MainThreadOnly((*f, it.clone())));
        format_items.push(it);
    }
    *FORMAT_ITEMS.lock().unwrap() = checks;
    let fmt_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = format_items.iter().map(|i| i as _).collect();
    let format_menu = Submenu::with_items(app, "Format", true, &fmt_refs)?;

    let file = Submenu::with_items(app, "File", true, &[
        &MenuItem::with_id(app, "ed:save", "Save", true, Some("CmdOrCtrl+S"))?,
        &MenuItem::with_id(app, "ed:save-as", "Save As…", true, Some("Shift+CmdOrCtrl+S"))?,
        &PredefinedMenuItem::separator(app)?,
        &format_menu,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "ed:close", "Close", true, Some("CmdOrCtrl+W"))?,
    ])?;
    // Undo/Redo/Copy are OURS (they act on the drawing), but they hand over to the text box when
    // one is being typed into. Cut/Paste/Select All are the system's, for that text box.
    let edit = Submenu::with_items(app, "Edit", true, &[
        &MenuItem::with_id(app, "ed:undo", "Undo", true, Some("CmdOrCtrl+Z"))?,
        &MenuItem::with_id(app, "ed:redo", "Redo", true, Some("Shift+CmdOrCtrl+Z"))?,
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::cut(app, None)?,
        &MenuItem::with_id(app, "ed:copy", "Copy", true, Some("CmdOrCtrl+C"))?,
        &PredefinedMenuItem::paste(app, None)?,
        &PredefinedMenuItem::select_all(app, None)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "ed:delete", "Delete Selected", true, None::<&str>)?,
    ])?;
    let window = Submenu::with_items(app, "Window", true, &[
        &PredefinedMenuItem::minimize(app, None)?,
        &PredefinedMenuItem::fullscreen(app, None)?,
    ])?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &window])
}

/// Send a menu action to the editor the user is working in. Nothing happens if no editor has focus
/// (for example while Settings is in front), rather than acting on a window the user cannot see.
fn route_menu_event(app: &tauri::AppHandle, id: &str) {
    if id == "app:settings" { let _ = open_settings(app.clone()); return; }
    let Some(action) = id.strip_prefix("ed:") else { return };
    let focused = app.webview_windows().into_iter()
        .find(|(l, w)| (l.starts_with(EDITOR_PREFIX) || l.starts_with(recording::VIDEO_PREFIX))
                     && w.is_focused().unwrap_or(false));
    if let Some((label, _)) = focused {
        let _ = app.emit_to(label.as_str(), "editor-menu", action.to_string());
    }
}

/// The focused editor reports its format so File > Format ticks the right one. Sync command, so it
/// runs on the main thread, where menu items must be touched.
#[tauri::command]
fn editor_format_changed(format: ImageFormat) {
    for item in FORMAT_ITEMS.lock().unwrap().iter() {
        let _ = item.0 .1.set_checked(item.0 .0 == format);
    }
}

fn overlay_label(id: DisplayId) -> String { format!("{OVERLAY_PREFIX}{}", id.0) }

/// Make the overlay windows match the current display layout: create what is missing, move and
/// resize what exists, destroy what no longer has a display. The ONE path used both at launch and
/// after a display change, so what the resolution-change test exercises is what startup runs.
///
/// Must run on the main thread: creating a webview window from another thread can deadlock.
fn sync_overlays(app: &tauri::AppHandle, state: &Arc<AppState>) -> tauri::Result<()> {
    let displays = state.layout.lock().unwrap().displays().to_vec();
    let wanted: Vec<String> = displays.iter().map(|d| overlay_label(d.id)).collect();

    for (label, w) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) && !wanted.contains(&label) {
            println!("[display] removing {label}: its display is gone");
            let _ = w.destroy();
        }
    }

    let mut ids = Vec::new();
    for d in &displays {
        let label = overlay_label(d.id);
        let w = match app.get_webview_window(&label) {
            Some(w) => {
                // Size FIRST, then position. macOS keeps a window's BOTTOM edge fixed when it is resized,
                // so shrinking a screen from 878 to 800 pt pushed the overlay's top edge 78 pt down and
                // left a strip uncovered; placing it afterwards puts the top-left back where it belongs.
                let _ = w.set_size(tauri::LogicalSize::new(d.bounds.size.w, d.bounds.size.h));
                let _ = w.set_position(tauri::LogicalPosition::new(d.bounds.min_x(), d.bounds.min_y()));
                w
            }
            None => WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
                .position(d.bounds.min_x(), d.bounds.min_y())
                .inner_size(d.bounds.size.w, d.bounds.size.h)
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .visible(false)
                .resizable(false)
                .shadow(false)
                .title("BHCapture selector")
                .build()?,
        };
        println!("[display] {label}: {}x{} pt at ({},{}) scale {:.2}",
            d.bounds.size.w, d.bounds.size.h, d.bounds.min_x(), d.bounds.min_y(), d.scale.get());

        #[cfg(target_os = "macos")]
        if let Ok(ptr) = w.ns_window() {
            // Make it a non-activating panel so it can take keystrokes from a global hotkey.
            // Idempotent, and it refuses (returns -1) rather than risk a memory overrun.
            let r = unsafe { bhcapture_capture::macos::make_overlay_panel(ptr) };
            if r != 1 { eprintln!("[panel] {label}: could not convert to a panel (code {r}); keyboard focus may need a click"); }

            // Register the overlay with the capturer so it is excluded from captures.
            // Without this the selector photographs its own scrim.
            let id = unsafe { bhcapture_capture::macos::cgwindow_id(ptr) };
            if id != 0 { ids.push(id); }
        }
    }
    *state.overlay_ids.lock().unwrap() = ids.clone();
    state.capturer.set_excluded_windows(ids);
    Ok(())
}

/// Re-read the displays and, if anything changed, rebuild the overlays to match.
fn refresh_displays(app: &tauri::AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let new = match state.capturer.list_displays() {
        Ok(l) => l,
        // Keep the old layout rather than replacing it with nothing: a stale layout is a visible
        // problem that can be fixed, an empty one disables the selector entirely.
        Err(e) => { eprintln!("[display] could not re-read displays ({e}); keeping the old layout"); return; }
    };
    {
        let old = state.layout.lock().unwrap();
        if old.displays() == new.displays() { return; }
        println!("[display] layout changed: {} -> {} display(s)", old.displays().len(), new.displays().len());
    }

    // A selector that is open was drawn from the OLD layout, so its overlays and its frozen frame
    // no longer describe the screen. Dismiss it; the user can press the hotkey again.
    *state.drag.lock().unwrap() = None;
    release_frozen(&state);
    broadcast(app, None);
    hide_overlays(app);
    *state.layout.lock().unwrap() = new;

    let (a, st) = (app.clone(), state.clone());
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = sync_overlays(&a, &st) {
            eprintln!("[display] could not rebuild the overlays: {e}");
        }
        // Overlays that already exist must refetch their origin/size/scale.
        let _ = a.emit("layout-changed", ());
    });
}

/// The OS reports one physical change as several callbacks, so wait for them to stop before
/// acting. Reading mid-change would see a half-applied layout.
fn on_displays_changed(app: &tauri::AppHandle) {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let mine = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if GENERATION.load(Ordering::SeqCst) == mine { refresh_displays(&app); }
    });
}

fn show_overlays(app: &tauri::AppHandle) {
    // The overlays are non-activating panels (see `bhc_make_overlay_panel`): the key window inside
    // this app without the app becoming active, so no Dock icon and no focus fight with the app in
    // front. Showing and focusing happen together on the main thread, window first and then its web
    // view (`WebviewWindow::set_focus` only focuses the WINDOW).
    //
    // NOTE this does NOT make real keystrokes arrive: macOS still routes them to the frontmost app
    // when the overlay is opened from a hotkey. That is what the selector's global keys are for.
    let a = app.clone();
    let _ = app.run_on_main_thread(move || {
        for (label, w) in a.webview_windows() {
            if label.starts_with(OVERLAY_PREFIX) {
                let _ = w.show();
                let _ = w.set_focus();
                let webview: &tauri::Webview = w.as_ref();
                let _ = webview.set_focus();
            }
        }
        apply_selector_keys(true);
    });
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
        editors: Mutex::new(std::collections::HashMap::new()),
        next_editor: AtomicU64::new(0),
    });
    apply_settings(&state);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state.clone())
        // The moment the last real window goes, the Dock icon goes with it.
        .on_menu_event(|app, event| route_menu_event(app, event.id().as_ref()))
        .on_window_event(|window, event| {
            match event {
                // An editor holds a capture that may not be saved anywhere yet. Closing it with the
                // red button must ASK, which the page does (its own dialog: window.confirm is a no-op
                // that returns true in a Tauri webview). The page then calls editor_close.
                tauri::WindowEvent::CloseRequested { api, .. }
                    if window.label().starts_with(EDITOR_PREFIX) || window.label().starts_with(recording::VIDEO_PREFIX) => {
                    api.prevent_close();
                    let _ = window.emit_to(window.label(), "editor-close-requested", ());
                }
                tauri::WindowEvent::Destroyed => {
                    // Free the pixels the moment their window is gone.
                    if let Ok(id) = recording::video_id_of(window.label()) { recording::forget_video(id); }
                    if let Ok(id) = editor_id_of(window.label()) {
                        if let Some(st) = window.app_handle().try_state::<Arc<AppState>>() {
                            st.editors.lock().unwrap().remove(&id);
                        }
                    }
                    sync_dock(window.app_handle(), Some(window.label()));
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            overlay_info, selection_begin, selection_update,
            selection_nudge, selection_cancel, selection_commit,
            snap_at, loupe, selection_set_rect, selection_fullscreen, open_privacy_settings, relaunch_app, report_overlay_focus,
            settings_get, settings_save, settings_pick_dir, open_settings,
            editor_info, editor_pixels, editor_save, editor_copy, editor_close, editor_format_changed,
            recording::recorder_info, recording::recorder_option, recording::recorder_start,
            recording::recorder_pause, recording::recorder_stop, recording::recorder_cancel, recording::recorder_hide, recording::recorder_restart,
            recording::video_info, recording::video_save, recording::video_close
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            // Menu-bar only until a window is opened. Info.plist's LSUIElement does this for the
            // bundled app; stating it here makes an unbundled dev run behave the same.
            sync_dock(&handle, None);
            // The menu bar for editor/Settings windows. macOS shows it only in regular mode.
            match build_app_menu(&handle) {
                Ok(m) => { let _ = app.set_menu(m); }
                Err(e) => eprintln!("[menu] could not build the menu bar: {e}"),
            }
            // Pre-create every overlay hidden AT LAUNCH. Creating a webview window on
            // hotkey-press is visibly slow; spike 0c measured ~25 ms for a pre-created one.
            sync_overlays(&handle, &state)?;

            // Keep the overlays matched to the real displays for the life of the process.
            let h_watch = handle.clone();
            bhcapture_capture::macos::watch_displays(move || on_displays_changed(&h_watch));

            // Tray: without it there is no way to reach settings, since every window is an
            // overlay that hides itself.
            let cap_i = MenuItem::with_id(app, "capture", "Capture Region", true, None::<&str>)?;
            let rec_i = MenuItem::with_id(app, "record", "Record Video", true, None::<&str>)?;
            // Only meaningful while recording; brings back (or hides) the control bar.
            let pause_i = MenuItem::with_id(app, "pause", "Pause Recording", false, Some(recording::PAUSE_SHORTCUT))?;
            let bar_i = MenuItem::with_id(app, "toggle-bar", "Hide Recording Bar", false, None::<&str>)?;
            let set_i = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit BHCapture", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&cap_i, &rec_i, &pause_i, &bar_i, &PredefinedMenuItem::separator(app)?, &set_i, &quit_i])?;
            *TRAY_CAPTURE_ITEM.lock().unwrap() = Some(MainThreadOnly(cap_i.clone()));
            *recording::TRAY_RECORD_ITEM.lock().unwrap() = Some(MainThreadOnly(rec_i.clone()));
            *recording::TRAY_BAR_ITEM.lock().unwrap() = Some(MainThreadOnly(bar_i.clone()));
            *recording::TRAY_PAUSE_ITEM.lock().unwrap() = Some(MainThreadOnly(pause_i.clone()));
            let tray_img = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            let tray = TrayIconBuilder::new()
                .icon(tray_img)
                // macOS recolours a template image with the rest of the menu bar; without this
                // the glyph stays black and disappears in dark mode.
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, ev| match ev.id().as_ref() {
                    "capture" => open_selector(app, false),
                    "record" => recording::record_shortcut(app),
                    "toggle-bar" => recording::toggle_bar(app),
                    "pause" => recording::toggle_pause(app),
                    "settings" => { let _ = open_settings(app.clone()); }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            // Keep the TrayIcon alive: dropping it removes the icon from the menu bar.
            app.manage(tray);

            // Kept (not leaked) so shortcuts can be changed and the selector's keys armed later,
            // always from the main thread; dropping it would unregister everything.
            *HOTKEY_MANAGER.lock().unwrap() = Some(MainThreadOnly(GlobalHotKeyManager::new()?));
            let wanted = state.settings.lock().unwrap().get().region_hotkey.clone();
            if let Err(e) = set_region_hotkey(&wanted) {
                eprintln!("[hotkey] {e}; falling back to {DEFAULT_REGION_HOTKEY}");
                if let Err(e2) = set_region_hotkey(DEFAULT_REGION_HOTKEY) {
                    eprintln!("[hotkey] the default could not be registered either: {e2}");
                }
            }
            let wanted = state.settings.lock().unwrap().get().record_hotkey.clone();
            if let Err(e) = set_record_hotkey(&wanted) {
                eprintln!("[hotkey] {e}; falling back to {DEFAULT_RECORD_HOTKEY}");
                if let Err(e2) = set_record_hotkey(DEFAULT_RECORD_HOTKEY) {
                    eprintln!("[hotkey] the default record shortcut could not be registered either: {e2}");
                }
            }
            // Recordings play from the private working folder, and ONLY that folder is exposed to
            // the web views' asset protocol (not $HOME, not the save folder).
            if let Err(e) = app.asset_protocol_scope().allow_directory(recording::recordings_dir(), false) {
                eprintln!("[record] could not allow playback from the recordings folder: {e}");
            }
            // A recording left over from a crash or a quit is reopened rather than silently lost.
            let h_left = handle.clone();
            std::thread::spawn(move || recording::reopen_leftovers(&h_left));
            println!("BHCapture ready.");

            let h = handle.clone();
            std::thread::spawn(move || {
                let rx = GlobalHotKeyEvent::receiver();
                while let Ok(ev) = rx.recv() {
                    if ev.state != global_hotkey::HotKeyState::Pressed { continue; }
                    if ev.id == REGION_SLOT.id.load(Ordering::SeqCst) {
                        println!("[hotkey] pressed");
                        // While a recording is set up or running, the capture shortcut must not open
                        // a second selector over it.
                        if !recording::busy() { open_selector(&h, false); }
                    } else if ev.id == RECORD_SLOT.id.load(Ordering::SeqCst) {
                        println!("[hotkey] record pressed");
                        recording::record_shortcut(&h);
                    } else if ev.id != 0 && ev.id == recording::PAUSE_HOTKEY_ID.load(Ordering::SeqCst) {
                        println!("[hotkey] pause pressed");
                        recording::toggle_pause(&h);
                    } else if let Some((_, key)) = selector_keys().iter().find(|(k, _)| k.id() == ev.id) {
                        handle_selector_key(&h, *key);
                    }
                }
            });
            #[cfg(feature = "demo")]
            {
            if std::env::args().any(|a| a == "--demo-keys") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    let js = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,\
                         clientX:{x},clientY:{y},button:0}}));}})()");
                    let rect = |st: &Arc<AppState>| st.drag.lock().unwrap().map(|d| {
                        let r = d.rect(); (r.min_x(), r.min_y(), r.size.w, r.size.h) });
                    ms(5000); // the harness puts another app in front meanwhile
                    // Open the way a hotkey does, so the keys are armed exactly as in real use.
                    open_selector(&h, false);
                    ms(1000);
                    let w = h.get_webview_window("overlay-1").expect("overlay-1");
                    let _ = w.eval(&js("mousedown", 300.0, 300.0));
                    for i in 1..=5 { let _ = w.eval(&js("mousemove", 300.0 + 40.0 * i as f64, 300.0 + 25.0 * i as f64)); ms(40); }
                    let _ = w.eval(&js("mouseup", 500.0, 425.0));
                    ms(400);
                    println!("[keys] selection {:?}; armed = {}", rect(&st), SELECTOR_KEYS_ARMED.load(Ordering::SeqCst));
                    println!("[keys] HOLD-1");          // harness sends: Right x3
                    ms(2500);
                    println!("[keys] after RIGHT x3: {:?}", rect(&st));
                    println!("[keys] HOLD-2");          // harness sends: Shift+Down
                    ms(2000);
                    println!("[keys] after SHIFT+DOWN: {:?}", rect(&st));
                    println!("[keys] HOLD-3");          // harness sends: Enter -> saves
                    ms(3500);
                    println!("[keys] after ENTER: selection cleared = {}, overlay visible = {}, armed = {}",
                        st.drag.lock().unwrap().is_none(), w.is_visible().unwrap_or(true),
                        SELECTOR_KEYS_ARMED.load(Ordering::SeqCst));
                    // A second session, dismissed with Esc, to prove the keys are released again.
                    open_selector(&h, false);
                    ms(800);
                    println!("[keys] reopened; armed = {}", SELECTOR_KEYS_ARMED.load(Ordering::SeqCst));
                    println!("[keys] HOLD-4");          // harness sends: Esc
                    ms(2500);
                    println!("[keys] after ESC: overlay visible = {}, armed = {}",
                        w.is_visible().unwrap_or(true), SELECTOR_KEYS_ARMED.load(Ordering::SeqCst));
                    ms(300);
                    h.exit(0);
                });
            }

            if std::env::args().any(|a| a == "--demo-record") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(2500));
                    recording::demo_record(&h, &st, 5);
                });
            }

            if std::env::args().any(|a| a == "--demo-editor") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    let js = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,\
                         clientX:{x},clientY:{y},button:0}}));}})()");
                    ms(2500);
                    open_selector(&h, false);
                    ms(1000);
                    let ov = h.get_webview_window("overlay-1").expect("overlay-1");
                    let _ = ov.eval(&js("mousedown", 200.0, 150.0));
                    for i in 1..=6 { let _ = ov.eval(&js("mousemove", 200.0 + 100.0 * i as f64, 150.0 + 60.0 * i as f64)); ms(40); }
                    let _ = ov.eval(&js("mouseup", 800.0, 510.0));
                    ms(400);
                    // Enter in the selector: must open the EDITOR, not save anything.
                    let label = match commit(&h, &st, false) {
                        Ok(l) => l, Err(e) => { println!("[edtest] commit FAILED {e}"); h.exit(1); return; }
                    };
                    println!("[edtest] Enter opened {label}; files saved so far = {}", std::fs::read_dir(default_capture_dir()).map(|d| d.count()).unwrap_or(0));
                    ms(1800);
                    let ed = h.get_webview_window(&label).expect("editor window");
                    #[cfg(target_os = "macos")]
                    if let Ok(ptr) = ed.ns_window() {
                        println!("[edtest] editor CGWindowID={}", unsafe { bhcapture_capture::macos::cgwindow_id(ptr) });
                    }
                    // Draw an arrow and a box on the editor canvas with real pointer events.
                    // Switch tool, let React apply it, THEN drag -- as a person does. Doing both in one
                    // instant tests nothing a user can do and draws with the previous tool.
                    let draw = |tool: &str, fx1: f64, fy1: f64, fx2: f64, fy2: f64| {
                        let _ = ed.eval(&format!("window.dispatchEvent(new KeyboardEvent('keydown',{{key:'{tool}',bubbles:true}}))"));
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        format!(r#"(()=>{{
                        const c=document.querySelector('canvas.ed-canvas'); const r=c.getBoundingClientRect();
                        const at=(fx,fy)=>({{clientX:r.left+r.width*fx, clientY:r.top+r.height*fy, bubbles:true, pointerId:1, button:0}});
                        c.dispatchEvent(new PointerEvent('pointerdown',at({fx1},{fy1})));
                        for(let i=1;i<=8;i++) c.dispatchEvent(new PointerEvent('pointermove',at({fx1}+({fx2}-{fx1})*i/8,{fy1}+({fy2}-{fy1})*i/8)));
                        c.dispatchEvent(new PointerEvent('pointerup',at({fx2},{fy2})));
                    }})()"#)
                    };
                    let _ = ed.eval(&draw("a", 0.15, 0.80, 0.55, 0.30));
                    ms(300);
                    let _ = ed.eval(&draw("r", 0.60, 0.15, 0.92, 0.60));
                    ms(300);
                    let _ = ed.eval(&draw("p", 0.42, 0.0, 0.98, 0.22));
                    ms(500);
                    println!("[edtest] HOLD-EDITOR");
                    ms(3000);
                    // From here the HARNESS sends real keystrokes (⌘W, Esc, ⌘S), so they go through the
                    // native menu bar exactly as a person's would.
                    println!("[edtest] HOLD-CLOSE1"); ms(2500);
                    println!("[edtest] after ⌘W on an unsaved capture: editor still open = {} (expect true: it must ask)",
                        h.get_webview_window(&label).is_some());
                    println!("[edtest] HOLD-ESC"); ms(2000);
                    println!("[edtest] after Esc on the dialog: editor still open = {}, capture held = {}",
                        h.get_webview_window(&label).is_some(), st.editors.lock().unwrap().len() == 1);
                    println!("[edtest] HOLD-SAVE"); ms(2500);
                    route_menu_event(&h, "ed:format:png");   // File > Format > PNG, through the same router
                    ms(600);
                    println!("[edtest] HOLD-SAVE2"); ms(2500);
                    println!("[edtest] HOLD-CLOSE2"); ms(2500);
                    println!("[edtest] after ⌘W: editor window open = {}, captures held in memory = {}",
                        h.get_webview_window(&label).is_some(), st.editors.lock().unwrap().len());
                    ms(300);
                    h.exit(0);
                });
            }

            if std::env::args().any(|a| a == "--demo-focus") {
                let h = handle.clone();
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    // Wait: the harness brings ANOTHER app to the front, which is exactly the state a
                    // hotkey press starts from.
                    ms(5000);
                    open_selector(&h, false);
                    ms(900);
                    let w = h.get_webview_window("overlay-1").expect("overlay-1");
                    println!("[focus] selector opened; asking the page itself whether it has focus");
                    let _ = w.eval("window.__TAURI_INTERNALS__.invoke('report_overlay_focus',{focused:document.hasFocus()})");
                    ms(4500); // the harness sends Esc during this window
                    println!("[focus] 4.5 s later: overlay visible = {:?}", w.is_visible());
                    ms(300);
                    h.exit(0);
                });
            }

            if std::env::args().any(|a| a == "--demo-notice") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    let js = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,\
                         clientX:{x},clientY:{y},button:0}}));}})()");
                    let enter = "window.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}))";
                    ms(1500);
                    open_selector(&h, false);
                    ms(1200);
                    let w = h.get_webview_window("overlay-1").expect("overlay-1");
                    println!("[notice] visible 1.2s after open_selector = {:?}", w.is_visible());

                    // 1) Enter with nothing selected.
                    let _ = w.eval(enter);
                    ms(700);
                    println!("[notice] visible after the empty Enter = {:?}", w.is_visible());
                    println!("[notice] HOLD-EMPTY");
                    ms(4500);

                    // 2) A real selection, then a capture that fails with a permission error.
                    let _ = w.eval(&js("mousedown", 300.0, 300.0));
                    for i in 1..=6 {
                        let _ = w.eval(&js("mousemove", 300.0 + 50.0 * i as f64, 300.0 + 33.0 * i as f64));
                        ms(40);
                    }
                    let _ = w.eval(&js("mouseup", 600.0, 498.0));
                    ms(400);
                    std::env::set_var("BHC_FORCE_ERROR", "permission");
                    let _ = w.eval(enter);
                    ms(900);
                    // The point of the restructure: a failure must destroy nothing.
                    println!("[notice] after the FAILED capture: selection kept = {}, frozen frame kept = {}, overlay still visible = {}",
                        st.drag.lock().unwrap().is_some(),
                        st.snapshot.lock().unwrap().is_some(),
                        w.is_visible().unwrap_or(false));
                    println!("[notice] HOLD-PERMISSION");
                    ms(5000);

                    // 3) Clear the forced failure: the SAME selection must now save.
                    std::env::remove_var("BHC_FORCE_ERROR");
                    match commit(&h, &st, false) {
                        Ok(p) => println!("[notice] retry saved: {p}"),
                        Err(e) => println!("[notice] retry FAILED: {e}"),
                    }
                    println!("[notice] after SUCCESS: selection cleared = {}, overlay hidden = {}",
                        st.drag.lock().unwrap().is_none(), !w.is_visible().unwrap_or(true));
                    ms(500);
                    h.exit(0);
                });
            }

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
                    open_selector(&h, false);
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
                    open_selector(&h, false);
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
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    println!("[dock] settings window OPEN");
                    ms(4000);
                    if let Some(w) = h.get_webview_window(SETTINGS_WINDOW) { let _ = w.close(); }
                    println!("[dock] settings window CLOSED");
                    ms(3500);
                    let _ = open_settings(h.clone());
                    println!("[dock] settings window REOPENED");
                    ms(3500);
                    if let Some(w) = h.get_webview_window(SETTINGS_WINDOW) { let _ = w.close(); }
                    println!("[dock] settings window CLOSED again");
                    ms(3000);
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
        .build(tauri::generate_context!())
        .expect("error while building BHCapture")
        .run(|_app, event| {
            // Quitting mid-recording finishes the file, so it is playable and offered next launch.
            if let tauri::RunEvent::Exit = event { recording::finish_on_exit(); }
        });
}
