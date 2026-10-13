//! Windows recorder: GDI frames + WASAPI audio -> Media Foundation sink writer (H.264 + AAC, MP4).
//!
//! - Video: the region is grabbed with GDI at the frame rate (pointer drawn in) and handed to the
//!   sink writer as RGB32; Windows' own H.264 encoder (hardware where present) does the rest.
//! - Audio: system sound (WASAPI loopback) and the microphone are read on ONE thread, each resampled
//!   to 48 kHz; the mic goes through the same chain as on macOS (noise filter -> voice EQ -> auto
//!   level, all from portable core); the two are MIXED LIVE into one stereo track. A clock drives the
//!   output: loopback delivers NO data while nothing plays, so silence is filled rather than waited for.
//! - Pause: the recording clock stops; nothing is written; so the file has no gap.
//! - Timestamps are ours (recording clock, 100 ns units), not the devices', so video and audio share
//!   one timeline by construction.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bhcapture_core::denoise::Denoiser;
use bhcapture_core::level::AutoLevel;
use bhcapture_core::resample::Resampler;
use bhcapture_core::voice_eq::VoiceEq;
use windows::core::{HSTRING, GUID};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_QUIT, WM_RBUTTONDOWN,
};

use crate::record::{RecordOptions, RecordState, RecordStatus};
use crate::CaptureError;

/// Windows records the microphone on every supported version.
pub fn microphone_supported() -> bool { true }

const RATE: u32 = 48_000;
const HNS: f64 = 10_000_000.0; // 100 ns units per second

fn be(e: impl std::fmt::Display) -> CaptureError { CaptureError::Backend(e.to_string()) }

/// The recording clock: wall time minus pauses.
struct Clock { start: Instant, paused_total: Duration, paused_at: Option<Instant> }

impl Clock {
    fn now(&self) -> Duration {
        let end = self.paused_at.unwrap_or_else(Instant::now);
        end.saturating_duration_since(self.start).saturating_sub(self.paused_total)
    }
}

struct Shared {
    writer: Mutex<Option<IMFSinkWriter>>,
    clock: Mutex<Clock>,
    stop: AtomicBool,
    failure: Mutex<Option<String>>,
    frames: AtomicU64,
    lvl_sys: AtomicU32,
    lvl_mic: AtomicU32,
    w: u32,
    h: u32,
}

// The sink writer is a free-threaded COM object; every use goes through the mutex.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    fn fail(&self, why: String) {
        let mut f = self.failure.lock().unwrap();
        if f.is_none() { eprintln!("[rec] {why}"); *f = Some(why); }
    }
    fn bump_level(slot: &AtomicU32, v: f32) {
        let cur = f32::from_bits(slot.load(Ordering::Relaxed));
        if v > cur { slot.store(v.to_bits(), Ordering::Relaxed); }
    }
    fn write(&self, stream: u32, sample: &IMFSample) {
        if let Some(w) = self.writer.lock().unwrap().as_ref() {
            if let Err(e) = unsafe { w.WriteSample(stream, sample) } {
                // Typically the disk filled up. Stop taking samples; stop() finishes what we have.
                self.fail(format!("writing the recording failed: {e}"));
                self.stop.store(true, Ordering::SeqCst);
            }
        }
    }
}

pub struct WinRecorder {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

unsafe impl Send for WinRecorder {}

fn media_sample(bytes: &[u8], time: i64, dur: i64) -> windows::core::Result<IMFSample> {
    unsafe {
        let buf = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut p: *mut u8 = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        buf.Unlock()?;
        buf.SetCurrentLength(bytes.len() as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        s.SetSampleTime(time)?;
        s.SetSampleDuration(dur)?;
        Ok(s)
    }
}

fn pack(a: u32, b: u32) -> u64 { ((a as u64) << 32) | b as u64 }

impl WinRecorder {
    pub fn start(opt: &RecordOptions) -> Result<Self, CaptureError> {
        // Where on the desktop, in physical pixels.
        let mons = super::monitors();
        let m = mons.iter().find(|m| m.id == opt.display.0).ok_or(CaptureError::DisplayNotFound(opt.display))?;
        let s = m.scale;
        let x = m.phys.left + (opt.region.origin.x * s).round() as i32;
        let y = m.phys.top + (opt.region.origin.y * s).round() as i32;
        let mut w = ((opt.region.size.w * s).round() as u32).min((m.phys.right - x).max(0) as u32);
        let mut h = ((opt.region.size.h * s).round() as u32).min((m.phys.bottom - y).max(0) as u32);
        // H.264 4:2:0 needs even sizes; the encoder's dependable ceiling is 4096 on the long side.
        // (A larger region is recorded at its top-left 4096 rather than scaled -- GDI does not scale.)
        w = w.min(4096) & !1;
        h = h.min(4096) & !1;
        if w < 2 || h < 2 { return Err(CaptureError::RegionEmpty); }
        let fps = opt.fps.clamp(1, 60);

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(be)?;
        }
        if opt.path.exists() { return Err(be("the output file already exists")); }
        let writer = unsafe { create_writer(&opt.path, w, h, fps, opt.system_audio || opt.microphone) }.map_err(be)?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(writer)),
            clock: Mutex::new(Clock { start: Instant::now(), paused_total: Duration::ZERO, paused_at: None }),
            stop: AtomicBool::new(false),
            failure: Mutex::new(None),
            frames: AtomicU64::new(0),
            lvl_sys: AtomicU32::new(0),
            lvl_mic: AtomicU32::new(0),
            w, h,
        });

        let mut threads = Vec::new();
        {
            let sh = shared.clone();
            let cursor = opt.show_cursor;
            threads.push(std::thread::spawn(move || video_loop(sh, x, y, w, h, fps, cursor)));
        }
        if opt.system_audio || opt.microphone {
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
            state,
            seconds: c.now().as_secs_f64(),
            frames: self.shared.frames.load(Ordering::Relaxed),
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
        let writer = self.shared.writer.lock().unwrap().take();
        let Some(writer) = writer else { return Ok(()) };
        if self.shared.frames.load(Ordering::Relaxed) == 0 {
            return Err(be("No video was captured"));
        }
        let r = unsafe { writer.Finalize() }.map_err(|e| be(format!("could not finish the file: {e}")));
        drop(writer);
        unsafe { let _ = MFShutdown(); }
        r?;
        if let Some(f) = self.shared.failure.lock().unwrap().clone() { return Err(be(f)); }
        Ok(())
    }
}

impl Drop for WinRecorder {
    fn drop(&mut self) { let _ = self.finish(); }
}

/// The sink writer: stream 0 = H.264 video (input RGB32, top-down), stream 1 = AAC (input PCM s16 stereo 48k).
unsafe fn create_writer(path: &std::path::Path, w: u32, h: u32, fps: u32, audio: bool) -> windows::core::Result<IMFSinkWriter> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 4)?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        // Fragmented MP4: written to disk piece by piece, so a crash, power cut or full disk keeps
        // everything recorded before it -- a plain MP4 writes its index only at the end and loses
        // the whole recording (decision 37: long recordings must never be lost). Windows 10+; if the
        // system refuses it, record a plain MP4 rather than not at all.
        let frag = attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_FMPEG4)
            .and_then(|_| MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs));
        let writer = match frag {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[rec] fragmented MP4 unavailable ({e}); writing a plain MP4");
                let _ = std::fs::remove_file(path);
                let _ = attrs.DeleteItem(&MF_TRANSCODE_CONTAINERTYPE);
                MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)?
            }
        };

        let bps = ((w as f64 * h as f64 * fps as f64 * 0.1).clamp(2e6, 50e6)) as u32;
        let vout = MFCreateMediaType()?;
        vout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        vout.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        vout.SetUINT32(&MF_MT_AVG_BITRATE, bps)?;
        vout.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        vout.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        vout.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        vout.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        let vs = writer.AddStream(&vout)?;
        let vin = MFCreateMediaType()?;
        vin.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        vin.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        vin.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        vin.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        vin.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        vin.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        // Positive stride = top-down rows, which is what GDI gives us (RGB32 defaults to bottom-up).
        vin.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
        writer.SetInputMediaType(vs, &vin, None)?;
        debug_assert_eq!(vs, 0);

        if audio {
            let aout = MFCreateMediaType()?;
            aout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            aout.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            aout.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
            aout.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            aout.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            aout.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 20_000)?; // 160 kbit/s
            let as_ = writer.AddStream(&aout)?;
            let ain = MFCreateMediaType()?;
            ain.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            ain.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            ain.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
            ain.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            ain.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            ain.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            ain.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * 4)?;
            writer.SetInputMediaType(as_, &ain, None)?;
            debug_assert_eq!(as_, 1);
        }
        writer.BeginWriting()?;
        Ok(writer)
    }
}

fn video_loop(sh: Arc<Shared>, x: i32, y: i32, w: u32, h: u32, fps: u32, cursor: bool) {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let dur = (HNS / fps as f64) as i64;
    let mut next = Instant::now();
    let mut last_ts: i64 = -1;
    while !sh.stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next { std::thread::sleep(next - now); }
        next += period;
        if Instant::now() > next + period * 4 { next = Instant::now() + period; } // fell behind: do not burst
        let (paused, t) = { let c = sh.clock.lock().unwrap(); (c.paused_at.is_some(), c.now()) };
        if paused { continue; }
        let ts = (t.as_secs_f64() * HNS) as i64;
        if ts <= last_ts { continue; }
        match super::grab(x, y, w as i32, h as i32, cursor) {
            Ok(px) => match media_sample(&px, ts, dur) {
                Ok(s) => { sh.write(0, &s); sh.frames.fetch_add(1, Ordering::Relaxed); last_ts = ts; }
                Err(e) => sh.fail(format!("video sample: {e}")),
            },
            Err(e) => { sh.fail(format!("screen capture failed: {e}")); break; }
        }
    }
}

/// One WASAPI input: system sound (loopback) or the microphone, resampled to 48 kHz.
struct Input {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: usize,
    float: bool,
    bits: u16,
    resampler: Resampler,
    scratch: Vec<f32>,
}

impl Input {
    unsafe fn open(loopback: bool) -> windows::core::Result<Self> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(if loopback { eRender } else { eCapture }, eConsole)?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let fmt = client.GetMixFormat()?;
            let f: WAVEFORMATEX = *fmt;
            let tag = f.wFormatTag as u32;
            let float = tag == 3 || (tag == 0xFFFE && {
                let ext: WAVEFORMATEXTENSIBLE = *(fmt as *const WAVEFORMATEXTENSIBLE);
                let sub: GUID = ext.SubFormat;
                sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            });
            let flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 10_000_000, 0, fmt, None);
            let (channels, rate, bits) = (f.nChannels as usize, f.nSamplesPerSec as f64, f.wBitsPerSample);
            CoTaskMemFree(Some(fmt as *const _));
            r?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self { client, capture, channels: channels.max(1), float, bits,
                      resampler: Resampler::new(rate as u32, RATE, channels.max(1)), scratch: Vec::new() })
        }
    }

    /// Everything waiting, as 48 kHz frames of `out_ch` channels (1 = mono mix, 2 = stereo).
    unsafe fn read(&mut self, out_ch: usize, out: &mut VecDeque<f32>) {
        unsafe {
            loop {
                let n = match self.capture.GetNextPacketSize() { Ok(n) if n > 0 => n, _ => break };
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                if self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() { break; }
                let _ = n;
                let ch = self.channels;
                let mut input: Vec<f32> = Vec::with_capacity(frames as usize * ch);
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    input.resize(frames as usize * ch, 0.0);
                } else if self.float && self.bits == 32 {
                    input.extend_from_slice(std::slice::from_raw_parts(data as *const f32, frames as usize * ch));
                } else if self.bits == 16 {
                    input.extend(std::slice::from_raw_parts(data as *const i16, frames as usize * ch).iter().map(|v| *v as f32 / 32768.0));
                } else if self.bits == 32 {
                    input.extend(std::slice::from_raw_parts(data as *const i32, frames as usize * ch).iter().map(|v| *v as f32 / 2147483648.0));
                } else {
                    input.resize(frames as usize * ch, 0.0);
                }
                let _ = self.capture.ReleaseBuffer(frames);
                self.scratch.clear();
                self.resampler.process(&input, out_ch, &mut self.scratch);
                out.extend(self.scratch.iter().copied());
            }
        }
    }

}

impl Drop for Input {
    fn drop(&mut self) { unsafe { let _ = self.client.Stop(); } }
}

fn peak(v: impl Iterator<Item = f32>) -> f32 { v.fold(0.0f32, |m, s| m.max(s.abs())).min(1.0) }

fn audio_loop(sh: Arc<Shared>, opt: RecordOptions) {
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let mut sys = if opt.system_audio {
        match unsafe { Input::open(true) } { Ok(i) => Some(i), Err(e) => { eprintln!("[rec] system audio unavailable: {e}"); None } }
    } else { None };
    let mut mic = if opt.microphone {
        match unsafe { Input::open(false) } { Ok(i) => Some(i), Err(e) => { eprintln!("[rec] microphone unavailable: {e}"); None } }
    } else { None };
    let mut dn = (opt.microphone && opt.noise_filter).then(Denoiser::new);
    let mut eq = (opt.microphone && opt.voice_clarity).then(|| VoiceEq::new(RATE));
    let mut lv = (opt.microphone && opt.auto_level).then(|| AutoLevel::new(RATE));

    let (mut sys_q, mut mic_q) = (VecDeque::<f32>::new(), VecDeque::<f32>::new());
    let mut written: u64 = 0; // 48 kHz frames already written
    let cap = RATE as usize / 2; // keep at most 0.5 s queued per source
    let mut block: Vec<u8> = Vec::new();
    while !sh.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
        if let Some(s) = sys.as_mut() { unsafe { s.read(2, &mut sys_q) }; }
        if let Some(m) = mic.as_mut() {
            let before = mic_q.len();
            unsafe { m.read(1, &mut mic_q) };
            // The mic chain runs on the NEW samples, in place (same length out as in).
            let new: &mut [f32] = mic_q.make_contiguous();
            let fresh = &mut new[before..];
            if let Some(d) = dn.as_mut() { d.process(fresh); }
            if let Some(e) = eq.as_mut() { e.process(fresh); }
            if let Some(l) = lv.as_mut() { l.process(fresh); }
            Shared::bump_level(&sh.lvl_mic, peak(fresh.iter().copied()));
        }
        if sys_q.len() > cap * 2 { let d = sys_q.len() - cap * 2; sys_q.drain(..d - d % 2); }
        if mic_q.len() > cap { let d = mic_q.len() - cap; mic_q.drain(..d); }

        let (paused, t) = { let c = sh.clock.lock().unwrap(); (c.paused_at.is_some(), c.now()) };
        if paused { sys_q.clear(); mic_q.clear(); continue; } // nothing said during a pause is kept
        let due = (t.as_secs_f64() * RATE as f64) as u64;
        if due <= written { continue; }
        let n = (due - written).min(RATE as u64) as usize;
        block.clear();
        let mut pk = 0.0f32;
        for _ in 0..n {
            let (sl, sr) = if sys_q.len() >= 2 { (sys_q.pop_front().unwrap(), sys_q.pop_front().unwrap()) } else { (0.0, 0.0) };
            pk = pk.max(sl.abs()).max(sr.abs());
            let m = mic_q.pop_front().unwrap_or(0.0);
            for v in [sl + m, sr + m] {
                block.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
            }
        }
        Shared::bump_level(&sh.lvl_sys, pk.min(1.0));
        let time = (written as f64 / RATE as f64 * HNS) as i64;
        let dur = (n as f64 / RATE as f64 * HNS) as i64;
        match media_sample(&block, time, dur) {
            Ok(s) => sh.write(1, &s),
            Err(e) => sh.fail(format!("audio sample: {e}")),
        }
        written += n as u64;
    }
}

// --- Click highlight: a low-level mouse hook on its own thread with a message loop. ---------------

type ClickFn = Box<dyn Fn(f64, f64, bool) + Send + Sync>;
static CLICK_WATCHER: Mutex<Option<ClickFn>> = Mutex::new(None);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && (wparam.0 as u32 == WM_LBUTTONDOWN || wparam.0 as u32 == WM_RBUTTONDOWN) {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let (px, py) = (info.pt.x, info.pt.y);
        let s = super::scale_at(&super::monitors(), px, py);
        if let Some(f) = CLICK_WATCHER.lock().unwrap().as_ref() {
            f(px as f64 / s, py as f64 / s, wparam.0 as u32 == WM_RBUTTONDOWN);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Call `f(x, y, right_button)` for every mouse-down anywhere (global logical points).
///
/// Returns only once the hook thread has its message queue and has published its id (or failed), so
/// an `unwatch_clicks` straight after can always reach it -- otherwise the thread could store its id
/// after the stop, keep the system-wide hook installed, and be orphaned (audit 2026-10-13).
pub fn watch_clicks(f: impl Fn(f64, f64, bool) + Send + Sync + 'static) {
    unwatch_clicks();
    *CLICK_WATCHER.lock().unwrap() = Some(Box::new(f));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_NOREMOVE};
        // Create this thread's message queue first, so a WM_QUIT posted the moment the id is
        // published cannot be lost.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let module = GetModuleHandleW(None).ok();
        let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module.map(|m| m.into()), 0) {
            Ok(h) => h,
            Err(e) => { eprintln!("[rec] click highlight unavailable: {e}"); let _ = ready_tx.send(()); return; }
        };
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
        let _ = ready_tx.send(());
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        let _ = UnhookWindowsHookEx(hook);
    });
    let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(2));
}

/// Stop reporting clicks.
pub fn unwatch_clicks() {
    let t = HOOK_THREAD.swap(0, Ordering::SeqCst);
    if t != 0 { unsafe { let _ = PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0)); } }
    *CLICK_WATCHER.lock().unwrap() = None;
}
