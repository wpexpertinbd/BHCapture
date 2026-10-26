#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! BHCapture shell: overlay windows, global hotkey, and the region selector's state.
//!
//! The **selection lives in Rust**, not in the webview. Each display gets its own overlay window,
//! and a drag that crosses a display boundary is still one selection — so no single webview can
//! own it. Rust holds the global rectangle and broadcasts it; every overlay renders the part that
//! falls on its own display.

use std::sync::{Arc, Mutex};

mod recording;
mod updater;
mod camera_bubble;

use bhcapture_capture::{platform::{self, PlatformCapturer}, CaptureError, Capturer, Snapshot};
use bhcapture_core::geom::{DisplayId, DisplayLayout, LogPoint, LogRect};
use bhcapture_core::image::Frame;
use bhcapture_core::naming::{Fields, Pattern, Stamp, unique_path};
use bhcapture_core::settings::{LoadOutcome, Settings, SettingsStore};
use bhcapture_core::snap::{snap_window, Candidate};
use bhcapture_core::hotkey::{Chord, Key};
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
/// The start window (decision 40): new capture, open an image, recent captures.
const HOME_WINDOW: &str = "home";
/// How many recent captures the start window shows.
const RECENT_MAX: usize = 12;

struct AppState {
    capturer: PlatformCapturer,
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
    /// The files the start window last listed. Its page refers to them by position only, so the
    /// webview never hands us a path to open.
    recent: Mutex<Vec<std::path::PathBuf>>,
}

/// One capture being edited. The pixels stay in memory until the window closes; a save writes the
/// EDITED image, which the editor renders and sends back.
struct EditorDoc {
    frame: Frame,
    saved_to: Option<std::path::PathBuf>,
    /// The file this was opened from (start window). Save writes a NEW file beside it, never over it.
    origin: Option<std::path::PathBuf>,
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
    /// Open at login is on, but macOS will not run it until the user allows it in Login Items.
    login_blocked: Option<String>,
}

fn settings_view(state: &AppState, notice: Option<String>) -> SettingsView {
    let st = state.settings.lock().unwrap();
    SettingsView {
        settings: st.get().clone(),
        default_dir: default_capture_dir().display().to_string(),
        path: st.path().display().to_string(),
        writable: st.is_writable(),
        notice,
        login_blocked: login_blocked(st.get().launch_at_login),
    }
}

#[tauri::command]
fn settings_get(state: tauri::State<Arc<AppState>>) -> SettingsView {
    settings_view(&state, None)
}

#[tauri::command]
fn settings_save(app: tauri::AppHandle, state: tauri::State<Arc<AppState>>, next: Settings) -> Result<SettingsView, String> {
    // Refuse BEFORE applying anything: an unreadable settings file means saving is refused, and the
    // running app must not quietly adopt values that were never saved (found by the Codex review).
    if !state.settings.lock().unwrap().is_writable() {
        return Err(state.settings.lock().unwrap().save().err().map(|e| e.to_string())
            .unwrap_or_else(|| "settings cannot be saved".into()));
    }
    let previous = state.settings.lock().unwrap().get().clone();
    // The updater owns these (its own switch and bookkeeping); a Settings page holding an older copy
    // must not write them back.
    let next = Settings { auto_update_check: previous.auto_update_check, last_update_check: previous.last_update_check,
                          notified_update: previous.notified_update.clone(),
                          login_registered: previous.login_registered, ..next };
    let mut notes = { state.settings.lock().unwrap().set(next) };

    // The shortcut takes effect NOW (sync commands run on the main thread, where the hotkey API
    // lives). If the OS will not give us the new chord, keep the old one and say so, rather than
    // saving a setting that does nothing.
    // Shortcuts take effect NOW (sync commands run on the main thread, where the hotkey API lives).
    // Any the OS refuses go back to their previous value, and the note says so.
    let next = state.settings.lock().unwrap().get().clone();
    let kept = apply_hotkeys(&next, Some(&previous));
    if !kept.is_empty() {
        let mut st = state.settings.lock().unwrap();
        let mut s = st.get().clone();
        for (slot, why) in &kept {
            notes.push(format!("{why}. Kept {}.", slot.read(&previous)));
            slot.write(&mut s, slot.read(&previous).to_string());
        }
        st.set(s);
    }

    // Report a refused save honestly, and roll back everything applied above, so what is running
    // always matches what is on disk.
    if let Err(e) = state.settings.lock().unwrap().save() {
        state.settings.lock().unwrap().set(previous.clone());
        let _ = apply_hotkeys(&previous, None);
        return Err(e.to_string());
    }
    apply_settings(&state);
    // A camera bubble on screen follows the new shape, size and background at once.
    let next_login = state.settings.lock().unwrap().get().launch_at_login;
    sync_launch_at_login(&state, previous.launch_at_login != next_login);
    let now = state.settings.lock().unwrap().get().clone();
    camera_bubble::apply_settings(&app, &now);
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

/// Settings › Camera › Background › Your picture: choose the picture.
#[tauri::command]
async fn settings_pick_image(app: tauri::AppHandle) -> Option<String> {
    app.dialog().file().set_title("Choose a background picture")
        .add_filter("Pictures", &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "gif"])
        .blocking_pick_file()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.display().to_string())
}

/// Settings › General: "Open Login Items" when macOS is holding the login entry back.
#[tauri::command]
fn login_open_settings() { bhcapture_capture::login::open_settings(); }

/// Open at login is on, but macOS will not run the entry until the user allows it.
fn login_blocked(want: bool) -> Option<String> {
    if want { bhcapture_capture::login::blocked_where().map(str::to_string) } else { None }
}

/// Make the OS's login entry match the setting (decision 38). `explicit` = the user has just changed
/// the setting in Settings, which is the only time a removed entry is re-created.
///
/// Never in debug, test or demo builds, and only from an installed copy: those must never register
/// (or take over) a login entry pointing at a development binary on the user's machine.
fn sync_launch_at_login(state: &AppState, explicit: bool) {
    if cfg!(debug_assertions) || cfg!(feature = "demo") || std::env::var_os("BHC_TEST_OUT").is_some() { return; }
    // Unreadable settings = running on defaults: never act on them (they would say "on").
    if !state.settings.lock().unwrap().is_writable() { return; }
    use bhcapture_capture::login::{self, Entry};
    let s = state.settings.lock().unwrap().get().clone();
    let mut next = s.clone();
    if !s.launch_at_login {
        match login::unregister() {
            Ok(()) => next.login_registered = false,
            Err(e) => eprintln!("[login] could not remove the login entry: {e}"),
        }
    } else {
        let Some(target) = login::current_target().filter(|t| login::is_installed_location(t)) else { return };
        if explicit { login::user_enabled(); }
        let write = |next: &mut Settings| match login::register(&target) {
            Ok(()) => { next.login_registered = true; println!("[login] open at login on ({})", target.display()); }
            Err(e) => eprintln!("[login] could not turn open-at-login on: {e}"),
        };
        match login::state(&target) {
            Entry::Current => next.login_registered = true,
            // The app was moved (or updated to another path): point the entry at this copy.
            Entry::Stale => write(&mut next),
            // Removed through the OS after we made it: that is the user saying "off".
            Entry::Missing if s.login_registered && !explicit => {
                next.launch_at_login = false;
                next.login_registered = false;
                println!("[login] the login entry was removed outside BHCapture; open at login is now off");
            }
            Entry::Missing => write(&mut next),
        }
    }
    if next != s {
        let mut st = state.settings.lock().unwrap();
        // Only the two login fields: anything else may have changed meanwhile.
        let mut cur = st.get().clone();
        cur.launch_at_login = next.launch_at_login;
        cur.login_registered = next.login_registered;
        st.set(cur);
        if let Err(e) = st.save() { eprintln!("[login] could not save: {e}"); }
    }
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
            let _ = app.run_on_main_thread(platform::refresh_menu_bar);
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

/// BHCapture was opened again while it runs: the Dock / Launchpad / Finder icon on macOS (the
/// `Reopen` handler), a second launch on Windows/Linux (the single-instance hand-off).
///
/// Captures and recordings are what the user comes back for (Benjamin, 2026-10-22: "I was just
/// need to see the last editor that was opened"), so those come to the front. Nothing else does:
/// with nothing open, BHCapture simply stays in the menu bar / tray, ready for a shortcut
/// ("as normally it will stay open as topbar"). Settings opens only when asked for (tray menu,
/// app menu, ⌘,).
fn reopen(app: &tauri::AppHandle) {
    let docs: Vec<_> = app.webview_windows().into_iter()
        .filter(|(l, _)| l.starts_with(EDITOR_PREFIX) || l.starts_with(recording::VIDEO_PREFIX))
        .map(|(_, w)| w)
        .collect();
    // Nothing open: the start window, Snagit's model (decision 40) -- never Settings.
    if docs.is_empty() { let _ = open_home(app); return; }
    for w in &docs {
        let _ = w.unminimize();
        let _ = w.show();
    }
    sync_dock(app, None);
    let _ = docs[0].set_focus();
}

/// Show the start window (create it, or bring an open one forward). Main thread.
fn open_home(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(HOME_WINDOW) {
        let _ = w.unminimize();
        let _ = w.show();
        sync_dock(app, None);
        let _ = w.set_focus();
        return Ok(());
    }
    let w = WebviewWindowBuilder::new(app, HOME_WINDOW, WebviewUrl::App("index.html".into()))
        .title("BHCapture")
        .inner_size(780.0, 560.0)
        .min_inner_size(420.0, 380.0)
        .center()
        .resizable(true)
        .build().map_err(|e| e.to_string())?;
    sync_dock(app, None);
    let _ = w.set_focus();
    Ok(())
}

/// The start window goes away once it has handed over to a capture or an editor.
fn close_home(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window(HOME_WINDOW) {
        let _ = w.destroy();
        sync_dock(app, Some(HOME_WINDOW));
    }
}

#[derive(Serialize)]
struct RecentCapture { name: String, width: u32, height: u32, thumb: String }

/// The latest saved captures in the save folder, with small previews. Decoding is slow-ish, so it
/// runs off the main thread; a file that cannot be read is left out, not reported.
#[tauri::command]
async fn home_recent(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<RecentCapture>, String> {
    use base64::Engine;
    let dir = state.settings.lock().unwrap().get().output_dir.clone().unwrap_or_else(default_capture_dir);
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // One listing at a time: the list this returns and the one `home_open_recent` indexes into
        // must be the same, and two decodes at once would only double the memory.
        static LISTING: Mutex<()> = Mutex::new(());
        let _one = LISTING.lock().unwrap_or_else(|p| p.into_inner());
        let files = bhcapture_core::library::recent_images(&dir, RECENT_MAX);
        let mut kept = Vec::new();
        let mut out = Vec::new();
        for p in files {
            let Ok((jpeg, width, height)) = bhcapture_core::library::thumbnail_jpeg(&p, 360) else { continue };
            out.push(RecentCapture {
                name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                width, height,
                thumb: format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpeg)),
            });
            kept.push(p);
        }
        *st.recent.lock().unwrap() = kept;
        out
    }).await.map_err(|e| e.to_string())
}

/// Open an image file in a new editor (async: `open_editor` waits on the main thread).
/// `listed`: the file came from the recent-captures list, so a symlink put in its place since is
/// refused (a file picked in Open Image… may be a link on purpose).
async fn open_image(app: &tauri::AppHandle, state: &Arc<AppState>, path: std::path::PathBuf, listed: bool) -> Result<(), String> {
    let p = path.clone();
    let (rgba, w, h) = tauri::async_runtime::spawn_blocking(move || if listed {
            bhcapture_core::library::decode_listed(&p)
        } else {
            bhcapture_core::library::decode_file(&p)
        })
        .await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let mut bgra = rgba;
    for px in bgra.chunks_exact_mut(4) { px.swap(0, 2); }
    let frame = Frame::from_bgra(bgra, w, h, w as usize * 4, bhcapture_core::geom::Scale::new(1.0).unwrap())
        .map_err(|e| e.to_string())?;
    open_editor(app, state, frame, Some(path))?;
    let a = app.clone();
    let _ = app.run_on_main_thread(move || close_home(&a));
    Ok(())
}

/// A recent capture, by its position in the list `home_recent` returned (never a path).
#[tauri::command]
async fn home_open_recent(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, index: usize) -> Result<(), String> {
    let path = state.recent.lock().unwrap().get(index).cloned().ok_or("that capture is no longer listed")?;
    open_image(&app, state.inner(), path, true).await
}

/// Open Image…: the file is chosen in a native dialog here, never named by the webview.
#[tauri::command]
async fn home_open_file(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    let dir = state.settings.lock().unwrap().get().output_dir.clone().unwrap_or_else(default_capture_dir);
    let mut d = app.dialog().file().set_title("Open an image to edit").set_directory(&dir);
    let exts: Vec<&str> = ALL_FORMATS.iter().flat_map(|f| f.extensions().iter().copied()).collect();
    d = d.add_filter("Images", &exts);
    if let Some(w) = app.get_webview_window(HOME_WINDOW) { d = d.set_parent(&w); }
    let Some(picked) = d.blocking_pick_file() else { return Ok(()) };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    open_image(&app, state.inner(), path, false).await
}

/// New Screenshot / Record Video from the start window: the same as the tray items.
#[tauri::command]
fn home_capture(app: tauri::AppHandle, video: bool) {
    close_home(&app);
    // A capture leaves out only the selector's own windows, so give the start window time to be
    // gone from the screen first -- it must not end up in the screenshot it was used to take.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let a = app.clone();
        let _ = app.run_on_main_thread(move || {
            if video { recording::record_shortcut(&a) } else { open_selector(&a, false) }
        });
    });
}

/// Release the frozen frames (tens of MB on a Retina panel) and invalidate in-flight work.
fn release_frozen(state: &AppState) {
    // The epoch moves under the snapshot lock, so a capture finishing on another thread either sees
    // the new epoch or lands before the clear -- never stores an old frame into a new session.
    let mut snap = state.snapshot.lock().unwrap();
    state.epoch.fetch_add(1, Ordering::SeqCst);
    *snap = None;
    drop(snap);
    let mut cands = state.cands.lock().unwrap();
    cands.clear();
}

/// The one way the selector opens, so the hotkey, the tray and the tests behave identically.
/// `video` picks what Enter does: open the capture in the editor, or set up a recording.
fn open_selector(app: &tauri::AppHandle, video: bool) { open_selector_as(app, video, false) }

/// All-in-One: the selector with an Image ⇄ Video switch, starting in the user's default mode.
fn open_all_in_one(app: &tauri::AppHandle) {
    let video = app.state::<Arc<AppState>>().settings.lock().unwrap().get().capture_default_mode
        == bhcapture_core::settings::CaptureMode::Video;
    open_selector_as(app, video, true);
}

/// Whether this selector session offers the Image ⇄ Video switch (All-in-One).
static SWITCHABLE: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
struct SelectorMode { mode: &'static str, switchable: bool }

fn emit_selector_mode(app: &tauri::AppHandle) {
    let video = recording::VIDEO_MODE.load(Ordering::SeqCst);
    let _ = app.emit("selector-mode", SelectorMode { mode: if video { "video" } else { "image" },
                                                     switchable: SWITCHABLE.load(Ordering::SeqCst) });
}

/// The All-in-One switch (a click on it, or Tab). Ignored outside All-in-One.
#[tauri::command]
fn selector_set_mode(app: tauri::AppHandle, video: bool) {
    if !SWITCHABLE.load(Ordering::SeqCst) { return; }
    recording::VIDEO_MODE.store(video, Ordering::SeqCst);
    emit_selector_mode(&app);
}

fn open_selector_as(app: &tauri::AppHandle, video: bool, switchable: bool) {
    recording::VIDEO_MODE.store(video, Ordering::SeqCst);
    SWITCHABLE.store(switchable, Ordering::SeqCst);
    emit_selector_mode(app);
    let state = app.state::<Arc<AppState>>().inner().clone();
    *state.drag.lock().unwrap() = None;
    release_frozen(&state);
    let epoch = state.epoch.load(Ordering::SeqCst);
    broadcast(app, None);
    // Where a capture cannot leave our own windows out (X11), freeze the screen BEFORE the overlay
    // appears -- afterwards it would photograph its own dim scrim. Costs the time of one capture.
    let frozen_first = platform::CAPTURE_SEES_OWN_WINDOWS;
    if frozen_first {
        let layout = state.layout.lock().unwrap().clone();
        match Snapshot::take(&state.capturer, &layout) {
            Ok(snap) => *state.snapshot.lock().unwrap() = Some(Arc::new(snap)),
            Err(e) => eprintln!("[warn] could not freeze the screen ({e})"),
        }
    }
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
    if !frozen_first { std::thread::spawn(move || {
        let layout = st.layout.lock().unwrap().clone();
        match Snapshot::take(&st.capturer, &layout) {
            Ok(snap) => {
                let mut slot = st.snapshot.lock().unwrap();
                if st.epoch.load(Ordering::SeqCst) != epoch { return; }
                *slot = Some(Arc::new(snap));
                drop(slot);
                // A pointer that has not moved since the overlay opened would otherwise show no
                // magnifier until its next movement.
                let _ = app_for_freeze.emit("frozen-ready", ());
            }
            Err(e) => eprintln!("[warn] could not freeze the screen ({e}); the magnifier is off and capture will be live"),
        }
    }); }
    let st = state.clone();
    std::thread::spawn(move || {
        match st.capturer.list_targets() {
            Ok(t) => {
                let own = st.overlay_ids.lock().unwrap().clone();
                let found: Vec<Candidate> = t.windows.iter()
                    .filter(|w| !own.contains(&(w.id.0 as u32)))
                    .map(|w| Candidate { bounds: w.bounds, layer: w.layer, z: w.z_order })
                    .collect();
                let mut cands = st.cands.lock().unwrap();
                if st.epoch.load(Ordering::SeqCst) == epoch { *cands = found; }
            }
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
fn settings_path() -> std::path::PathBuf { app_data_dir().join("settings.json") }

/// The user's home folder: `HOME` on macOS/Linux, `USERPROFILE` on Windows.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from)
}

/// BHCapture's private data folder: `~/Library/Application Support/BHCapture` on macOS,
/// `%APPDATA%\BHCapture` on Windows, `$XDG_DATA_HOME/BHCapture` (or `~/.local/share`) on Linux.
fn app_data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    let base = home_dir().map(|h| h.join("Library").join("Application Support"));
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from).or_else(|| home_dir().map(|h| h.join("AppData").join("Roaming")));
    #[cfg(not(any(target_os = "macos", windows)))]
    let base = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from).or_else(|| home_dir().map(|h| h.join(".local").join("share")));
    base.unwrap_or_else(std::env::temp_dir).join("BHCapture")
}

/// The OS window behind a Tauri window: `NSWindow*` on macOS, `HWND` on Windows.
fn native_handle(w: &tauri::WebviewWindow) -> Option<*mut std::ffi::c_void> {
    #[cfg(target_os = "macos")]
    { w.ns_window().ok() }
    #[cfg(windows)]
    { w.hwnd().ok().map(|h| h.0 as *mut std::ffi::c_void) }
    #[cfg(not(any(target_os = "macos", windows)))]
    { let _ = w; None }
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
    let home = home_dir();
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
#[cfg(windows)]
fn local_stamp() -> Stamp {
    let t = bhcapture_capture::windows::local_time();
    Stamp { year: t.0, month: t.1, day: t.2, hour: t.3, minute: t.4, second: t.5 }
}

#[cfg(not(windows))]
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

    open_editor(app, state, frame, None).map_err(CommitError::Failed)
}

/// Where a save goes, and in which format: the default folder with a generated, collision-free name,
/// or a native Save dialog. The webview never supplies a path -- it only says "save" or "save as".
///
/// In Save As the extension the user types wins (`shot.jpg` is a JPEG even if PNG was selected), and a
/// name with no extension we write gets the selected format's.
fn choose_save_path(app: &tauri::AppHandle, state: &AppState, width: u32, height: u32, save_as: bool,
                    format: ImageFormat, parent: Option<&tauri::WebviewWindow>, origin: Option<&std::path::Path>)
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
    let mut stem = pattern.render(local_stamp(), &Fields { width, height, ..Default::default() });
    // An image opened for editing: the edited copy goes BESIDE it as "<name> edited", and the
    // original is never written (decision 40). unique_path adds " (2)" rather than overwrite.
    let mut out_dir = out_dir;
    if let Some(o) = origin {
        if let Some(d) = o.parent() { out_dir = d.to_path_buf(); }
        stem = bhcapture_core::library::edited_stem(o);
    }

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
        Ok((unique_path(&out_dir, &stem, format.extension(), &|p| bhcapture_core::private_file::occupied(p)), format))
    }
}

// ---------------------------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------------------------

/// Open `frame` in a new editor window. Windows are created on the main thread; this waits for it.
/// Must not be called ON the main thread (every caller is a command or worker thread).
fn open_editor(app: &tauri::AppHandle, state: &Arc<AppState>, frame: Frame, origin: Option<std::path::PathBuf>)
    -> Result<String, String> {
    let id = state.next_editor.fetch_add(1, Ordering::SeqCst) + 1;
    let (w, h, scale) = (frame.width(), frame.height(), frame.scale().get());
    let title = match &origin {
        Some(p) => format!("BHCapture — {}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        None => format!("BHCapture — {w} × {h}"),
    };
    state.editors.lock().unwrap().insert(id, EditorDoc { frame, saved_to: None, origin });

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
        let b = WebviewWindowBuilder::new(&a, &l, WebviewUrl::App("index.html".into()))
            .title(title)
            .inner_size(ww, wh)
            .min_inner_size(640.0, 420.0)
            .center()
            .resizable(true);
        let r = with_window_menu(&a, &l, b).build();
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
struct EditorInfo { id: u64, width: u32, height: u32, scale: f64, saved_to: Option<String>, format: ImageFormat,
                    /// Opened from this file (start window): nothing to lose by closing it unedited.
                    origin: Option<String> }

#[tauri::command]
fn editor_info(window: tauri::Window, state: tauri::State<Arc<AppState>>) -> Result<EditorInfo, String> {
    let id = editor_id_of(window.label())?;
    let format = state.settings.lock().unwrap().get().image_format;
    let docs = state.editors.lock().unwrap();
    let d = docs.get(&id).ok_or("this capture is no longer open")?;
    Ok(EditorInfo { id, width: d.frame.width(), height: d.frame.height(), scale: d.frame.scale().get(),
                    saved_to: d.saved_to.as_ref().map(|p| p.display().to_string()), format,
                    origin: d.origin.as_ref().map(|p| p.display().to_string()) })
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

/// Which window may call which command. Every page is our own code, but a webview that ever rendered
/// something hostile (a crafted window title, say) must not be able to reach commands meant for a
/// different window -- above all `settings_save`, which picks where captures are written and which
/// keys are grabbed system-wide. Anything not listed is refused (audit 2026-10-13).
fn command_allowed(label: &str, cmd: &str) -> bool {
    let window = if label.starts_with(OVERLAY_PREFIX) { "overlay" }
        else if label.starts_with(EDITOR_PREFIX) { "editor" }
        else if label.starts_with(recording::VIDEO_PREFIX) { "video" }
        else if label == SETTINGS_WINDOW { "settings" }
        else if label == recording::HUD { "recorder" }
        else if label == camera_bubble::CAMERA_WINDOW { "camera" }
        else if label == HOME_WINDOW { "home" }
        else { return false };
    match window {
        "overlay" => matches!(cmd, "overlay_info" | "selection_begin" | "selection_update" | "selection_nudge"
            | "selection_cancel" | "selection_commit" | "snap_at" | "loupe" | "selection_set_rect"
            | "selection_fullscreen" | "selector_set_mode" | "open_privacy_settings" | "relaunch_app"
            | "report_overlay_focus"),
        "settings" => matches!(cmd, "settings_get" | "settings_save" | "settings_pick_dir" | "settings_pick_image" | "login_open_settings" | "hotkey_check" | "hotkeys_suspend" | "camera_devices")
            || cmd.starts_with("update_"),
        "editor" => cmd.starts_with("editor_"),
        "video" => cmd.starts_with("video_"),
        "recorder" => cmd.starts_with("recorder_"),
        "camera" => cmd == "camera_attach",
        "home" => matches!(cmd, "home_recent" | "home_open_recent" | "home_open_file" | "home_capture"),
        _ => false,
    }
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

    let origin = state.editors.lock().unwrap().get(&id).and_then(|d| d.origin.clone());
    let dialog = save_as || state.settings.lock().unwrap().get().always_ask_where_to_save;
    let (mut path, format) = choose_save_path(&app, &state, w, h, save_as, wanted, Some(&window), origin.as_deref())?;
    // An image opened for editing is never written over (decision 40) -- not even when the user
    // picks its own name in the Save dialog.
    if origin.as_deref().is_some_and(|o| same_file(&path, o)) {
        return Err(CommitError::Failed("That is the original image, which BHCapture leaves unchanged. Choose another name.".into()));
    }
    let (rgba, rw, rh) = decode_png_rgba(&png).map_err(CommitError::Failed)?;
    let bytes = export::encode(&rgba, rw, rh, format, quality).map_err(|e| CommitError::Failed(e.to_string()))?;
    let failed = |e: std::io::Error| CommitError::Failed(format!("Could not save the file: {e}"));
    if dialog {
        // A name the user chose (the dialog has already asked about replacing an existing file).
        bhcapture_core::private_file::write_private(&path, &bytes).map_err(failed)?;
    } else {
        // A generated name must be NEW: if something took it meanwhile, pick the next free one
        // rather than replace it. An opened image whose folder cannot be written to (a read-only
        // share, another user's folder) is saved to the normal capture folder instead.
        let mut origin_here = origin.clone();
        let mut tries = 0;
        loop {
            match bhcapture_core::private_file::write_private_new(&path, &bytes) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
                Err(e) if origin_here.is_some()
                    && matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) => {
                    println!("[editor] cannot write beside the original ({e}); saving to the capture folder");
                    origin_here = None;
                }
                Err(e) => return Err(failed(e)),
            }
            path = choose_save_path(&app, &state, w, h, false, format, Some(&window), origin_here.as_deref())?.0;
        }
    }
    if let Some(d) = state.editors.lock().unwrap().get_mut(&id) { d.saved_to = Some(path.clone()); }

    // Clipboard is a convenience on top of a file that already exists -- never fail the save for it.
    if clip {
        if let Err(e) = rgba_to_clipboard(rgba, rw, rh) { eprintln!("[warn] saved but clipboard copy failed: {e}"); }
    }
    println!("[editor] {} saved {w}x{h} as {} ({} bytes) to {}", window.label(), format.label(), bytes.len(), path.display());
    Ok(path.display().to_string())
}

/// Whether two paths are the same file on disk (so a different spelling, case or a link still
/// matches). A path that does not exist yet is never the same file.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
            _ => false,
        }
    }
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
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
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
enum SelectorKey { Cancel, Save { dialog: bool }, Nudge(f64, f64), FullScreen, ToggleMode }

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
        // Tab flips Image ⇄ Video in All-in-One (does nothing in the single-purpose selectors).
        v.push((HotKey::new(None, Code::Tab), SelectorKey::ToggleMode));
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
        SelectorKey::ToggleMode => {
            if SWITCHABLE.load(Ordering::SeqCst) {
                let v = !recording::VIDEO_MODE.load(Ordering::SeqCst);
                recording::VIDEO_MODE.store(v, Ordering::SeqCst);
                emit_selector_mode(app);
            }
        }
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
/// The tray's "All-in-One Capture" item.
static TRAY_ALLINONE_ITEM: Mutex<Option<MainThreadOnly<MenuItem<tauri::Wry>>>> = Mutex::new(None);
static CAPTURE_SLOT: HotkeySlot = HotkeySlot { id: AtomicU32::new(0), key: Mutex::new(None), tray: &TRAY_ALLINONE_ITEM, what: "All-in-One" };
static REGION_SLOT: HotkeySlot = HotkeySlot { id: AtomicU32::new(0), key: Mutex::new(None), tray: &TRAY_CAPTURE_ITEM, what: "screenshot" };
static RECORD_SLOT: HotkeySlot = HotkeySlot { id: AtomicU32::new(0), key: Mutex::new(None), tray: &recording::TRAY_RECORD_ITEM, what: "record" };
/// The always-registered shortcuts (pause is registered only while recording).
static SLOTS: [&HotkeySlot; 3] = [&CAPTURE_SLOT, &REGION_SLOT, &RECORD_SLOT];

impl HotkeySlot {
    fn read<'s>(&self, s: &'s Settings) -> &'s str {
        match self.what { "All-in-One" => &s.capture_hotkey, "screenshot" => &s.region_hotkey, _ => &s.record_hotkey }
    }
    fn write(&self, s: &mut Settings, v: String) {
        match self.what { "All-in-One" => s.capture_hotkey = v, "screenshot" => s.region_hotkey = v, _ => s.record_hotkey = v }
    }
    /// Release this slot's chord, if any. Main thread.
    fn release(&self) {
        let mut cur = self.key.lock().unwrap();
        if let (Some(k), Some(m)) = (cur.take(), HOTKEY_MANAGER.lock().unwrap().as_ref()) { let _ = m.0.unregister(k); }
        self.id.store(0, Ordering::SeqCst);
    }
}

/// Register every always-on shortcut from `next`. All are released FIRST, so swapping two shortcuts
/// in one save works (registering one's new chord would otherwise fail while the other still holds
/// it). A chord the OS refuses falls back to its value in `fallback`, if given. Returns the slots that
/// could not take their new chord, with the reason. Main thread.
fn apply_hotkeys(next: &Settings, fallback: Option<&Settings>) -> Vec<(&'static HotkeySlot, String)> {
    for slot in SLOTS { slot.release(); }
    let mut kept = Vec::new();
    for slot in SLOTS {
        if let Err(e) = set_slot_hotkey(slot, slot.read(next)) {
            if let Some(prev) = fallback {
                if set_slot_hotkey(slot, slot.read(prev)).is_err() { eprintln!("[hotkey] {} has no working shortcut", slot.what); }
            }
            kept.push((slot, e));
        }
    }
    kept
}

/// Is this a shortcut BHCapture can use? Returns its canonical spelling, or why not. The Settings
/// shortcut boxes call it as soon as keys are pressed, before anything is saved.
#[tauri::command]
fn hotkey_check(spec: String) -> Result<String, String> {
    Chord::parse(&spec).map(|c| c.display()).map_err(|e| e.to_string())
}

/// Switch BHCapture's global shortcuts off while a Settings shortcut box is listening (otherwise
/// pressing the current capture shortcut to change it would open the selector), and back on.
/// Sync command: runs on the main thread, where the hotkey API lives.
#[tauri::command]
fn hotkeys_suspend(suspended: bool) {
    static SUSPENDED: AtomicBool = AtomicBool::new(false);
    if SUSPENDED.swap(suspended, Ordering::SeqCst) == suspended { return; }
    let guard = HOTKEY_MANAGER.lock().unwrap();
    let Some(m) = guard.as_ref() else { return };
    for slot in SLOTS {
        if let Some(k) = *slot.key.lock().unwrap() {
            let _ = if suspended { m.0.unregister(k) } else { m.0.register(k) };
        }
    }
}

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
                      Code::F7, Code::F8, Code::F9, Code::F10, Code::F11, Code::F12,
                      Code::F13, Code::F14, Code::F15, Code::F16, Code::F17, Code::F18,
                      Code::F19][(n.clamp(1, 19) - 1) as usize],
        Key::PrintScreen => Code::PrintScreen,
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

fn set_slot_hotkey(slot: &HotkeySlot, spec: &str) -> Result<String, String> {
    let chord = Chord::parse(spec).map_err(|e| e.to_string())?;
    let hk = to_platform_hotkey(&chord);
    let guard = HOTKEY_MANAGER.lock().unwrap();
    let m = guard.as_ref().ok_or("hotkey manager not ready")?;
    let mut current = slot.key.lock().unwrap();
    if current.map(|c| c.id()) == Some(hk.id()) { return Ok(chord.display()); }
    m.0.register(hk).map_err(|e| {
        if cfg!(windows) && chord.key == Key::PrintScreen {
            // Windows 11 gives Print Screen to its own Snipping Tool by default.
            format!("Windows is keeping Print Screen for itself. To use it for BHCapture, turn off \
                     Settings › Accessibility › Keyboard › \"Use the Print screen key to open screen capture\" ({e})")
        } else {
            format!("{} could not be registered (another app may already use it): {e}", chord.display())
        }
    })?;
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

/// The File > Format check items, so their ticks can follow the focused editor. The scope is the
/// window that owns the menu (Windows/Linux, one menu per window), or None for the macOS app menu.
static FORMAT_ITEMS: Mutex<Vec<MainThreadOnly<(Option<String>, ImageFormat, CheckMenuItem<tauri::Wry>)>>> = Mutex::new(Vec::new());

/// Separates the owning window from the action in a per-window menu item id: `w:<label>|ed:save`.
const MENU_SCOPE_SEP: char = '|';

/// `scope` = the window this menu belongs to (Windows/Linux). Its item ids then carry that window's
/// label, because Tauri hands every menu click to every window and to the app alike, and on Windows
/// "which window is focused" is false while the page inside it has the keyboard -- so neither can
/// say which window's File > Save was clicked. macOS has one app menu: scope None, plain ids.
fn build_app_menu(app: &tauri::AppHandle, scope: Option<&str>) -> tauri::Result<Menu<tauri::Wry>> {
    let id = |s: &str| -> String { match scope { Some(l) => format!("w:{l}{MENU_SCOPE_SEP}{s}"), None => s.to_string() } };
    let about = PredefinedMenuItem::about(app, Some("About BHCapture"), None)?;
    let settings = MenuItem::with_id(app, id("app:settings"), "Settings…", true, Some("CmdOrCtrl+,"))?;
    let app_menu = Submenu::with_items(app, "BHCapture", true, &[
        &about, &PredefinedMenuItem::separator(app)?, &settings, &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::hide(app, None)?, &PredefinedMenuItem::hide_others(app, None)?,
        &PredefinedMenuItem::separator(app)?, &PredefinedMenuItem::quit(app, None)?,
    ])?;

    let mut checks = Vec::new();
    let mut format_items: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
    for f in ALL_FORMATS {
        let it = CheckMenuItem::with_id(app, id(&format!("ed:format:{}", f.extension())), f.label(), true,
                                        *f == ImageFormat::Jpeg, None::<&str>)?;
        checks.push(MainThreadOnly((scope.map(str::to_string), *f, it.clone())));
        format_items.push(it);
    }
    {
        // Replace this menu's own items; drop those of windows that no longer exist.
        let mut all = FORMAT_ITEMS.lock().unwrap();
        all.retain(|it| it.0 .0.as_deref().is_some_and(|l| Some(l) != scope && app.get_webview_window(l).is_some()));
        all.extend(checks);
    }
    let fmt_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = format_items.iter().map(|i| i as _).collect();
    let format_menu = Submenu::with_items(app, "Image Format", true, &fmt_refs)?;
    // Recordings: the popular upload formats (decision 22). Acts only in a recording window.
    let mut exports = vec![
        MenuItem::with_id(app, id("ed:export:mp4"), "MP4 (H.264) — plays everywhere…", true, None::<&str>)?,
        MenuItem::with_id(app, id("ed:export:hevc"), "MP4 (HEVC) — smaller file…", true, None::<&str>)?,
    ];
    // MOV is Apple's container; Windows' Media Foundation cannot write it.
    if cfg!(target_os = "macos") {
        exports.push(MenuItem::with_id(app, id("ed:export:mov"), "MOV — for Final Cut / iMovie…", true, None::<&str>)?);
    }
    exports.push(MenuItem::with_id(app, id("ed:export:gif"), "Animated GIF — for chats and READMEs…", true, Some("Shift+CmdOrCtrl+E"))?);
    let export_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = exports.iter().map(|i| i as _).collect();
    let export_menu = Submenu::with_items(app, "Export Recording As", true, &export_refs)?;

    let file = Submenu::with_items(app, "File", true, &[
        &MenuItem::with_id(app, id("ed:save"), "Save", true, Some("CmdOrCtrl+S"))?,
        &MenuItem::with_id(app, id("ed:save-as"), "Save As…", true, Some("Shift+CmdOrCtrl+S"))?,
        &PredefinedMenuItem::separator(app)?,
        &format_menu,
        &export_menu,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, id("ed:close"), "Close", true, Some("CmdOrCtrl+W"))?,
    ])?;
    // Undo/Redo/Copy are OURS (they act on the drawing), but they hand over to the text box when
    // one is being typed into. Cut/Paste/Select All are the system's, for that text box.
    let edit = Submenu::with_items(app, "Edit", true, &[
        &MenuItem::with_id(app, id("ed:undo"), "Undo", true, Some("CmdOrCtrl+Z"))?,
        &MenuItem::with_id(app, id("ed:redo"), "Redo", true, Some("Shift+CmdOrCtrl+Z"))?,
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::cut(app, None)?,
        &MenuItem::with_id(app, id("ed:copy"), "Copy", true, Some("CmdOrCtrl+C"))?,
        &PredefinedMenuItem::paste(app, None)?,
        &PredefinedMenuItem::select_all(app, None)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, id("ed:delete"), "Delete Selected", true, None::<&str>)?,
    ])?;
    let window = Submenu::with_items(app, "Window", true, &[
        &PredefinedMenuItem::minimize(app, None)?,
        &PredefinedMenuItem::fullscreen(app, None)?,
    ])?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &window])
}

/// Give a document window (editor, playback) its menu on Windows/Linux, where menus live inside
/// windows. On macOS the app menu bar serves every window, so the builder is returned unchanged.
fn with_window_menu<'a>(app: &tauri::AppHandle, label: &str, b: WebviewWindowBuilder<'a, tauri::Wry, tauri::AppHandle>)
    -> WebviewWindowBuilder<'a, tauri::Wry, tauri::AppHandle> {
    #[cfg(not(target_os = "macos"))]
    if let Ok(m) = build_app_menu(app, Some(label)) { return b.menu(m); }
    let _ = (app, label);
    b
}

/// Send a menu action to the editor the user is working in. Nothing happens if no editor has focus
/// (for example while Settings is in front), rather than acting on a window the user cannot see.
fn route_menu_event(app: &tauri::AppHandle, id: &str) {
    // A per-window menu (Windows/Linux) names its window: act on exactly that one.
    if let Some((label, rest)) = id.strip_prefix("w:").and_then(|r| r.split_once(MENU_SCOPE_SEP)) {
        if rest == "app:settings" { let _ = open_settings(app.clone()); return; }
        let Some(action) = rest.strip_prefix("ed:") else { return };
        if (label.starts_with(EDITOR_PREFIX) || label.starts_with(recording::VIDEO_PREFIX))
            && app.get_webview_window(label).is_some() {
            let _ = app.emit_to(label, "editor-menu", action.to_string());
        }
        return;
    }
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
fn editor_format_changed(window: tauri::WebviewWindow, format: ImageFormat) {
    for item in FORMAT_ITEMS.lock().unwrap().iter() {
        let (scope, f, it) = &item.0;
        // The app menu (macOS), or this window's own menu (Windows/Linux).
        if scope.as_deref().is_none_or(|l| l == window.label()) { let _ = it.set_checked(*f == format); }
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
            // Undo the panel conversion first, or WebKit's teardown throws and the app aborts.
            if let Some(p) = native_handle(&w) { unsafe { platform::release_tool_window(p); } }
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

        if let Some(ptr) = native_handle(&w) {
            // macOS: a non-activating panel, so it can take keystrokes from a global hotkey (idempotent;
            // refuses rather than risk a memory overrun). Windows: kept out of every capture.
            if !unsafe { platform::prepare_tool_window(ptr) } {
                eprintln!("[panel] {label}: could not prepare the overlay; keyboard focus may need a click");
            }
            // Register the overlay with the capturer so it is excluded from captures (macOS's list;
            // Windows does it per window above). Without this the selector photographs its own scrim.
            let id = unsafe { platform::window_id(ptr) };
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

/// One BHCapture per user. A second copy would fight over the shortcuts, and -- worse -- treat the
/// first one's LIVE recording in the shared working folder as a crash leftover and rewrite it.
/// The lock is held for the life of the process and released by the OS however it ends.
static SECOND_COPY: AtomicBool = AtomicBool::new(false);

fn claim_single_instance() -> Option<std::fs::File> {
    let dir = app_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)); }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("instance.lock")).ok()?;
    match f.try_lock() {
        Ok(()) => Some(f),
        // Already running: the single-instance plugin hands this launch to the running copy (which
        // opens Settings) and exits. Should it not (a copy started from another bundle), setup exits.
        Err(std::fs::TryLockError::WouldBlock) => { SECOND_COPY.store(true, Ordering::SeqCst); None }
        // A filesystem without locks: run anyway rather than refuse to start.
        Err(std::fs::TryLockError::Error(e)) => { eprintln!("[app] could not take the instance lock ({e})"); None }
    }
}

fn main() {
    let _instance = claim_single_instance();
    let capturer = PlatformCapturer::new();
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
        recent: Mutex::new(Vec::new()),
    });
    apply_settings(&state);

    let builder = tauri::Builder::default();
    // First, so a second launch is handed over before anything else starts: launching BHCapture
    // again (Start menu, app launcher, Finder) brings the running copy's open captures forward --
    // the same rule as the macOS Dock click (`reopen`): never Settings on its own.
    // Test builds run beside the user's own BHCapture under a throwaway HOME, so they may skip it.
    #[cfg(feature = "demo")]
    let skip_single = std::env::var_os("BHC_TEST_OUT").is_some();
    #[cfg(not(feature = "demo"))]
    let skip_single = false;
    let builder = if skip_single { builder } else {
        builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let a = app.clone();
            let _ = app.run_on_main_thread(move || reopen(&a));
        }))
    };
    builder
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
                    // Settings suspends the shortcuts while a shortcut field is listening; closing it
                    // mid-listen must not leave them off until the next launch.
                    if window.label() == SETTINGS_WINDOW { hotkeys_suspend(false); }
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
        .invoke_handler({
            let handler: Box<dyn Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync> = Box::new(tauri::generate_handler![
            overlay_info, selection_begin, selection_update,
            selection_nudge, selection_cancel, selection_commit,
            snap_at, loupe, selection_set_rect, selection_fullscreen, selector_set_mode, hotkey_check, hotkeys_suspend, open_privacy_settings, relaunch_app, report_overlay_focus,
            settings_get, settings_save, settings_pick_dir, settings_pick_image, open_settings,
            editor_info, editor_pixels, editor_save, editor_copy, editor_close, editor_format_changed,
            recording::recorder_info, recording::recorder_option, recording::recorder_start,
            recording::recorder_pause, recording::recorder_stop, recording::recorder_cancel, recording::recorder_hide, recording::recorder_restart,
            recording::video_info, recording::video_save, recording::video_close, recording::video_export,
            camera_bubble::camera_attach, camera_bubble::camera_devices,
            updater::update_info, updater::update_check, updater::update_set_auto, updater::update_install, updater::update_open_page, login_open_settings,
            home_recent, home_open_recent, home_open_file, home_capture
            ]);
            move |invoke: tauri::ipc::Invoke<tauri::Wry>| {
                let (label, cmd) = (invoke.message.webview_ref().label().to_string(), invoke.message.command().to_string());
                if !command_allowed(&label, &cmd) {
                    eprintln!("[ipc] refused {cmd} from window {label}");
                    invoke.resolver.reject(format!("{cmd} is not available in this window"));
                    return true;
                }
                handler(invoke)
            }
        })
        .setup(move |app| {
            if SECOND_COPY.load(Ordering::SeqCst) {
                eprintln!("[app] BHCapture is already running; this copy exits.");
                std::process::exit(0);
            }
            let handle = app.handle().clone();
            sync_launch_at_login(&state, false);
            // Menu-bar only until a window is opened. Info.plist's LSUIElement does this for the
            // bundled app; stating it here makes an unbundled dev run behave the same.
            sync_dock(&handle, None);
            // The menu bar for editor/Settings windows. macOS shows it only in regular mode.
            // macOS: ONE menu bar for the app. Windows/Linux put a menu INSIDE each window, and an
            // app-wide one would land on every window -- the selector overlays and the recording bar
            // too -- so there the editor and playback windows get theirs (`with_window_menu`).
            #[cfg(target_os = "macos")]
            match build_app_menu(&handle, None) {
                Ok(m) => { let _ = app.set_menu(m); }
                Err(e) => eprintln!("[menu] could not build the menu bar: {e}"),
            }
            // Pre-create every overlay hidden AT LAUNCH. Creating a webview window on
            // hotkey-press is visibly slow; spike 0c measured ~25 ms for a pre-created one.
            sync_overlays(&handle, &state)?;

            // Keep the overlays matched to the real displays for the life of the process.
            let h_watch = handle.clone();
            platform::watch_displays(move || on_displays_changed(&h_watch));

            // Tray: without it there is no way to reach settings, since every window is an
            // overlay that hides itself.
            let all_i = MenuItem::with_id(app, "all-in-one", "All-in-One Capture", true, None::<&str>)?;
            let cap_i = MenuItem::with_id(app, "capture", "Screenshot", true, None::<&str>)?;
            let rec_i = MenuItem::with_id(app, "record", "Record Video", true, None::<&str>)?;
            // Only meaningful while recording; brings back (or hides) the control bar.
            let pause_key = state.settings.lock().unwrap().get().pause_hotkey.clone();
            let pause_i = MenuItem::with_id(app, "pause", "Pause Recording", false, Some(pause_key.as_str()))?;
            let bar_i = MenuItem::with_id(app, "toggle-bar", "Hide Recording Bar", false, None::<&str>)?;
            let upd_i = MenuItem::with_id(app, "check-updates", "Check for Updates…", true, None::<&str>)?;
            let set_i = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit BHCapture", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&all_i, &cap_i, &rec_i, &pause_i, &bar_i, &PredefinedMenuItem::separator(app)?, &upd_i, &set_i, &quit_i])?;
            updater::set_tray_item(upd_i.clone());
            *TRAY_CAPTURE_ITEM.lock().unwrap() = Some(MainThreadOnly(cap_i.clone()));
            *TRAY_ALLINONE_ITEM.lock().unwrap() = Some(MainThreadOnly(all_i.clone()));
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
                    "all-in-one" => { if !recording::busy() { open_all_in_one(app); } }
                    "capture" => open_selector(app, false),
                    "record" => recording::record_shortcut(app),
                    "toggle-bar" => recording::toggle_bar(app),
                    "pause" => recording::toggle_pause(app),
                    "settings" => { let _ = open_settings(app.clone()); }
                    // Settings shows the result (and the Download & Install button).
                    "check-updates" => {
                        let _ = open_settings(app.clone());
                        let a = app.clone();
                        std::thread::spawn(move || updater::check(&a, true));
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            // Keep the TrayIcon alive: dropping it removes the icon from the menu bar.
            app.manage(tray);
            updater::start_background(&handle);

            // Kept (not leaked) so shortcuts can be changed and the selector's keys armed later,
            // always from the main thread; dropping it would unregister everything.
            *HOTKEY_MANAGER.lock().unwrap() = Some(MainThreadOnly(GlobalHotKeyManager::new()?));
            // Every shortcut from settings; one the OS refuses falls back to its default, and failing
            // that is reported (the tray still works without it).
            let wanted = state.settings.lock().unwrap().get().clone();
            for (slot, why) in apply_hotkeys(&wanted, Some(&Settings::default())) {
                eprintln!("[hotkey] {} shortcut: {why}", slot.what);
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
                    if ev.id == CAPTURE_SLOT.id.load(Ordering::SeqCst) {
                        println!("[hotkey] All-in-One pressed");
                        if !recording::busy() { open_all_in_one(&h); }
                    } else if ev.id == REGION_SLOT.id.load(Ordering::SeqCst) {
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

            // Audit 2026-10-13: every page renders under the CSP, may call its own commands and is
            // REFUSED another window's. Results come back as "sec-probe" events.
            if std::env::args().any(|a| a == "--demo-security") {
                use tauri::Listener;
                let (h, st) = (handle.clone(), state.clone());
                h.listen_any("sec-probe", |e| println!("[sec] {}", e.payload()));
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    // `own` must succeed, `foreign` must be refused; also report what rendered.
                    let probe = |w: &tauri::WebviewWindow, own: &str, foreign: &str| {
                        let js = format!(r#"(async()=>{{
                          const I=window.__TAURI_INTERNALS__; const out={{win:{label:?}, html:document.body.innerText.length, text:document.body.innerText.slice(0,60),
                            styled:getComputedStyle(document.body).margin, csp:window.__cspv||[]}};
                          try{{ await I.invoke({own:?}); out.own='ok'; }}catch(e){{ out.own='ERR '+e; }}
                          try{{ await I.invoke({foreign:?}, {{settings:{{}}}}); out.foreign='ALLOWED'; }}catch(e){{ out.foreign='refused: '+e; }}
                          const v=document.querySelector('video'); if(v) out.video={{ready:v.readyState, err:v.error&&v.error.code, src:v.currentSrc.slice(0,40)}};
                          const bar=document.querySelector('.rc-bar'); if(bar) out.bar=[bar.scrollWidth, bar.clientWidth, window.innerWidth];
                          const tab=[...document.querySelectorAll('.tabs button')].find(b=>b.textContent==='Camera'); if(tab){{ tab.click(); await new Promise(r=>setTimeout(r,300)); out.camtab=document.body.innerText.split('Camera').slice(1).join('Camera').slice(0,400); out.camtabOverflow=document.documentElement.scrollWidth>window.innerWidth; }}
                          await I.invoke('plugin:event|emit',{{event:'sec-probe',payload:out}});
                        }})()"#, label = w.label());
                        let _ = w.eval(&js);
                    };
                    let watch_csp = "window.__cspv=[];document.addEventListener('securitypolicyviolation',e=>window.__cspv.push(e.violatedDirective+' '+e.blockedURI));";
                    ms(2500);
                    // 1. Selector overlay.
                    open_selector(&h, false);
                    ms(1200);
                    let ov = h.get_webview_window("overlay-1").expect("overlay-1");
                    let _ = ov.eval(watch_csp);
                    probe(&ov, "overlay_info", "settings_save");
                    ms(800);
                    // 2. Editor, from a real selection.
                    let mev = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,\
                         clientX:{x},clientY:{y},button:0}}));}})()");
                    let _ = ov.eval(&mev("mousedown", 200.0, 150.0));
                    for i in 1..=6 { let _ = ov.eval(&mev("mousemove", 200.0 + 60.0 * i as f64, 150.0 + 40.0 * i as f64)); ms(40); }
                    let _ = ov.eval(&mev("mouseup", 560.0, 390.0));
                    ms(400);
                    probe(&ov, "overlay_info", "settings_save");
                    ms(600);
                    match commit(&h, &st, false) {
                        Ok(label) => {
                            ms(2000);
                            let ed = h.get_webview_window(&label).expect("editor");
                            let _ = ed.eval(watch_csp);
                            probe(&ed, "editor_info", "settings_save");
                            ms(800);
                            probe(&ed, "editor_info", "hotkeys_suspend");
                            ms(600);
                            let _ = h.emit_to(label.as_str(), "editor-menu", "save".to_string());
                            ms(2000);
                        }
                        Err(e) => println!("[sec] editor commit FAILED {e:?}"),
                    }
                    ms(1000);
                    // 3. Settings.
                    let _ = open_settings(h.clone());
                    ms(2000);
                    if let Some(w) = h.get_webview_window(SETTINGS_WINDOW) {
                        let _ = w.eval(watch_csp);
                        probe(&w, "settings_get", "recorder_start");
                    }
                    ms(800);
                    // 4. Playback window, on a sample file copied into the working folder.
                    if let Ok(sample) = std::env::var("BHC_SAMPLE_MP4") {
                        let dst = recording::recordings_dir().join("sec-probe.mp4");
                        let _ = std::fs::copy(&sample, &dst);
                        match recording::open_video(&h, dst, None) {
                            Ok(label) => {
                                ms(2500);
                                let w = h.get_webview_window(&label).expect("video");
                                let _ = w.eval(watch_csp);
                                ms(1500);
                                probe(&w, "video_info", "editor_save");
                                ms(600);
                                let _ = h.emit_to(label.as_str(), "editor-menu", "save".to_string());
                                ms(2000);
                            }
                            Err(e) => println!("[sec] video open FAILED {e}"),
                        }
                    }
                    // 6. Updates: a check from Settings, then the download (test env stops before the installer).
                    if std::env::var_os("BHC_UPDATE_AS_VERSION").is_some() {
                        if let Some(w) = h.get_webview_window(SETTINGS_WINDOW) {
                            let _ = w.eval("window.__TAURI_INTERNALS__.invoke('update_check')");
                            ms(6000);
                            let _ = w.eval("window.__TAURI_INTERNALS__.invoke('update_info').then(v=>window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'sec-probe',payload:{upd:v,text:document.body.innerText.split('Updates')[1]?.slice(0,300)}}))");
                            ms(1000);
                            let _ = w.eval("window.__TAURI_INTERNALS__.invoke('update_install').then(()=>'ok',e=>'ERR '+e).then(r=>window.__TAURI_INTERNALS__.invoke('update_info').then(v=>window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'sec-probe',payload:{install:r,upd:v}})))");
                            ms(30000);
                        }
                    }
                    // 5. The recording bar: set up a recording (nothing is captured until Start) and cancel.
                    open_selector(&h, true);
                    ms(1200);
                    let _ = ov.eval(&mev("mousedown", 200.0, 150.0));
                    for i in 1..=6 { let _ = ov.eval(&mev("mousemove", 200.0 + 60.0 * i as f64, 150.0 + 40.0 * i as f64)); ms(40); }
                    let _ = ov.eval(&mev("mouseup", 560.0, 390.0));
                    ms(400);
                    match commit(&h, &st, false) {
                        Ok(_) => {
                            ms(2000);
                            if let Some(w) = h.get_webview_window(recording::HUD) {
                                let _ = w.eval(watch_csp);
                                probe(&w, "recorder_info", "settings_save");
                                ms(800);
                                let _ = w.eval("window.__TAURI_INTERNALS__.invoke('recorder_cancel')");
                            } else { println!("[sec] no recorder bar"); }
                        }
                        Err(e) => println!("[sec] video commit FAILED {e:?}"),
                    }
                    ms(2500);
                    h.exit(0);
                });
            }

            // Camera bubble: set up a recording with the camera on, measure the bubble's frame rate,
            // record 4 s, stop. Run under a throwaway HOME; the user allows the camera once.
            if std::env::args().any(|a| a == "--demo-camera") {
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    {
                        let mut g = st.settings.lock().unwrap();
                        let s = g.get().clone();
                        let bg = std::env::var("BHC_CAM_BG").unwrap_or_else(|_| "blur".into());
                        g.set(Settings { camera_enabled: true, camera_background: bg, camera_shape: std::env::var("BHC_CAM_SHAPE").unwrap_or_else(|_| "circle".into()),
                                         record_countdown: false, record_microphone: std::env::var_os("BHC_CAM_AUDIO").is_some(), record_system_audio: std::env::var_os("BHC_CAM_AUDIO").is_some(), ..s });
                    }
                    ms(2500);
                    open_selector(&h, true);
                    ms(1200);
                    let ov = h.get_webview_window("overlay-1").expect("overlay-1");
                    let mev = |t: &str, x: f64, y: f64| format!(
                        "(()=>{{const el=document.elementFromPoint({x},{y})||document.body;\
                         el.dispatchEvent(new MouseEvent('{t}',{{bubbles:true,cancelable:true,clientX:{x},clientY:{y},button:0}}));}})()");
                    let _ = ov.eval(&mev("mousedown", 100.0, 100.0));
                    for i in 1..=6 { let _ = ov.eval(&mev("mousemove", 100.0 + 120.0 * i as f64, 100.0 + 80.0 * i as f64)); ms(40); }
                    let _ = ov.eval(&mev("mouseup", 820.0, 580.0));
                    ms(400);
                    if let Err(e) = commit(&h, &st, false) { println!("[camtest] commit FAILED {e:?}"); h.exit(1); return; }
                    println!("[camtest] bubble open = {}", camera_bubble::is_open(&h));
                    // Wait for permission + the first frames (up to 60 s for the user to click Allow).
                    let t0 = std::time::Instant::now();
                    while camera_bubble::FRAMES_SENT.load(Ordering::Relaxed) == 0 && t0.elapsed().as_secs() < 60 { ms(200); }
                    let a = camera_bubble::FRAMES_SENT.load(Ordering::Relaxed);
                    let ai = camera_bubble::FRAMES_IN.load(Ordering::Relaxed);
                    println!("[camtest] first frame after {:.1}s", t0.elapsed().as_secs_f64());
                    ms(3000);
                    let b = camera_bubble::FRAMES_SENT.load(Ordering::Relaxed);
                    let bi = camera_bubble::FRAMES_IN.load(Ordering::Relaxed);
                    println!("[camtest] bubble frame rate: {:.1} fps (camera delivered {:.1} fps)", (b - a) as f64 / 3.0, (bi - ai) as f64 / 3.0);
                    if let Some(w) = h.get_webview_window(camera_bubble::CAMERA_WINDOW) {
                        let _ = w.eval("(()=>{const c=document.querySelector('canvas');const d=c.getContext('2d').getImageData(0,0,c.width,c.height).data;let n=0,s=0;for(let i=0;i<d.length;i+=4){s+=d[i]+d[i+1]+d[i+2];n++;}window.__TAURI_INTERNALS__.invoke('plugin:event|emit',{event:'sec-probe',payload:{canvas:[c.width,c.height],mean:Math.round(s/n/3),shape:getComputedStyle(document.querySelector('.cam')).borderRadius,msg:document.body.innerText}})})()");
                    }
                    use tauri::Listener;
                    h.listen_any("sec-probe", |e| println!("[camtest] page {}", e.payload()));
                    ms(800);
                    let r = tauri::async_runtime::block_on(recording::recorder_start(h.clone(), h.state::<Arc<AppState>>()));
                    println!("[camtest] record start: {r:?}");
                    let secs: u64 = std::env::var("BHC_CAM_REC_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
                    for i in 0..secs {
                        let (a0, b0) = (camera_bubble::FRAMES_IN.load(Ordering::Relaxed), camera_bubble::FRAMES_SENT.load(Ordering::Relaxed));
                        ms(1000);
                        if (i + 1) % 60 == 0 || i < 3 { println!("[camtest] rec {:>4}s: camera {} fps, sent {} fps", i + 1,
                            camera_bubble::FRAMES_IN.load(Ordering::Relaxed) - a0, camera_bubble::FRAMES_SENT.load(Ordering::Relaxed) - b0); }
                    }
                    let t_stop = std::time::Instant::now();
                    let r = tauri::async_runtime::block_on(recording::recorder_stop(h.clone()));
                    println!("[camtest] stopping + saving took {:.1} s", t_stop.elapsed().as_secs_f64());
                    ms(1000);
                    println!("[camtest] record stop: {r:?}; bubble still open 1 s later = {}", camera_bubble::is_open(&h));
                    // Second take: the hidden bubble is reused and its camera starts again.
                    open_selector(&h, true);
                    ms(1200);
                    let _ = ov.eval(&mev("mousedown", 150.0, 120.0));
                    for i in 1..=6 { let _ = ov.eval(&mev("mousemove", 150.0 + 100.0 * i as f64, 120.0 + 70.0 * i as f64)); ms(40); }
                    let _ = ov.eval(&mev("mouseup", 750.0, 540.0));
                    ms(400);
                    let before = camera_bubble::FRAMES_SENT.load(Ordering::Relaxed);
                    println!("[camtest] second commit: {:?}", commit(&h, &st, false).map(|_| ()));
                    ms(4000);
                    let after = camera_bubble::FRAMES_SENT.load(Ordering::Relaxed);
                    println!("[camtest] second take: bubble open = {}, frames in 4 s = {}", camera_bubble::is_open(&h), after - before);
                    let _ = tauri::async_runtime::block_on(recording::recorder_cancel(h.clone()));
                    ms(1000);
                    let c0 = camera_bubble::FRAMES_SENT.load(Ordering::Relaxed);
                    ms(1500);
                    println!("[camtest] after cancel: bubble open = {}, frames in 1.5 s = {} (expect 0: camera off)",
                        camera_bubble::is_open(&h), camera_bubble::FRAMES_SENT.load(Ordering::Relaxed) - c0);
                    // Hammer the Camera switch: on/off/on/off with no pause, ending OFF.
                    let r = LogRect::new(150.0, 120.0, 600.0, 420.0);
                    for i in 0..6 { camera_bubble::request(&h, i % 2 == 0, Some(r)); }
                    ms(4000);
                    let c1 = camera_bubble::FRAMES_SENT.load(Ordering::Relaxed);
                    ms(1500);
                    println!("[camtest] after rapid toggles ending off: bubble open = {}, frames in 1.5 s = {} (expect 0)",
                        camera_bubble::is_open(&h), camera_bubble::FRAMES_SENT.load(Ordering::Relaxed) - c1);
                    ms(1500);
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

            // A display going away (monitor unplugged / switched to another input): the real
            // `sync_overlays` path, with the current display replaced by one with a new id, so the
            // old display's overlay is destroyed and a new one created. Crashed 1.1.5 (2026-10-25).
            if std::env::args().any(|a| a == "--demo-display-gone") {
                #[cfg(target_os = "macos")]
                bhcapture_capture::macos::log_objc_exceptions();
                let (h, st) = (handle.clone(), state.clone());
                std::thread::spawn(move || {
                    let ms = |n| std::thread::sleep(std::time::Duration::from_millis(n));
                    ms(2500);
                    // Away and back three times: a monitor switched to another input and back.
                    for round in 0..6 {
                        let shift: i64 = if round % 2 == 0 { 1000 } else { -1000 };
                        let moved: Vec<_> = st.layout.lock().unwrap().displays().iter()
                            .map(|d| { let mut d = d.clone(); d.id = DisplayId((d.id.0 as i64 + shift) as u32); d }).collect();
                        println!("[gone] round {round}: overlays before: {:?}",
                            h.webview_windows().keys().filter(|l| l.starts_with(OVERLAY_PREFIX)).collect::<Vec<_>>());
                        *st.layout.lock().unwrap() = bhcapture_core::geom::DisplayLayout::new(moved);
                        let (a, s2) = (h.clone(), st.clone());
                        let _ = h.run_on_main_thread(move || {
                            if let Err(e) = sync_overlays(&a, &s2) { eprintln!("[gone] sync failed: {e}"); }
                        });
                        ms(1500);
                    }
                    // The selector still opens on the overlay that exists now.
                    open_selector(&h, false);
                    ms(1200);
                    println!("[gone] selector visible after the changes: {:?}",
                        h.webview_windows().iter().filter(|(l, _)| l.starts_with(OVERLAY_PREFIX))
                            .map(|(l, w)| (l.clone(), w.is_visible().unwrap_or(false))).collect::<Vec<_>>());
                    hide_overlays(&h);
                    ms(800);
                    println!("[gone] SURVIVED; overlays after: {:?}",
                        h.webview_windows().keys().filter(|l| l.starts_with(OVERLAY_PREFIX)).collect::<Vec<_>>());
                    h.exit(0);
                });
            }

            // Open the image files named after this switch in editors (one each), with no capture --
            // for reproducing Save / Save As behaviour on known files.
            if let Some(pos) = std::env::args().position(|a| a == "--demo-open-images") {
                let files: Vec<std::path::PathBuf> = std::env::args().skip(pos + 1).map(Into::into).collect();
                let (h, st) = (handle.clone(), state.clone());
                tauri::async_runtime::spawn(async move {
                    for f in files {
                        match open_image(&h, &st, f.clone(), false).await {
                            Ok(()) => println!("[open-images] opened {}", f.display()),
                            Err(e) => println!("[open-images] FAILED {}: {e}", f.display()),
                        }
                    }
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
        .run(|app, event| {
            // Quitting mid-recording finishes the file, so it is playable and offered next launch.
            if let tauri::RunEvent::Exit = event {
                recording::finish_on_exit();
                // Undo every panel conversion before shutdown tears the windows and their web views
                // down -- the same WebKit teardown that aborted 1.1.5 when a display went away.
                // (A window that was never converted is left as it is.) Exit runs on the main thread.
                for (_, w) in app.webview_windows() {
                    if let Some(p) = native_handle(&w) { unsafe { platform::release_tool_window(p); } }
                }
            }
            // Opening BHCapture again while it runs (Dock, Launchpad, Finder, Spotlight): bring back
            // open captures and recordings, nothing else (see `reopen`).
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event { reopen(app); }
            #[cfg(not(target_os = "macos"))]
            let _ = app;
        });
}
