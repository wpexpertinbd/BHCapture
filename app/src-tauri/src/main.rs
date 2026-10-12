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
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
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
fn sync_dock(app: &tauri::AppHandle, closing: Option<&str>) {
    #[cfg(target_os = "macos")]
    {
        let any_window = app.webview_windows().iter().any(|(label, w)| {
            !label.starts_with(OVERLAY_PREFIX)
                && Some(label.as_str()) != closing
                && w.is_visible().unwrap_or(false)
        });
        let policy = if any_window {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };
        let _ = app.set_activation_policy(policy);
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
fn open_selector(app: &tauri::AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    *state.drag.lock().unwrap() = None;
    release_frozen(&state);
    let epoch = state.epoch.load(Ordering::SeqCst);
    broadcast(app, None);
    show_overlays(app); // appear first: the freeze happens behind an already-visible overlay
    set_selector_keys(app, true, false);
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
    // Whenever the selector goes away its keys go with it, so Esc/Enter work normally again.
    set_selector_keys(app, false, false);
    for (label, w) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) { let _ = w.hide(); }
    }
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

/// `save_as` opens a native dialog so this one capture can go anywhere; otherwise it lands in the
/// default folder. The default is a default, not a cage.
#[tauri::command]
async fn selection_commit(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, save_as: bool)
    -> Result<String, CommitError>
{
    commit(&app, &state, save_as)
}

/// Nothing destructive happens until the file is safely written. A failure at ANY earlier step
/// leaves the overlay, the selection and the frozen frame intact, so the user sees the problem
/// where they are and can retry or adjust, instead of losing a selection they spent time framing.
fn commit(app: &tauri::AppHandle, state: &Arc<AppState>, save_as: bool) -> Result<String, CommitError> {
    let rect = match *state.drag.lock().unwrap() {
        Some(d) => d.rect(),
        None => return Err(CommitError::Empty(NOTHING_SELECTED.into())),
    };
    if rect.is_empty() {
        return Err(CommitError::Empty(NOTHING_SELECTED.into()));
    }

    // Test hook so the permission UI can be exercised without revoking a real grant.
    #[cfg(feature = "demo")]
    if std::env::var("BHC_FORCE_ERROR").as_deref() == Ok("permission") {
        return Err(CommitError::Permission(PERMISSION_MESSAGE.into()));
    }

    // Prefer the frozen frames: the file then holds exactly what the user saw while selecting.
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

    let asking = save_as || ask;
    let path = if asking {
        // The selector deliberately STAYS UP behind the Save dialog. Hiding it first was the plan,
        // but the overlay came back within ~250 ms of the dialog opening regardless of what was
        // tried (with and without the panel conversion), so rather than fight AppKit the design
        // uses it: the user keeps seeing exactly what they are saving, a cancelled dialog needs no
        // restore step, and the dialog (a modal panel) sits above the overlay.
        // The selector's global Enter/Esc must NOT be live while the dialog is up, or the dialog could
        // never receive its own. Waited for: a merely queued disarm would race the dialog opening.
        set_selector_keys(app, false, true);
        let picked = app.dialog().file()
            .set_title("Save capture")
            .set_directory(&out_dir)
            .set_file_name(format!("{stem}.png"))
            .add_filter("PNG image", &["png"])
            .blocking_save_file();
        set_selector_keys(app, true, false); // the selector is still up behind it
        match picked.map(|p| p.into_path()) {
            Some(Ok(p)) => p,
            Some(Err(e)) => return Err(CommitError::Failed(e.to_string())),
            None => return Err(CommitError::Cancelled("Save cancelled. Your selection is still here.".into())),
        }
    } else {
        let _ = std::fs::create_dir_all(&out_dir);
        unique_path(&out_dir, &stem, "png", &|p| p.exists())
    };

    if let Err(e) = write_png(&frame, &path) {
        return Err(CommitError::Failed(format!("Could not save the file: {e}")));
    }

    // Written. Only now is it safe to tear the selector down.
    *state.drag.lock().unwrap() = None;
    release_frozen(state);
    broadcast(app, None);
    hide_overlays(app);

    // Clipboard is a convenience on top of a file that already exists -- never fail the capture
    // for it, just say so.
    if clip {
        if let Err(e) = copy_to_clipboard(&frame) {
            eprintln!("[warn] capture saved but clipboard copy failed: {e}");
        }
    }
    Ok(path.display().to_string())
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
enum SelectorKey { Cancel, Save { dialog: bool }, Nudge(f64, f64) }

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

/// Arm or disarm the selector's keys. Idempotent. With `wait`, returns only once it has happened
/// (bounded), which the Save dialog needs: its own Enter/Esc must not be swallowed.
fn set_selector_keys(app: &tauri::AppHandle, on: bool, wait: bool) {
    let (tx, rx) = std::sync::mpsc::channel();
    let queued = app.run_on_main_thread(move || {
        if SELECTOR_KEYS_ARMED.swap(on, Ordering::SeqCst) != on {
            if let Some(m) = HOTKEY_MANAGER.lock().unwrap().as_ref() {
                for (hk, _) in selector_keys() {
                    let r = if on { m.0.register(*hk) } else { m.0.unregister(*hk) };
                    if let Err(e) = r { eprintln!("[keys] {} {:?} failed: {e}", if on { "register" } else { "unregister" }, hk); }
                }
            }
        }
        let _ = tx.send(());
    });
    if wait && queued.is_ok() { let _ = rx.recv_timeout(std::time::Duration::from_millis(1500)); }
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
    });
    apply_settings(&state);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state.clone())
        // The moment the last real window goes, the Dock icon goes with it.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                sync_dock(window.app_handle(), Some(window.label()));
            }
        })
        .invoke_handler(tauri::generate_handler![
            overlay_info, selection_begin, selection_update,
            selection_nudge, selection_cancel, selection_commit,
            snap_at, loupe, selection_set_rect, open_privacy_settings, relaunch_app, report_overlay_focus,
            settings_get, settings_save, settings_pick_dir, open_settings
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            // Menu-bar only until a window is opened. Info.plist's LSUIElement does this for the
            // bundled app; stating it here makes an unbundled dev run behave the same.
            sync_dock(&handle, None);
            // Pre-create every overlay hidden AT LAUNCH. Creating a webview window on
            // hotkey-press is visibly slow; spike 0c measured ~25 ms for a pre-created one.
            sync_overlays(&handle, &state)?;

            // Keep the overlays matched to the real displays for the life of the process.
            let h_watch = handle.clone();
            bhcapture_capture::macos::watch_displays(move || on_displays_changed(&h_watch));

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
            let region_id = hk.id();
            // Kept (not leaked) so the selector's keys can be armed and disarmed later, always from
            // the main thread; dropping it would unregister the capture shortcut.
            *HOTKEY_MANAGER.lock().unwrap() = Some(MainThreadOnly(manager));
            println!("BHCapture ready. Region capture: Cmd+Shift+Ctrl+4");

            let h = handle.clone();
            std::thread::spawn(move || {
                let rx = GlobalHotKeyEvent::receiver();
                while let Ok(ev) = rx.recv() {
                    if ev.state != global_hotkey::HotKeyState::Pressed { continue; }
                    if ev.id == region_id {
                        println!("[hotkey] pressed");
                        open_selector(&h);
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
                    open_selector(&h);
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
                    open_selector(&h);
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

            if std::env::args().any(|a| a == "--demo-saveas") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    let js = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,\
                         clientX:{x},clientY:{y},button:0}}));}})()");
                    ms(5000); // the harness puts another app in front meanwhile
                    open_selector(&h);
                    ms(900);
                    let w = h.get_webview_window("overlay-1").expect("overlay-1");
                    let _ = w.eval(&js("mousedown", 300.0, 300.0));
                    for i in 1..=5 { let _ = w.eval(&js("mousemove", 300.0 + 40.0 * i as f64, 300.0 + 25.0 * i as f64)); ms(40); }
                    let _ = w.eval(&js("mouseup", 500.0, 425.0));
                    ms(400);
                    println!("[saveas] selection made; opening the Save dialog (blocks until it is dismissed)");
                    let r = commit(&h, &st, true);
                    println!("[saveas] dialog dismissed -> {:?}", r.as_ref().map(|_| "saved").map_err(|e| e.message().to_string()));
                    ms(600);
                    println!("[saveas] after CANCEL: selection kept = {}, overlay still up = {}",
                        st.drag.lock().unwrap().is_some(), w.is_visible().unwrap_or(false));
                    println!("[saveas] HOLD-AFTER-CANCEL");
                    ms(3500); // the harness sends a second Esc here: it must now reach the OVERLAY
                    println!("[saveas] 3.5 s later: overlay visible = {}", w.is_visible().unwrap_or(false));
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
                    open_selector(&h);
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
                    open_selector(&h);
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
        .run(tauri::generate_context!())
        .expect("error while running BHCapture");
}
