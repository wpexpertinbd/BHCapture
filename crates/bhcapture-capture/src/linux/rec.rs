//! Linux recorder (X11): GetImage frames -> OpenH264; PulseAudio/PipeWire audio -> Opus; one MP4.
//!
//! - Video: the region is read from the root window at the frame rate, the pointer drawn in from
//!   XFixes, converted to YUV and encoded by OpenH264 (compiled in: Linux has no system encoder that
//!   is always present). Screen-content mode: sharp text at a modest bitrate.
//! - Audio: system sound from the default output's MONITOR source and the microphone from the
//!   default input, both through the PulseAudio API (PipeWire serves it too). The mic gets the same
//!   chain as everywhere (noise filter -> voice EQ -> auto level); the two are mixed live on one
//!   clock into stereo 48 kHz and encoded as Opus in 20 ms packets.
//! - Container: `bhcapture_core::mp4_writer`, streaming to disk (only the index is in memory).
//! - Pause stops the recording clock; nothing is written meanwhile, so the file has no gap.
//!
//! Wayland comes next (portal + PipeWire); there, `start` says so rather than recording black.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bhcapture_core::denoise::Denoiser;
use bhcapture_core::level::AutoLevel;
use bhcapture_core::mp4_writer::Mp4Writer;
use bhcapture_core::voice_eq::VoiceEq;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;

use crate::record::{RecordOptions, RecordState, RecordStatus};
use crate::CaptureError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoExport { Mov, Hevc }

const RATE: u32 = 48_000;
/// Opus packet: 20 ms.
const OPUS_FRAME: usize = 960;

fn be(e: impl std::fmt::Display) -> CaptureError { CaptureError::Backend(e.to_string()) }

pub fn microphone_supported() -> bool { true }

struct Clock { start: Instant, paused_total: Duration, paused_at: Option<Instant> }
impl Clock {
    fn now(&self) -> Duration {
        self.paused_at.unwrap_or_else(Instant::now).saturating_duration_since(self.start).saturating_sub(self.paused_total)
    }
}

struct Shared {
    out: Mutex<Option<Mp4Writer>>,
    clock: Mutex<Clock>,
    stop: AtomicBool,
    failure: Mutex<Option<String>>,
    frames: AtomicU64,
    lvl_sys: AtomicU32,
    lvl_mic: AtomicU32,
    /// Audio is held back until the first video frame is in (MP4 players start at the first frame).
    video_started: AtomicBool,
    w: u32,
    h: u32,
}

impl Shared {
    fn fail(&self, why: String) {
        let mut f = self.failure.lock().unwrap();
        if f.is_none() { eprintln!("[rec] {why}"); *f = Some(why); }
        self.stop.store(true, Ordering::SeqCst);
    }
    fn bump(slot: &AtomicU32, v: f32) {
        if v > f32::from_bits(slot.load(Ordering::Relaxed)) { slot.store(v.to_bits(), Ordering::Relaxed); }
    }
}

pub struct LinuxRecorder { shared: Arc<Shared>, threads: Vec<JoinHandle<()>> }

impl LinuxRecorder {
    pub fn start(opt: &RecordOptions) -> Result<Self, CaptureError> {
        if super::is_wayland() {
            return Err(be("Recording on Wayland is coming in the next update. For now, choose \"Ubuntu on Xorg\" (or your desktop's X11 session) at the login screen."));
        }
        let (c, root) = super::connect()?;
        let mons = super::monitors(&c, root)?;
        let m = mons.iter().find(|m| m.id == opt.display.0).ok_or(CaptureError::DisplayNotFound(opt.display))?;
        let s = super::scale();
        let x = m.x + (opt.region.origin.x * s).round() as i32;
        let y = m.y + (opt.region.origin.y * s).round() as i32;
        let mut w = ((opt.region.size.w * s).round() as u32).min((m.x + m.w as i32 - x).max(0) as u32);
        let mut h = ((opt.region.size.h * s).round() as u32).min((m.y + m.h as i32 - y).max(0) as u32);
        w = w.min(4096) & !1;
        h = h.min(4096) & !1;
        if w < 2 || h < 2 { return Err(CaptureError::RegionEmpty); }
        let fps = opt.fps.clamp(1, 60);
        if opt.path.exists() { return Err(be("the output file already exists")); }
        let audio = opt.system_audio || opt.microphone;
        let pre_skip = if audio { opus_lookahead().unwrap_or(312) } else { 0 };
        let writer = Mp4Writer::create(&opt.path, w, h, fps, audio.then_some((2, pre_skip))).map_err(be)?;

        let shared = Arc::new(Shared {
            out: Mutex::new(Some(writer)),
            clock: Mutex::new(Clock { start: Instant::now(), paused_total: Duration::ZERO, paused_at: None }),
            stop: AtomicBool::new(false), failure: Mutex::new(None), frames: AtomicU64::new(0),
            lvl_sys: AtomicU32::new(0), lvl_mic: AtomicU32::new(0), video_started: AtomicBool::new(false), w, h,
        });
        let mut threads = Vec::new();
        {
            let sh = shared.clone();
            let cursor = opt.show_cursor;
            threads.push(std::thread::spawn(move || video_loop(sh, x, y, w, h, fps, cursor)));
        }
        if audio {
            let sh = shared.clone();
            let o = opt.clone();
            threads.push(std::thread::spawn(move || audio_loop(sh, o)));
        }
        Ok(Self { shared, threads })
    }

    pub fn set_paused(&self, paused: bool) {
        let mut c = self.shared.clock.lock().unwrap();
        match (paused, c.paused_at) {
            (true, None) => c.paused_at = Some(Instant::now()),
            (false, Some(at)) => { c.paused_total += at.elapsed(); c.paused_at = None; }
            _ => {}
        }
    }

    pub fn status(&self) -> RecordStatus {
        let c = self.shared.clock.lock().unwrap();
        let failure = self.shared.failure.lock().unwrap().clone();
        let state = if failure.is_some() { RecordState::Failed } else if c.paused_at.is_some() { RecordState::Paused } else { RecordState::Recording };
        RecordStatus {
            state, seconds: c.now().as_secs_f64(), frames: self.shared.frames.load(Ordering::Relaxed),
            width: self.shared.w, height: self.shared.h,
            level_system: f32::from_bits(self.shared.lvl_sys.swap(0, Ordering::Relaxed)),
            level_mic: f32::from_bits(self.shared.lvl_mic.swap(0, Ordering::Relaxed)),
            error: failure,
        }
    }

    pub fn stop(mut self) -> Result<(), CaptureError> { self.finish() }

    fn finish(&mut self) -> Result<(), CaptureError> {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) { let _ = t.join(); }
        let end = self.shared.clock.lock().unwrap().now().as_secs_f64();
        let Some(w) = self.shared.out.lock().unwrap().take() else { return Ok(()) };
        w.finish(Some(end)).map_err(|e| be(format!("could not finish the file: {e}")))?;
        if let Some(f) = self.shared.failure.lock().unwrap().clone() { return Err(be(f)); }
        Ok(())
    }
}

impl Drop for LinuxRecorder { fn drop(&mut self) { let _ = self.finish(); } }

/// Draw the X11 pointer into a BGRA frame whose top-left is screen point (`ox`, `oy`).
fn draw_cursor(c: &x11rb::rust_connection::RustConnection, px: &mut [u8], w: u32, h: u32, ox: i32, oy: i32) {
    let Ok(Ok(img)) = c.xfixes_get_cursor_image().map(|ck| ck.reply()) else { return };
    let (cw, ch) = (img.width as i32, img.height as i32);
    let (left, top) = (img.x as i32 - img.xhot as i32 - ox, img.y as i32 - img.yhot as i32 - oy);
    for cy in 0..ch {
        let fy = top + cy;
        if fy < 0 || fy >= h as i32 { continue; }
        for cx in 0..cw {
            let fx = left + cx;
            if fx < 0 || fx >= w as i32 { continue; }
            let argb = img.cursor_image[(cy * cw + cx) as usize];
            let a = (argb >> 24) & 0xff;
            if a == 0 { continue; }
            let i = ((fy as u32 * w + fx as u32) * 4) as usize;
            // Cursor pixels are premultiplied ARGB.
            for (k, sh) in [(0usize, 0u32), (1, 8), (2, 16)] {
                let s = (argb >> sh) & 0xff;
                px[i + k] = (s + px[i + k] as u32 * (255 - a) / 255).min(255) as u8;
            }
        }
    }
}

fn video_loop(sh: Arc<Shared>, x: i32, y: i32, w: u32, h: u32, fps: u32, cursor: bool) {
    let (c, root) = match super::connect() { Ok(v) => v, Err(e) => { sh.fail(e.to_string()); return; } };
    let cursor = cursor && c.xfixes_query_version(5, 0).ok().and_then(|ck| ck.reply().ok()).is_some();
    let bps = ((w as f64 * h as f64 * fps as f64 * 0.1).clamp(1.5e6, 30e6)) as u32;
    let cfg = EncoderConfig::new().bitrate(BitRate::from_bps(bps)).max_frame_rate(FrameRate::from_hz(fps as f32))
        .usage_type(UsageType::ScreenContentRealTime).skip_frames(false);
    let mut enc = match Encoder::with_api_config(OpenH264API::from_source(), cfg) {
        Ok(e) => e, Err(e) => { sh.fail(format!("H.264 encoder: {e}")); return; }
    };
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut last_t = -1.0f64;
    while !sh.stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next { std::thread::sleep(next - now); }
        next += period;
        if Instant::now() > next + period * 4 { next = Instant::now() + period; }
        let (paused, t) = { let c = sh.clock.lock().unwrap(); (c.paused_at.is_some(), c.now().as_secs_f64()) };
        if paused || t <= last_t { continue; }
        let mut px = match super::grab(&c, root, x, y, w, h) { Ok(p) => p, Err(e) => { sh.fail(format!("screen capture failed: {e}")); break; } };
        if cursor { draw_cursor(&c, &mut px, w, h, x, y); }
        let yuv = YUVBuffer::from_rgb_source(BgraSliceU8::new(&px, (w as usize, h as usize)));
        let bits = match enc.encode(&yuv) { Ok(b) => b.to_vec(), Err(e) => { sh.fail(format!("H.264 encode: {e}")); break; } };
        if bits.is_empty() { continue; }
        let mut out = sh.out.lock().unwrap();
        let Some(wr) = out.as_mut() else { break };
        if let Err(e) = wr.write_video(t, &bits) { drop(out); sh.fail(format!("writing the recording failed: {e}")); break; }
        sh.video_started.store(true, Ordering::SeqCst);
        sh.frames.fetch_add(1, Ordering::Relaxed);
        last_t = t;
    }
}

fn opus_lookahead() -> Option<u16> {
    let mut e = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio).ok()?;
    e.get_lookahead().ok().map(|v| v.clamp(0, u16::MAX as i32) as u16)
}

/// One PulseAudio capture stream on its own thread (`read` blocks), feeding a shared queue of
/// 48 kHz float frames (`ch` interleaved).
fn pulse_reader(sh: Arc<Shared>, device: Option<&'static str>, ch: u8, q: Arc<Mutex<VecDeque<f32>>>) -> Option<JoinHandle<()>> {
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;
    let spec = Spec { format: Format::F32le, channels: ch, rate: RATE };
    let name = if device.is_some() { "system audio" } else { "microphone" };
    let s = match Simple::new(None, "BHCapture", Direction::Record, device, name, &spec, None, None) {
        Ok(s) => s,
        Err(e) => { eprintln!("[rec] {name} unavailable: {e}"); return None; }
    };
    Some(std::thread::spawn(move || {
        let mut buf = vec![0u8; 480 * ch as usize * 4]; // 10 ms
        while !sh.stop.load(Ordering::SeqCst) {
            if s.read(&mut buf).is_err() { break; }
            let mut g = q.lock().unwrap();
            g.extend(buf.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            let cap = RATE as usize * ch as usize / 2; // never more than 0.5 s queued
            if g.len() > cap { let d = g.len() - cap; g.drain(..d - d % ch as usize); }
        }
    }))
}

fn audio_loop(sh: Arc<Shared>, opt: RecordOptions) {
    let sys_q = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let mic_q = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let mut readers = Vec::new();
    if opt.system_audio { readers.extend(pulse_reader(sh.clone(), Some("@DEFAULT_MONITOR@"), 2, sys_q.clone())); }
    if opt.microphone { readers.extend(pulse_reader(sh.clone(), None, 1, mic_q.clone())); }
    let mut dn = (opt.microphone && opt.noise_filter).then(Denoiser::new);
    let mut eq = (opt.microphone && opt.voice_clarity).then(|| VoiceEq::new(RATE));
    let mut lv = (opt.microphone && opt.auto_level).then(|| AutoLevel::new(RATE));
    let mut enc = match opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio) {
        Ok(e) => e, Err(e) => { eprintln!("[rec] Opus encoder unavailable: {e}"); return; }
    };
    let _ = enc.set_bitrate(opus::Bitrate::Bits(160_000));
    let mut pending: Vec<f32> = Vec::with_capacity(OPUS_FRAME * 4);
    let mut packet = vec![0u8; 4000];
    let mut written: u64 = 0;
    let mut mic_buf: Vec<f32> = Vec::new();
    while !sh.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
        let (paused, t) = { let c = sh.clock.lock().unwrap(); (c.paused_at.is_some(), c.now().as_secs_f64()) };
        if paused || !sh.video_started.load(Ordering::SeqCst) {
            // Nothing said during a pause (or before the first frame) is kept.
            sys_q.lock().unwrap().clear(); mic_q.lock().unwrap().clear();
            if paused { continue; }
            written = (t * RATE as f64) as u64;
            continue;
        }
        let due = (t * RATE as f64) as u64;
        if due <= written { continue; }
        let n = (due - written).min(RATE as u64) as usize;
        // The mic chain runs on exactly the samples about to be mixed (same length out as in).
        mic_buf.clear();
        { let mut q = mic_q.lock().unwrap(); for _ in 0..n { mic_buf.push(q.pop_front().unwrap_or(0.0)); } }
        if let Some(d) = dn.as_mut() { d.process(&mut mic_buf); }
        if let Some(e) = eq.as_mut() { e.process(&mut mic_buf); }
        if let Some(l) = lv.as_mut() { l.process(&mut mic_buf); }
        Shared::bump(&sh.lvl_mic, mic_buf.iter().fold(0.0f32, |m, v| m.max(v.abs())).min(1.0));
        let mut pk = 0.0f32;
        {
            let mut q = sys_q.lock().unwrap();
            for m in mic_buf.iter() {
                let (l, r) = if q.len() >= 2 { (q.pop_front().unwrap(), q.pop_front().unwrap()) } else { (0.0, 0.0) };
                pk = pk.max(l.abs()).max(r.abs());
                pending.push((l + m).clamp(-1.0, 1.0));
                pending.push((r + m).clamp(-1.0, 1.0));
            }
        }
        Shared::bump(&sh.lvl_sys, pk.min(1.0));
        written += n as u64;
        while pending.len() >= OPUS_FRAME * 2 {
            let frame: Vec<f32> = pending.drain(..OPUS_FRAME * 2).collect();
            match enc.encode_float(&frame, &mut packet) {
                Ok(len) => {
                    let mut out = sh.out.lock().unwrap();
                    if let Some(w) = out.as_mut() {
                        if let Err(e) = w.write_audio(&packet[..len], OPUS_FRAME as u32) { drop(out); sh.fail(format!("writing audio failed: {e}")); }
                    }
                }
                Err(e) => { sh.fail(format!("Opus encode: {e}")); break; }
            }
        }
    }
    drop(readers); // they notice `stop` within one 10 ms read
}

// GIF export and HEVC need a decoder; on Linux they come with the Wayland/portal step.
pub fn export_video(_src: &Path, _dst: &Path, _kind: VideoExport, _progress: impl FnMut(f64) -> bool) -> Result<(), CaptureError> {
    Err(be("HEVC and MOV export are not available on Linux; MP4 is the format to share"))
}
pub fn mix_audio_tracks(_src: &Path, _dst: &Path) -> Result<(), CaptureError> { Err(be("nothing to mix")) }
pub fn video_frames(_p: &Path, _fps: f64, _max: u32, _f: impl FnMut(&[u8], u32, u32, f64, f64) -> bool) -> Result<(), CaptureError> {
    Err(be("GIF export on Linux comes in a later update"))
}
pub fn watch_clicks(_f: impl Fn(f64, f64, bool) + Send + Sync + 'static) {}
pub fn unwatch_clicks() {}
