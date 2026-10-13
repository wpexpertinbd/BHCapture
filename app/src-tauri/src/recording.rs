//! Screen recording: the selector in record mode, the control bar ("HUD"), and the playback window.
//!
//! Flow: record shortcut / tray → the same region selector as screenshots → Enter → the control bar
//! appears under the region with its options (cursor, system audio, microphone, countdown) → Record
//! → countdown → recording, with Pause and Stop → the finished MP4 opens in a playback window, where
//! the user saves it (File › Save / Save As) or closes it. Like a screenshot (decision 11), nothing
//! lands in the user's folders until they save.
//!
//! While recording, the file is written to a private folder (`…/Application Support/BHCapture/
//! Recordings`, 0700) AS IT IS ENCODED, never held in memory. A file left there by a crash or a quit
//! mid-recording is reopened at the next launch rather than silently lost.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bhcapture_capture::macos::{microphone_supported, MacCapturer, MacRecorder};
use bhcapture_capture::record::{RecordOptions, RecordState};
use bhcapture_core::geom::{DisplayId, LogRect, LogSize};
use bhcapture_core::naming::{unique_path, Fields, Pattern};
use bhcapture_core::record::{place_bar, record_target};
use serde::Serialize;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;

use crate::{broadcast, default_capture_dir, hide_overlays, local_stamp, release_frozen, sync_dock,
            AppState, CommitError, MainThreadOnly, OVERLAY_PREFIX, PERMISSION_MESSAGE};

pub(crate) const HUD: &str = "recorder";
pub(crate) const VIDEO_PREFIX: &str = "video-";
/// Wide enough for the options row; tall enough for an error line under the bar.
const HUD_SIZE: LogSize = LogSize { w: 760.0, h: 96.0 };

/// The selector is shared by screenshots and recordings; this says which one Enter starts.
pub(crate) static VIDEO_MODE: AtomicBool = AtomicBool::new(false);

/// The tray's record item, so it can read "Stop Recording" while one runs.
pub(crate) static TRAY_RECORD_ITEM: Mutex<Option<MainThreadOnly<tauri::menu::MenuItem<tauri::Wry>>>> = Mutex::new(None);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase { Ready, Recording, Paused, Finishing }

struct Session {
    display: DisplayId,
    local: LogRect,
    /// Pixels the file will have (before the encoder's even-size rounding), for the bar's readout.
    px: (u32, u32),
    phase: Phase,
    recorder: Option<MacRecorder>,
    file: Option<PathBuf>,
    /// System audio AND microphone: two tracks that must be mixed into one when finished.
    two_tracks: bool,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// One finished recording, open in a playback window. `file` is the private working copy; a save
/// COPIES it out, so the window keeps playing from a path that never changes.
struct VideoDoc { file: PathBuf, saved_to: Option<PathBuf>, note: Option<String> }

static VIDEOS: Mutex<Option<HashMap<u64, VideoDoc>>> = Mutex::new(None);
static NEXT_VIDEO: AtomicU64 = AtomicU64::new(0);

fn videos<R>(f: impl FnOnce(&mut HashMap<u64, VideoDoc>) -> R) -> R {
    let mut g = VIDEOS.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

/// `~/Library/Application Support/BHCapture/Recordings`, private (0700).
pub(crate) fn recordings_dir() -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let dir = home.join("Library").join("Application Support").join("BHCapture").join("Recordings");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)); }
    dir
}

fn set_tray_label(app: &tauri::AppHandle, recording: bool) {
    let _ = app.run_on_main_thread(move || {
        if let Some(it) = TRAY_RECORD_ITEM.lock().unwrap().as_ref() {
            let _ = it.0.set_text(if recording { "Stop Recording" } else { "Record Video" });
        }
    });
}

fn emit_state(app: &tauri::AppHandle) { let _ = app.emit_to(HUD, "recorder-state", ()); }

/// The record shortcut / tray item. Context-sensitive, so ONE key drives the whole thing:
/// nothing running → open the selector; bar waiting → start; recording → stop.
pub(crate) fn record_shortcut(app: &tauri::AppHandle) {
    let phase = SESSION.lock().unwrap().as_ref().map(|s| s.phase);
    match phase {
        None => crate::open_selector(app, true),
        // The bar runs the countdown (it is what the user is looking at), so ask it to start.
        Some(Phase::Ready) => { let _ = app.emit_to(HUD, "recorder-start-requested", ()); }
        Some(Phase::Recording | Phase::Paused) => {
            let a = app.clone();
            std::thread::spawn(move || { let _ = stop(&a); });
        }
        Some(Phase::Finishing) => {}
    }
}

/// Enter in the selector, in record mode: put the control bar under the region. Nothing is recorded
/// yet -- the user still chooses audio and presses Record.
pub(crate) fn prepare(app: &tauri::AppHandle, state: &Arc<AppState>, rect: LogRect) -> Result<String, CommitError> {
    if !MacCapturer::has_permission() {
        return Err(CommitError::Permission(PERMISSION_MESSAGE.into()));
    }
    if SESSION.lock().unwrap().is_some() {
        return Err(CommitError::Failed("A recording is already in progress.".into()));
    }
    let layout = state.layout.lock().unwrap().clone();
    let target = record_target(&layout, rect).ok_or_else(|| CommitError::Empty(
        "Select a larger area on one screen to record.".into()))?;
    let d = layout.get(target.display).cloned().ok_or_else(|| CommitError::Failed("display is gone".into()))?;
    let px = ((target.global.size.w * d.scale.get()).round() as u32, (target.global.size.h * d.scale.get()).round() as u32);

    // The selector's job is done.
    *state.drag.lock().unwrap() = None;
    release_frozen(state);
    broadcast(app, None);
    hide_overlays(app);

    *SESSION.lock().unwrap() = Some(Session {
        display: target.display, local: target.local, px,
        phase: Phase::Ready, recorder: None, file: None, two_tracks: false,
    });
    show_frame(app, target.display, Some(target.global));
    let at = place_bar(d.bounds, target.global, HUD_SIZE);
    open_hud(app, at.x, at.y).map_err(CommitError::Failed)?;
    println!("[record] ready: {}x{} pt at ({},{}) on display {} -> ~{}x{} px",
        target.global.size.w, target.global.size.h, target.global.min_x(), target.global.min_y(), target.display.0, px.0, px.1);
    Ok(HUD.to_string())
}

/// Outline the region being recorded, using that display's overlay with the mouse passed THROUGH it
/// (the user must be able to work in the region). The overlay is left out of the recording because
/// the capture filter excludes this whole app. `None` takes the outline down everywhere.
fn show_frame(app: &tauri::AppHandle, display: DisplayId, rect: Option<LogRect>) {
    let payload = rect.map(|r| [r.min_x(), r.min_y(), r.size.w, r.size.h]);
    let label = format!("{OVERLAY_PREFIX}{}", display.0);
    let a = app.clone();
    // The page switches to frame mode first, so the window never flashes the selector's dim scrim.
    let _ = app.emit("record-frame", payload);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(60));
        let b = a.clone();
        let _ = a.run_on_main_thread(move || {
            for (l, w) in b.webview_windows() {
                if !l.starts_with(OVERLAY_PREFIX) { continue; }
                if payload.is_some() && l == label {
                    let _ = w.set_ignore_cursor_events(true);
                    let _ = w.show();
                } else if payload.is_none() {
                    let _ = w.hide();
                    let _ = w.set_ignore_cursor_events(false);
                }
            }
        });
    });
}

fn open_hud(app: &tauri::AppHandle, x: f64, y: f64) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let a = app.clone();
    app.run_on_main_thread(move || {
        let w = match a.get_webview_window(HUD) {
            Some(w) => Ok(w),
            None => WebviewWindowBuilder::new(&a, HUD, WebviewUrl::App("index.html".into()))
                .title("BHCapture recorder")
                .inner_size(HUD_SIZE.w, HUD_SIZE.h)
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .resizable(false)
                .shadow(false)
                .visible(false)
                // Without this the first click on the bar only focuses it (WebKit swallows it), so
                // Record needed two presses -- found by a real-click test.
                .accept_first_mouse(true)
                .build()
                .map(|w| {
                    // A non-activating panel: clicking the bar must not pull focus away from the app
                    // being recorded. (Never call set_focusable on it -- see bhc_make_overlay_panel.)
                    #[cfg(target_os = "macos")]
                    if let Ok(ptr) = w.ns_window() { unsafe { bhcapture_capture::macos::make_overlay_panel(ptr); } }
                    w
                }),
        };
        let _ = tx.send(w.map(|w| {
            let _ = w.set_position(tauri::LogicalPosition::new(x, y));
            let _ = w.show();
        }).map_err(|e| e.to_string()));
    }).map_err(|e| e.to_string())?;
    let r = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or_else(|_| Err("timed out opening the recorder".into()));
    emit_state(app);
    r
}

fn hide_hud(app: &tauri::AppHandle) {
    let a = app.clone();
    let _ = app.run_on_main_thread(move || { if let Some(w) = a.get_webview_window(HUD) { let _ = w.hide(); } });
}

#[derive(Serialize)]
pub(crate) struct HudInfo {
    phase: Option<Phase>,
    width: u32,
    height: u32,
    cursor: bool,
    system_audio: bool,
    microphone: bool,
    countdown: bool,
    mic_supported: bool,
    shortcut: String,
}

#[tauri::command]
pub(crate) fn recorder_info(state: tauri::State<Arc<AppState>>) -> HudInfo {
    let s = state.settings.lock().unwrap().get().clone();
    let g = SESSION.lock().unwrap();
    HudInfo {
        phase: g.as_ref().map(|s| s.phase),
        width: g.as_ref().map_or(0, |s| s.px.0), height: g.as_ref().map_or(0, |s| s.px.1),
        cursor: s.record_cursor, system_audio: s.record_system_audio,
        microphone: s.record_microphone && microphone_supported(), countdown: s.record_countdown,
        mic_supported: microphone_supported(), shortcut: s.record_hotkey,
    }
}

/// A toggle on the bar. Remembered, so the next recording starts the way this one was set up.
#[tauri::command]
pub(crate) fn recorder_option(state: tauri::State<Arc<AppState>>, name: String, value: bool) -> Result<(), String> {
    if SESSION.lock().unwrap().as_ref().map_or(false, |s| s.phase != Phase::Ready) {
        return Err("Options can only change before recording starts.".into());
    }
    let mut st = state.settings.lock().unwrap();
    let mut s = st.get().clone();
    match name.as_str() {
        "cursor" => s.record_cursor = value,
        "system_audio" => s.record_system_audio = value,
        "microphone" => {
            if value && !microphone_supported() { return Err("Recording the microphone needs macOS 15 or later.".into()); }
            s.record_microphone = value;
        }
        "countdown" => s.record_countdown = value,
        _ => return Err(format!("unknown option {name}")),
    }
    st.set(s);
    // An unreadable settings file refuses saves (see SettingsStore); the option still applies to
    // this recording, it just is not remembered.
    if let Err(e) = st.save() { eprintln!("[record] option not saved: {e}"); }
    Ok(())
}

/// Record (after the bar's countdown). Blocks until capture has really started, so the bar only
/// shows the timer once frames are flowing.
#[tauri::command]
pub(crate) async fn recorder_start(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    start(&app, &state)
}

pub(crate) fn start(app: &tauri::AppHandle, state: &Arc<AppState>) -> Result<(), String> {
    let (display, local, px) = {
        let g = SESSION.lock().unwrap();
        let s = g.as_ref().ok_or("Nothing to record. Select an area first.")?;
        if s.phase != Phase::Ready { return Err("Already recording.".into()); }
        (s.display, s.local, s.px)
    };
    let (pattern, opts) = {
        let st = state.settings.lock().unwrap();
        let s = st.get();
        (Pattern::parse(&s.pattern).unwrap_or_default(),
         (s.record_cursor, s.record_system_audio, s.record_microphone && microphone_supported(), s.record_fps))
    };
    let dir = recordings_dir();
    let stem = pattern.render(local_stamp(), &Fields { width: px.0, height: px.1, ..Default::default() });
    let file = unique_path(&dir, &stem, "mp4", &|p| p.exists());

    let rec = MacRecorder::start(&RecordOptions {
        display, region: local, fps: opts.3, show_cursor: opts.0,
        system_audio: opts.1, microphone: opts.2, path: file.clone(),
    }).map_err(|e| match e {
        bhcapture_capture::CaptureError::PermissionDenied => PERMISSION_MESSAGE.to_string(),
        other => format!("Could not start recording: {other}"),
    })?;
    {
        let mut g = SESSION.lock().unwrap();
        match g.as_mut() {
            Some(s) if s.phase == Phase::Ready => {
                s.recorder = Some(rec);
                s.file = Some(file.clone());
                s.two_tracks = opts.1 && opts.2;
                s.phase = Phase::Recording;
            }
            // Cancelled during the start: drop() finishes the file; throw it away.
            _ => { drop(rec); let _ = std::fs::remove_file(&file); return Err("Recording was cancelled.".into()); }
        }
    }
    println!("[record] started -> {} (system audio {}, microphone {})", file.display(), opts.1, opts.2);
    set_tray_label(app, true);
    emit_state(app);
    spawn_status(app.clone());
    Ok(())
}

#[derive(Serialize, Clone)]
struct StatusPayload { seconds: f64, level_system: f32, level_mic: f32, paused: bool }

/// Feed the bar its timer and level meters, and catch a recording that dies on its own (disk full,
/// display unplugged) -- that one is finished and opened like a normal stop, so what was recorded
/// is kept.
fn spawn_status(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let st = {
            let g = SESSION.lock().unwrap();
            match g.as_ref() {
                Some(s) if matches!(s.phase, Phase::Recording | Phase::Paused) => s.recorder.as_ref().map(|r| r.status()),
                _ => return,
            }
        };
        let Some(st) = st else { return };
        if st.state == RecordState::Failed {
            eprintln!("[record] stopped by an error: {:?}", st.error);
            let _ = stop(&app);
            return;
        }
        let _ = app.emit_to(HUD, "recorder-status", StatusPayload {
            seconds: st.seconds, level_system: st.level_system, level_mic: st.level_mic,
            paused: st.state == RecordState::Paused,
        });
    });
}

#[tauri::command]
pub(crate) fn recorder_pause(app: tauri::AppHandle, paused: bool) -> Result<(), String> {
    let mut g = SESSION.lock().unwrap();
    let s = g.as_mut().ok_or("Not recording.")?;
    if !matches!(s.phase, Phase::Recording | Phase::Paused) { return Err("Not recording.".into()); }
    if let Some(r) = &s.recorder { r.set_paused(paused); }
    s.phase = if paused { Phase::Paused } else { Phase::Recording };
    drop(g);
    emit_state(&app);
    Ok(())
}

#[tauri::command]
pub(crate) async fn recorder_stop(app: tauri::AppHandle) -> Result<(), String> { stop(&app) }

/// Test harness: record a region for `secs` while the harness plays sound, then stop.
#[cfg(feature = "demo")]
pub(crate) fn demo_record(app: &tauri::AppHandle, state: &Arc<AppState>, secs: u64) {
    let r = prepare(app, state, LogRect::new(200.0, 150.0, 600.0, 400.0));
    println!("[rectest] prepare: {:?}", r.as_ref().map(|_| ()).map_err(|e| e.to_string()));
    std::thread::sleep(std::time::Duration::from_millis(800));
    println!("[rectest] start: {:?}", start(app, state));
    println!("[rectest] RECORDING");
    let mut peak_sys = 0f32;
    for _ in 0..(secs * 5) {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Some(st) = SESSION.lock().unwrap().as_ref().and_then(|s| s.recorder.as_ref().map(|r| r.status())) {
            peak_sys = peak_sys.max(st.level_system);
        }
    }
    println!("[rectest] peak system level seen by the meter: {peak_sys:.3}");
    println!("[rectest] stop: {:?}", stop(app));
}

/// Finish the file and open it for playback. Safe to call twice; the second does nothing.
fn stop(app: &tauri::AppHandle) -> Result<(), String> {
    let (rec, file, two_tracks) = {
        let mut g = SESSION.lock().unwrap();
        let Some(s) = g.as_mut() else { return Ok(()) };
        if !matches!(s.phase, Phase::Recording | Phase::Paused) { return Ok(()); }
        s.phase = Phase::Finishing;
        (s.recorder.take(), s.file.clone(), s.two_tracks)
    };
    emit_state(app);
    let result = rec.map(|r| r.stop()).unwrap_or(Ok(()));
    if let (Ok(()), true, Some(f)) = (&result, two_tracks, &file) { mix_into_one_track(f); }
    end_session(app);

    let Some(file) = file else { return Ok(()) };
    let usable = std::fs::metadata(&file).map(|m| m.len() > 0).unwrap_or(false);
    match (&result, usable) {
        (Ok(()), true) => { open_video(app, file, None)?; Ok(()) }
        // Cut short (disk full, display gone): keep and show what was written, and say why.
        (Err(e), true) => {
            open_video(app, file, Some(format!("The recording stopped early: {e}. What was recorded up to then is kept.")))?;
            Ok(())
        }
        (Err(e), false) => {
            let msg = format!("The recording could not be saved: {e}");
            app.dialog().message(msg.clone()).title("BHCapture").show(|_| {});
            Err(msg)
        }
        (Ok(()), false) => Err("The recording is empty.".into()),
    }
}

/// System audio and the microphone are recorded as two tracks; most players (browsers, upload sites)
/// play only the first, so mix them into one. On failure the two-track file is kept -- nothing is
/// lost, it just plays fully only in players that mix tracks themselves (QuickTime does).
fn mix_into_one_track(file: &Path) {
    let tmp = file.with_extension("mixing.mp4");
    match bhcapture_capture::macos::mix_audio_tracks(file, &tmp).and_then(|()| std::fs::rename(&tmp, file)
            .map_err(|e| bhcapture_capture::CaptureError::Backend(e.to_string()))) {
        Ok(()) => println!("[record] mixed system audio and microphone into one track"),
        Err(e) if e.to_string().contains("nothing to mix") => { let _ = std::fs::remove_file(&tmp); }
        Err(e) => { let _ = std::fs::remove_file(&tmp); eprintln!("[record] could not mix the audio tracks ({e}); kept both"); }
    }
}

/// Cancel: before recording, just close the bar; while recording, stop and throw the file away.
#[tauri::command]
pub(crate) async fn recorder_cancel(app: tauri::AppHandle) -> Result<(), String> {
    let (rec, file) = {
        let mut g = SESSION.lock().unwrap();
        let Some(s) = g.as_mut() else { return Ok(()) };
        if s.phase == Phase::Finishing { return Ok(()); }
        s.phase = Phase::Finishing;
        (s.recorder.take(), s.file.clone())
    };
    if let Some(r) = rec { let _ = r.stop(); }
    if let Some(f) = file { let _ = std::fs::remove_file(f); }
    end_session(&app);
    println!("[record] cancelled");
    Ok(())
}

fn end_session(app: &tauri::AppHandle) {
    let display = SESSION.lock().unwrap().take().map(|s| s.display);
    hide_hud(app);
    if let Some(d) = display { show_frame(app, d, None); }
    set_tray_label(app, false);
}

/// Called when the app quits: a running recording is finished, so its file stays playable and is
/// offered again at the next launch.
pub(crate) fn finish_on_exit() {
    let rec = SESSION.lock().unwrap().as_mut().and_then(|s| s.recorder.take());
    if let Some(r) = rec { let _ = r.stop(); }
}

// ---------------------------------------------------------------------------------------------
// Playback window
// ---------------------------------------------------------------------------------------------

/// A window went away without `video_close` (e.g. the app quit): forget it, but KEEP the file, so
/// it is offered again at the next launch.
pub(crate) fn forget_video(id: u64) { videos(|v| { v.remove(&id); }); }

/// A recording is being set up or is running.
pub(crate) fn busy() -> bool { SESSION.lock().unwrap().is_some() }

pub(crate) fn video_id_of(label: &str) -> Result<u64, String> {
    label.strip_prefix(VIDEO_PREFIX).and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("{label} is not a recording window"))
}

pub(crate) fn open_video(app: &tauri::AppHandle, file: PathBuf, note: Option<String>) -> Result<String, String> {
    let id = NEXT_VIDEO.fetch_add(1, Ordering::SeqCst) + 1;
    videos(|v| v.insert(id, VideoDoc { file, saved_to: None, note }));
    let label = format!("{VIDEO_PREFIX}{id}");
    let (a, l) = (app.clone(), label.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        let r = WebviewWindowBuilder::new(&a, &l, WebviewUrl::App("index.html".into()))
            .title("BHCapture — Recording")
            .inner_size(960.0, 640.0)
            .min_inner_size(520.0, 360.0)
            .center()
            .resizable(true)
            .build();
        let _ = tx.send(match r {
            Ok(w) => { sync_dock(&a, None); let _ = w.set_focus(); Ok(()) }
            Err(e) => Err(e.to_string()),
        });
    }).map_err(|e| e.to_string())?;
    let built = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or_else(|_| Err("timed out".into()));
    if let Err(e) = built {
        videos(|v| v.remove(&id)); // the file stays in Recordings and is offered at next launch
        return Err(format!("Could not open the recording: {e}"));
    }
    println!("[record] opened {label}");
    Ok(label)
}

#[derive(Serialize)]
pub(crate) struct VideoInfo { file: String, saved_to: Option<String>, note: Option<String> }

#[tauri::command]
pub(crate) fn video_info(window: tauri::Window) -> Result<VideoInfo, String> {
    let id = video_id_of(window.label())?;
    videos(|v| v.get(&id).map(|d| VideoInfo {
        file: d.file.display().to_string(),
        saved_to: d.saved_to.as_ref().map(|p| p.display().to_string()),
        note: d.note.clone(),
    })).ok_or_else(|| "this recording is no longer open".into())
}

/// Save a COPY of the recording to the default folder, or wherever the Save dialog says. The path
/// is chosen here, never by the page.
#[tauri::command]
pub(crate) async fn video_save(app: tauri::AppHandle, window: tauri::WebviewWindow, state: tauri::State<'_, Arc<AppState>>,
                               save_as: bool) -> Result<String, CommitError> {
    let id = video_id_of(window.label()).map_err(CommitError::Failed)?;
    let src = videos(|v| v.get(&id).map(|d| d.file.clone()))
        .ok_or_else(|| CommitError::Failed("this recording is no longer open".into()))?;
    let (pattern, out_dir, ask) = {
        let st = state.settings.lock().unwrap();
        let s = st.get();
        (Pattern::parse(&s.pattern).unwrap_or_default(),
         s.output_dir.clone().unwrap_or_else(default_capture_dir), s.always_ask_where_to_save)
    };
    // The working file was named from the pattern when recording started, so reuse its name.
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| pattern.render(local_stamp(), &Fields::default()));

    let dest = if save_as || ask {
        let d = app.dialog().file()
            .set_title("Save recording")
            .set_directory(&out_dir)
            .set_file_name(format!("{stem}.mp4"))
            .add_filter("MPEG-4 Video", &["mp4"])
            .set_parent(&window);
        match d.blocking_save_file().map(|p| p.into_path()) {
            Some(Ok(mut p)) => {
                if p.extension().map_or(true, |e| !e.eq_ignore_ascii_case("mp4")) {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    p.set_file_name(format!("{name}.mp4"));
                }
                p
            }
            Some(Err(e)) => return Err(CommitError::Failed(e.to_string())),
            None => return Err(CommitError::Cancelled("Save cancelled.".into())),
        }
    } else {
        let _ = std::fs::create_dir_all(&out_dir);
        unique_path(&out_dir, &stem, "mp4", &|p| p.exists())
    };
    if dest == src { return Err(CommitError::Failed("Choose a folder other than BHCapture's working folder.".into())); }
    copy_private(&src, &dest).map_err(|e| CommitError::Failed(format!("Could not save the recording: {e}")))?;
    videos(|v| if let Some(d) = v.get_mut(&id) { d.saved_to = Some(dest.clone()); });
    println!("[record] {} saved to {}", window.label(), dest.display());
    Ok(dest.display().to_string())
}

/// Copy readable only by this user (0600), like every capture (see `write_private`).
fn copy_private(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dest)?;
    #[cfg(unix)]
    { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?; }
    Ok(())
}

/// Close for good (the page has already asked about an unsaved recording). Deletes the working copy:
/// a saved recording lives on as the user's copy, an unsaved one was explicitly discarded.
#[tauri::command]
pub(crate) fn video_close(window: tauri::WebviewWindow) -> Result<(), String> {
    let id = video_id_of(window.label())?;
    if let Some(d) = videos(|v| v.remove(&id)) {
        // Only ever delete inside our own working folder.
        if d.file.parent() == Some(recordings_dir().as_path()) { let _ = std::fs::remove_file(&d.file); }
    }
    window.destroy().map_err(|e| e.to_string())
}

/// Recordings left in the working folder by a crash or a quit (never saved, never discarded):
/// reopen them so they are not lost silently. At most a few, newest first.
pub(crate) fn reopen_leftovers(app: &tauri::AppHandle) {
    let dir = recordings_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "mp4"))
        // A half-made mix is a by-product; the recording itself is the file next to it.
        .filter(|p| !p.to_string_lossy().ends_with(".mixing.mp4"))
        .filter_map(|p| std::fs::metadata(&p).ok().filter(|m| m.len() > 0).and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, f) in files.into_iter().take(5) {
        // A recording cut off by a quit or crash never reached the mixing step, so it can still
        // hold system audio and the microphone as two tracks, and players use only the first.
        // Mixing a one-track file just fails harmlessly ("nothing to mix") and leaves it alone.
        mix_into_one_track(&f);
        let _ = open_video(app, f, Some("Recovered: this recording was not saved before BHCapture last quit.".into()));
    }
}
