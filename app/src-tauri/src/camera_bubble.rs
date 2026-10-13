//! The camera bubble (decisions 34/35): a small transparent always-on-top window showing the
//! camera, placed inside the recorded area and let INTO the recording, so the camera is in the
//! video exactly where the user put it -- Snagit/Camtasia's model, and Cap's "instant" mode.
//!
//! The camera is captured natively (`platform::Camera`); every frame is rendered here at the
//! bubble's pixel size by `bhcapture_core::camera` (crop to shape, mirror, enhance, background)
//! and pushed to the window's canvas over an IPC channel. The shape itself is CSS on that canvas.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bhcapture_capture::camera::{CameraDevice, CameraFrame, CameraPermission};
use bhcapture_capture::platform;
use bhcapture_core::camera::{self as cam, Background, FrameOptions, Mask, Source};
use bhcapture_core::geom::LogRect;
use bhcapture_core::settings::Settings;
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::AppState;

pub(crate) const CAMERA_WINDOW: &str = "camera";
/// Gap between the bubble and the edge of the recorded area, in points.
const MARGIN: f64 = 16.0;
/// Frames pushed to the bubble at most this often.
const MAX_FPS: u32 = 30;

struct Run {
    camera: Option<platform::Camera>,
    opts: Arc<Mutex<FrameOptions>>,
}
static RUN: Mutex<Option<Run>> = Mutex::new(None);
/// The bubble page's frame channel. The window is HIDDEN and reused, never destroyed: it is turned
/// into a non-activating panel (object_setClass), and destroying such a window aborts the app
/// ("Rust cannot catch foreign exceptions" -- found by the camera test, twice).
static CHANNEL: Mutex<Option<Channel<InvokeResponseBody>>> = Mutex::new(None);
/// Whether the bubble is meant to be on screen now.
static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Frames sent to the bubble (test harness reads it to measure the frame rate).
pub(crate) static FRAMES_SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The bubble's size in points for a shape and height.
fn bubble_size(s: &Settings) -> (f64, f64) {
    let h = s.camera_size as f64;
    if s.camera_shape == "wide" { (h * 16.0 / 9.0, h) } else { (h, h) }
}

/// Where the bubble starts: the chosen corner of the recorded area (global logical points).
fn corner_position(s: &Settings, region: LogRect) -> (f64, f64) {
    let (w, h) = bubble_size(s);
    let left = region.min_x() + MARGIN;
    let right = region.min_x() + region.size.w - w - MARGIN;
    let top = region.min_y() + MARGIN;
    let bottom = region.min_y() + region.size.h - h - MARGIN;
    let (x, y) = match s.camera_corner.as_str() {
        "bottom-left" => (left, bottom), "top-right" => (right, top), "top-left" => (left, top), _ => (right, bottom),
    };
    // A region smaller than the bubble: pin to its top-left rather than outside it.
    (x.max(region.min_x()), y.max(region.min_y()))
}

/// The background as pixels at the output size (decoded once per change, not per frame).
fn background_for(s: &Settings, w: u32, h: u32) -> Background {
    match s.camera_background.as_str() {
        "blur" => Background::Blur((w.max(h) / 40).max(4)),
        "color" => Background::Color(cam::parse_hex_color(&s.camera_bg_color).unwrap_or([31, 111, 235])),
        "image" => s.camera_bg_image.as_ref()
            .and_then(|p| cam::background_from_file(p, w, h))
            .map(Background::Image)
            .unwrap_or(Background::Blur((w.max(h) / 40).max(4))),
        other => other.strip_prefix("preset:").and_then(|p| cam::preset_image(p, w, h)).map(Background::Image)
            .unwrap_or(Background::Keep),
    }
}

fn frame_options(s: &Settings, scale: f64) -> FrameOptions {
    let (w, h) = bubble_size(s);
    // Pixel size, capped: a 480 pt bubble on a 2x screen is 960 px, more than any webcam delivers.
    let (ow, oh) = (((w * scale).round() as u32).min(1280), ((h * scale).round() as u32).min(720));
    FrameOptions { out_w: ow.max(2), out_h: oh.max(2), mirror: s.camera_mirror, enhance: s.camera_enhance,
                   background: background_for(s, ow.max(2), oh.max(2)) }
}

/// 0 off, 1 fast, 2 balanced: segmentation only when the background is replaced.
fn segmentation_level(s: &Settings) -> u8 { if s.camera_background == "none" { 0 } else { 2 } }

#[derive(Serialize, Clone)]
pub(crate) struct BubbleStyle { shape: String, width: f64, height: f64 }

fn style(s: &Settings) -> BubbleStyle {
    let (width, height) = bubble_size(s);
    BubbleStyle { shape: s.camera_shape.clone(), width, height }
}

/// Show the bubble in `region` (the area being recorded). Creates the window on first use.
pub(crate) fn open(app: &tauri::AppHandle, region: LogRect) -> Result<(), String> {
    let s = app.state::<Arc<AppState>>().settings.lock().unwrap().get().clone();
    let (w, h) = bubble_size(&s);
    let (x, y) = corner_position(&s, region);
    let (tx, rx) = std::sync::mpsc::channel();
    let a = app.clone();
    app.run_on_main_thread(move || {
        let win = match a.get_webview_window(CAMERA_WINDOW) {
            Some(w) => Ok(w),
            None => WebviewWindowBuilder::new(&a, CAMERA_WINDOW, WebviewUrl::App("index.html".into()))
                .title("BHCapture camera")
                .decorations(false).transparent(true).always_on_top(true).resizable(false).shadow(false)
                .skip_taskbar(true).visible(false).accept_first_mouse(true)
                .build()
                .map(|w| {
                    // Non-activating like the bar: dragging the bubble must not take focus from the
                    // app being recorded. (Windows: out of captures until the recording lets it in.)
                    if let Some(p) = crate::native_handle(&w) { unsafe { platform::prepare_tool_window(p); } }
                    w
                }),
        };
        let _ = tx.send(win.map(|win| {
            let _ = win.set_size(tauri::LogicalSize::new(w, h));
            let _ = win.set_position(tauri::LogicalPosition::new(x, y));
            let _ = win.show();
        }).map_err(|e| e.to_string()));
    }).map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| Err("timed out opening the camera".into()))?;
    SHOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = app.emit_to(CAMERA_WINDOW, "camera-style", style(&s));
    // A reused window already handed over its channel: start the camera again for it. (A new window
    // starts it through camera_attach when its page loads.)
    if RUN.lock().unwrap().is_none() {
        if let Some(ch) = CHANNEL.lock().unwrap().clone() {
            let a = app.clone();
            std::thread::spawn(move || run_camera(&a, ch));
        }
    }
    Ok(())
}

/// Close the bubble and release the camera (its light goes off).
pub(crate) fn close(app: &tauri::AppHandle) {
    SHOWN.store(false, std::sync::atomic::Ordering::SeqCst);
    let run = RUN.lock().unwrap().take();
    drop(run); // stops the camera (its light goes off), after its last frame
    let a = app.clone();
    let _ = app.run_on_main_thread(move || {
        // Hidden, never destroyed: see CHANNEL.
        if let Some(w) = a.get_webview_window(CAMERA_WINDOW) { let _ = w.hide(); }
    });
}

pub(crate) fn is_open(app: &tauri::AppHandle) -> bool {
    SHOWN.load(std::sync::atomic::Ordering::SeqCst) && app.get_webview_window(CAMERA_WINDOW).is_some()
}

/// Settings changed (Settings window or the bar): restyle and re-render a running bubble.
pub(crate) fn apply_settings(app: &tauri::AppHandle, s: &Settings) {
    let Some(win) = app.get_webview_window(CAMERA_WINDOW) else { return };
    let scale = win.scale_factor().unwrap_or(1.0);
    if let Some(run) = RUN.lock().unwrap().as_ref() {
        *run.opts.lock().unwrap() = frame_options(s, scale);
        if let Some(c) = &run.camera { c.set_segmentation(segmentation_level(s)); }
    }
    let (w, h) = bubble_size(s);
    let _ = win.set_size(tauri::LogicalSize::new(w, h));
    let _ = app.emit_to(CAMERA_WINDOW, "camera-style", style(s));
}

#[derive(Serialize)]
pub(crate) struct AttachInfo { style: BubbleStyle, permission: CameraPermission }

/// The bubble page hands over its frame channel; the camera starts (asking for permission the
/// first time -- on a worker thread, the prompt blocks).
#[tauri::command]
pub(crate) async fn camera_attach(app: tauri::AppHandle, on_frame: Channel<InvokeResponseBody>) -> Result<AttachInfo, String> {
    *CHANNEL.lock().unwrap() = Some(on_frame.clone());
    // Replace any earlier run (a reloaded page) before starting a new camera.
    drop(RUN.lock().unwrap().take());
    let a = app.clone();
    if SHOWN.load(std::sync::atomic::Ordering::SeqCst) {
        tauri::async_runtime::spawn_blocking(move || run_camera(&a, on_frame)).await.map_err(|e| e.to_string())?;
    }
    let s = app.state::<Arc<AppState>>().settings.lock().unwrap().get().clone();
    Ok(AttachInfo { style: style(&s), permission: platform::camera_permission() })
}

/// Start the camera feeding `channel` (blocks while the first-time permission prompt is up).
fn run_camera(app: &tauri::AppHandle, channel: Channel<InvokeResponseBody>) {
    let s = app.state::<Arc<AppState>>().settings.lock().unwrap().get().clone();
    let scale = app.get_webview_window(CAMERA_WINDOW).and_then(|w| w.scale_factor().ok()).unwrap_or(1.0);
    let opts = Arc::new(Mutex::new(frame_options(&s, scale)));
    let ch = Arc::new(Mutex::new(Some(channel)));
    let camera = match start_camera(&s, opts.clone(), ch) {
        Ok(c) => Some(c),
        Err(e) => { eprintln!("[camera] {e}"); let _ = app.emit_to(CAMERA_WINDOW, "camera-error", e); None }
    };
    // Closed while the camera was starting: stop it again at once.
    if !SHOWN.load(std::sync::atomic::Ordering::SeqCst) { drop(camera); return; }
    *RUN.lock().unwrap() = Some(Run { camera, opts });
}

fn start_camera(s: &Settings, opts: Arc<Mutex<FrameOptions>>, channel: Arc<Mutex<Option<Channel<InvokeResponseBody>>>>)
    -> Result<platform::Camera, String> {
    let min_gap = Duration::from_millis(1000 / MAX_FPS as u64);
    let mut last = Instant::now() - min_gap;
    let sink: bhcapture_capture::camera::FrameSink = Box::new(move |f: &CameraFrame| {
        if last.elapsed() < min_gap { return; }
        last = Instant::now();
        let o = opts.lock().unwrap().clone();
        let wants_mask = o.background != Background::Keep;
        // macOS hands over Vision's mask; elsewhere the segmentation model makes one.
        #[cfg(any(not(target_os = "macos"), feature = "demo"))]
        let own = if wants_mask && (f.mask.is_none() || compare_masks()) { model_mask(f) } else { None };
        #[cfg(not(any(not(target_os = "macos"), feature = "demo")))]
        let own: Option<Vec<u8>> = { let _ = wants_mask; None };
        #[cfg(feature = "demo")]
        if let (Some(m), Some((d, w, h, st))) = (&own, f.mask) { report_agreement(m, d, w, h, st); }
        // Vision's mask when there is one (macOS); the model's otherwise. A test build comparing
        // the two shows the model's, so it can be judged on screen.
        let model = own.as_ref().map(|m| Mask { data: m, w: SEG_W as u32, h: SEG_H as u32, stride: SEG_W });
        let vision = f.mask.map(|(d, w, h, st)| Mask { data: d, w, h, stride: st });
        let mask = if compare_masks() && model.is_some() { model } else { vision.or(model) };
        let Some(rgba) = cam::render(&Source { bgra: f.bgra, w: f.width, h: f.height, stride: f.stride }, mask.as_ref(), &o)
            else { return };
        // 8-byte header (width, height, little-endian) so the page never guesses the size.
        let mut msg = Vec::with_capacity(8 + rgba.len());
        msg.extend_from_slice(&o.out_w.to_le_bytes());
        msg.extend_from_slice(&o.out_h.to_le_bytes());
        msg.extend_from_slice(&rgba);
        if let Some(ch) = channel.lock().unwrap().as_ref() {
            if ch.send(InvokeResponseBody::Raw(msg)).is_ok() { FRAMES_SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
        }
    });
    let cam = platform::Camera::start(&s.camera_device, 1080, 30, sink).map_err(|e| match e {
        bhcapture_capture::CaptureError::PermissionDenied => "Camera access is off for BHCapture. Allow it in the system settings, then try again.".to_string(),
        other => format!("Could not start the camera: {other}"),
    })?;
    cam.set_segmentation(segmentation_level(s));
    Ok(cam)
}

#[cfg(any(not(target_os = "macos"), feature = "demo"))]
use bhcapture_core::segment::{IN_H as SEG_H, IN_W as SEG_W};
#[cfg(not(any(not(target_os = "macos"), feature = "demo")))]
const SEG_W: usize = 0;
#[cfg(not(any(not(target_os = "macos"), feature = "demo")))]
const SEG_H: usize = 0;

/// The segmentation model, loaded on first use (a few MB of weights in memory) and kept.
#[cfg(any(not(target_os = "macos"), feature = "demo"))]
fn model_mask(f: &CameraFrame) -> Option<Vec<u8>> {
    static SEG: std::sync::OnceLock<Option<bhcapture_core::segment::Segmenter>> = std::sync::OnceLock::new();
    let s = SEG.get_or_init(|| bhcapture_core::segment::Segmenter::new().map_err(|e| eprintln!("[camera] segmentation unavailable: {e}")).ok());
    s.as_ref()?.mask(f.bgra, f.width, f.height, f.stride)
}

/// Test builds on macOS: run the model too and compare it with Vision (BHC_SEG_COMPARE=1).
fn compare_masks() -> bool {
    cfg!(feature = "demo") && std::env::var_os("BHC_SEG_COMPARE").is_some()
}

/// Agreement between the model's mask and Vision's, both thresholded at 50%, over a few frames.
#[cfg(feature = "demo")]
fn report_agreement(model: &[u8], vision: &[u8], vw: u32, vh: u32, vstride: usize) {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n % 15 != 0 { return; }
    let (mut both, mut either, mut model_on, mut vision_on) = (0u32, 0u32, 0u32, 0u32);
    for y in 0..SEG_H { for x in 0..SEG_W {
        let a = model[y * SEG_W + x] > 127;
        let vx = x * vw as usize / SEG_W; let vy = y * vh as usize / SEG_H;
        let b = vision[vy * vstride + vx] > 127;
        if a && b { both += 1; } if a || b { either += 1; } if a { model_on += 1; } if b { vision_on += 1; }
    } }
    let total = (SEG_W * SEG_H) as f32;
    println!("[segcmp] IoU {:.2}; person share: model {:.0}%, vision {:.0}%", both as f32 / either.max(1) as f32,
        model_on as f32 / total * 100.0, vision_on as f32 / total * 100.0);
}

#[tauri::command]
pub(crate) fn camera_devices() -> Vec<CameraDevice> { platform::camera_devices() }

/// Bar/Settings: turn the camera on or off for the recording being set up.
pub(crate) fn set_enabled(app: &tauri::AppHandle, on: bool, region: Option<LogRect>) -> Result<(), String> {
    if on { if let Some(r) = region { open(app, r)?; } } else { close(app); }
    Ok(())
}
