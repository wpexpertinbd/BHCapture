//! Windows media helpers on Media Foundation: decode a recording into frames (GIF export) and
//! re-encode it as HEVC. MOV is Apple's container and is not offered on Windows.

use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::CaptureError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoExport { Mov, Hevc }

fn be(e: impl std::fmt::Display) -> CaptureError { CaptureError::Backend(e.to_string()) }

/// Recordings on Windows already hold ONE mixed audio track (mixed live), so there is never
/// anything to mix afterwards.
pub fn mix_audio_tracks(_src: &Path, _dst: &Path) -> Result<(), CaptureError> {
    Err(be("nothing to mix"))
}

/// Media Foundation for this thread, released when dropped.
struct Mf;
impl Mf {
    fn start() -> Result<Self, CaptureError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(be)?;
        }
        Ok(Mf)
    }
}
impl Drop for Mf { fn drop(&mut self) { unsafe { let _ = MFShutdown(); } } }

fn reader(path: &Path, video_processing: bool) -> windows::core::Result<IMFSourceReader> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.unwrap();
        if video_processing { attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?; }
        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)
    }
}

/// Duration in seconds, from the presentation descriptor.
fn duration(r: &IMFSourceReader) -> f64 {
    unsafe {
        r.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .map(|pv| pv.Anonymous.Anonymous.Anonymous.uhVal as f64 / 10_000_000.0).unwrap_or(0.0)
    }
}

fn unpack(v: u64) -> (u32, u32) { ((v >> 32) as u32, v as u32) }

/// Box-filter downscale of top-down BGRA to RGBA. Each output pixel averages the source pixels it
/// covers, which keeps text legible when a 4K recording becomes a 960 px GIF.
fn scale_to_rgba(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let y0 = y * sh / dh; let y1 = ((y + 1) * sh / dh).max(y0 + 1).min(sh);
        for x in 0..dw {
            let x0 = x * sw / dw; let x1 = ((x + 1) * sw / dw).max(x0 + 1).min(sw);
            let (mut b, mut g, mut r, mut n) = (0u32, 0u32, 0u32, 0u32);
            for yy in y0..y1 {
                let row = yy * sw * 4;
                for xx in x0..x1 {
                    let i = row + xx * 4;
                    b += src[i] as u32; g += src[i + 1] as u32; r += src[i + 2] as u32; n += 1;
                }
            }
            let o = (y * dw + x) * 4;
            out[o] = (r / n) as u8; out[o + 1] = (g / n) as u8; out[o + 2] = (b / n) as u8; out[o + 3] = 255;
        }
    }
    out
}

/// Decode a recording into RGBA frames at `fps`, longest side at most `max_side`. `f(rgba, w, h, t,
/// duration)` returns false to stop. Same contract as the macOS version.
pub fn video_frames(path: &Path, fps: f64, max_side: u32, mut f: impl FnMut(&[u8], u32, u32, f64, f64) -> bool) -> Result<(), CaptureError> {
    let _mf = Mf::start()?;
    let fps = if fps > 0.0 { fps } else { 12.0 };
    unsafe {
        let r = reader(path, true).map_err(be)?;
        let dur = duration(&r);
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(be)?;
        r.SetStreamSelection(video, true).map_err(be)?;
        let want = MFCreateMediaType().map_err(be)?;
        want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(be)?;
        want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(be)?;
        r.SetCurrentMediaType(video, None, &want).map_err(be)?;
        let cur = r.GetCurrentMediaType(video).map_err(be)?;
        let (sw, sh) = unpack(cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(be)?);
        let (sw, sh) = (sw as usize, sh as usize);
        // RGB32 is bottom-up unless the type says otherwise (a positive stride).
        let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(-(sw as i32 * 4));
        let longest = sw.max(sh).max(1);
        let k = if max_side > 0 && longest > max_side as usize { max_side as f64 / longest as f64 } else { 1.0 };
        let (dw, dh) = (((sw as f64 * k).round() as usize).max(1), ((sh as f64 * k).round() as usize).max(1));

        let (mut next, step) = (0.0f64, 1.0 / fps);
        let mut last: Option<Vec<u8>> = None;
        let mut top_down = vec![0u8; sw * sh * 4];
        loop {
            let (mut flags, mut ts) = (0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            r.ReadSample(video, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).map_err(be)?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
            let Some(sample) = sample else { continue };
            let t = ts as f64 / 10_000_000.0;
            if t + 1e-6 < next { continue; } // between output ticks
            let buf = sample.ConvertToContiguousBuffer().map_err(be)?;
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len)).map_err(be)?;
            let row = sw * 4;
            let abs = stride.unsigned_abs() as usize;
            if (len as usize) >= abs * (sh - 1) + row {
                for y in 0..sh {
                    let src_y = if stride < 0 { sh - 1 - y } else { y };
                    std::ptr::copy_nonoverlapping(p.add(src_y * abs), top_down.as_mut_ptr().add(y * row), row);
                }
            }
            let _ = buf.Unlock();
            let rgba = scale_to_rgba(&top_down, sw, sh, dw, dh);
            while t + 1e-6 >= next {
                if !f(&rgba, dw as u32, dh as u32, next, dur) { return Err(be("cancelled")); }
                next += step;
            }
            last = Some(rgba);
        }
        let Some(rgba) = last else { return Err(be("no frames in this file")) };
        while next < dur {
            if !f(&rgba, dw as u32, dh as u32, next, dur) { return Err(be("cancelled")); }
            next += step;
        }
        Ok(())
    }
}

/// Re-encode as HEVC MP4 (audio copied). Needs Microsoft's "HEVC Video Extensions" on the PC;
/// without it Windows has no HEVC encoder, and the user is told so rather than given a broken file.
pub fn export_video(src: &Path, dst: &Path, kind: VideoExport, mut progress: impl FnMut(f64) -> bool) -> Result<(), CaptureError> {
    if kind == VideoExport::Mov { return Err(be("MOV is not available on Windows")); }
    let _mf = Mf::start()?;
    let _ = std::fs::remove_file(dst);
    unsafe {
        let r = reader(src, true).map_err(be)?;
        let dur = duration(&r).max(0.001);
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let nv12 = MFCreateMediaType().map_err(be)?;
        nv12.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(be)?;
        nv12.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(be)?;
        r.SetCurrentMediaType(video, None, &nv12).map_err(be)?;
        let vin = r.GetCurrentMediaType(video).map_err(be)?;
        let size = vin.GetUINT64(&MF_MT_FRAME_SIZE).map_err(be)?;
        let rate = vin.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(((30u64) << 32) | 1);
        let has_audio = r.GetNativeMediaType(audio, 0).is_ok();

        // ~55% of the original's bitrate, as on macOS (where Apple's presets came out BIGGER).
        let file_bits = std::fs::metadata(src).map(|m| m.len() as f64 * 8.0).unwrap_or(0.0);
        let bps = ((file_bits / dur) * 0.55).max(250e3) as u32;

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(be)?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).map_err(be)?;
        let w = MFCreateSinkWriterFromURL(&HSTRING::from(dst.as_os_str()), None, &attrs).map_err(be)?;
        let vout = MFCreateMediaType().map_err(be)?;
        vout.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(be)?;
        vout.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC).map_err(be)?;
        vout.SetUINT32(&MF_MT_AVG_BITRATE, bps).map_err(be)?;
        vout.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(be)?;
        vout.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(be)?;
        vout.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(be)?;
        vout.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).map_err(be)?;
        let vs = w.AddStream(&vout).map_err(be)?;
        if w.SetInputMediaType(vs, &vin, None).is_err() {
            let _ = std::fs::remove_file(dst);
            return Err(CaptureError::Backend("This PC has no HEVC encoder. Install Microsoft's \"HEVC Video Extensions\" from the Microsoft Store, or export as MP4 (H.264).".into()));
        }
        let mut as_ = None;
        if has_audio {
            // Copy the AAC as is: the writer's output type IS the reader's native type, no input type set.
            let native = r.GetNativeMediaType(audio, 0).map_err(be)?;
            r.SetCurrentMediaType(audio, None, &native).map_err(be)?;
            as_ = Some(w.AddStream(&native).map_err(be)?);
        }
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(be)?;
        r.SetStreamSelection(video, true).map_err(be)?;
        if has_audio { r.SetStreamSelection(audio, true).map_err(be)?; }
        w.BeginWriting().map_err(be)?;

        let mut done_video = false;
        let mut done_audio = !has_audio;
        let mut last_report = -1.0;
        while !(done_video && done_audio) {
            let (mut idx, mut flags, mut ts) = (0u32, 0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            r.ReadSample(MF_SOURCE_READER_ALL_STREAMS.0 as u32, 0, Some(&mut idx), Some(&mut flags), Some(&mut ts), Some(&mut sample)).map_err(be)?;
            let is_video = r.GetCurrentMediaType(idx).ok().and_then(|t| t.GetGUID(&MF_MT_MAJOR_TYPE).ok()) == Some(MFMediaType_Video);
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                if is_video { done_video = true } else { done_audio = true }
                continue;
            }
            let Some(s) = sample else { continue };
            let stream = if is_video { vs } else { as_.unwrap_or(vs) };
            if let Err(e) = w.WriteSample(stream, &s) {
                let _ = std::fs::remove_file(dst);
                return Err(be(format!("HEVC export failed: {e}")));
            }
            if is_video {
                let p = (ts as f64 / 10_000_000.0 / dur).clamp(0.0, 0.99);
                if p - last_report >= 0.01 {
                    last_report = p;
                    if !progress(p) { drop(w); let _ = std::fs::remove_file(dst); return Err(be("cancelled")); }
                }
            }
        }
        w.Finalize().map_err(be)?;
        progress(1.0);
        Ok(())
    }
}
