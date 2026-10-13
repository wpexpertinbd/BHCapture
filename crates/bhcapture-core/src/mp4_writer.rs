//! Streaming MP4 writer for H.264 video + Opus audio, for platforms with no system muxer (Linux).
//!
//! **Fragmented MP4, crash-safe.** The file is an init segment (`ftyp` + `moov` without sample
//! tables) followed by self-contained `moof` + `mdat` fragments of about two seconds, each written
//! and flushed to disk as soon as it is complete. A crash, a power cut or a full disk loses at most
//! the last fragment: everything before it plays (the same guarantee macOS gets from AVAssetWriter's
//! `movieFragmentInterval`). A long tutorial must never be lost to one bad moment at the end.
//! Memory holds only the fragment being built.
//!
//! Built on Shiguredo's `shiguredo_mp4` (`Fmp4SegmentMuxer`), which lays out the boxes; this module
//! converts the encoder's output into what MP4 stores:
//!
//! - H.264 arrives as Annex B (start codes); MP4 wants each NAL prefixed by its 4-byte length, and the
//!   SPS/PPS in the `avcC` box, taken from the first keyframe.
//! - A video frame's duration is only known when the NEXT frame arrives (screen capture is variable
//!   frame rate: nothing changes, nothing is sent), so ONE encoded frame is held back.

use std::fs::File;
use std::io::Write;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;

use shiguredo_mp4::boxes::{
    AudioSampleEntryFields, Avc1Box, AvccBox, DopsBox, OpusBox, SampleEntry, VisualSampleEntryFields,
};
use shiguredo_mp4::mux::{Fmp4SegmentMuxer, Sample};
use shiguredo_mp4::{FixedPointNumber, TrackKind, Uint};

/// 90 kHz: the usual video timescale, fine enough for any frame rate.
const VIDEO_TIMESCALE: u32 = 90_000;
/// Opus always runs at 48 kHz.
pub const OPUS_RATE: u32 = 48_000;
/// A fragment is closed once it holds this much video (and starts the next at a video sample).
const FRAGMENT_TICKS: u64 = 2 * VIDEO_TIMESCALE as u64;
/// With audio expected, the first fragment waits (up to this long) for audio to arrive, so the
/// init segment declares both tracks.
const FIRST_AUDIO_WAIT_TICKS: u64 = 5 * VIDEO_TIMESCALE as u64;

#[derive(Debug)]
pub struct Mp4Error(pub String);
impl std::fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
fn err(e: impl std::fmt::Display) -> Mp4Error { Mp4Error(e.to_string()) }

/// The NAL units of an Annex B stream, without their start codes.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        // A 4-byte start code leaves a trailing zero on the previous NAL.
        while e > s && data[e - 1] == 0 && k + 1 < starts.len() { e -= 1; }
        if e > s { out.push(&data[s..e]); }
    }
    out
}

struct Held { pts: u64, body: Vec<u8>, keyframe: bool, entry: Option<SampleEntry> }

/// One sample waiting in the open fragment.
struct Pending { kind: TrackKind, entry: Option<SampleEntry>, keyframe: bool, timescale: u32, duration: u32, body: Vec<u8> }

pub struct Mp4Writer {
    file: File,
    muxer: Fmp4SegmentMuxer,
    width: u16,
    height: u16,
    video_entry_sent: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    held: Option<Held>,
    first_pts: Option<f64>,
    audio_entry: Option<SampleEntry>,
    audio_entry_sent: bool,
    frame_period: u64,
    /// The fragment being built, in arrival order.
    pending: Vec<Pending>,
    pending_video_ticks: u64,
    /// Whether the init segment is on disk yet.
    started: bool,
}

impl Mp4Writer {
    /// `audio`: (channels, Opus pre-skip in 48 kHz samples); None = no audio track.
    pub fn create(path: &Path, width: u32, height: u32, fps: u32, audio: Option<(u16, u16)>) -> Result<Self, Mp4Error> {
        let file = File::create(path).map_err(err)?;
        let muxer = Fmp4SegmentMuxer::new().map_err(|e| err(format!("{e:?}")))?;
        let audio_entry = audio.map(|(channels, pre_skip)| SampleEntry::Opus(OpusBox {
            audio: AudioSampleEntryFields {
                data_reference_index: NonZeroU16::MIN,
                channelcount: channels,
                samplesize: 16,
                samplerate: FixedPointNumber::new(OPUS_RATE as u16, 0),
            },
            dops_box: DopsBox { output_channel_count: channels as u8, pre_skip, input_sample_rate: OPUS_RATE, output_gain: 0 },
            unknown_boxes: Vec::new(),
        }));
        Ok(Self {
            file, muxer, width: width as u16, height: height as u16,
            video_entry_sent: false, sps: None, pps: None, held: None, first_pts: None,
            audio_entry, audio_entry_sent: false,
            frame_period: (VIDEO_TIMESCALE / fps.max(1)) as u64,
            pending: Vec::new(), pending_video_ticks: 0, started: false,
        })
    }

    /// One encoded frame (Annex B), shown from `pts` seconds. Frames before the first keyframe are
    /// dropped: nothing can decode them.
    pub fn write_video(&mut self, pts: f64, annexb: &[u8]) -> Result<(), Mp4Error> {
        let nals = annexb_nals(annexb);
        let mut keyframe = false;
        for n in &nals {
            match n.first().map(|b| b & 0x1f) {
                Some(7) if self.sps.is_none() => self.sps = Some(n.to_vec()),
                Some(8) if self.pps.is_none() => self.pps = Some(n.to_vec()),
                Some(5) => keyframe = true,
                _ => {}
            }
        }
        if !self.video_entry_sent && !keyframe { return Ok(()); }
        // MP4 form: each NAL with a 4-byte big-endian length (AUD and parameter sets dropped from
        // the samples; the parameter sets live in avcC).
        let mut body = Vec::with_capacity(annexb.len() + nals.len() * 4);
        for n in &nals {
            if matches!(n.first().map(|b| b & 0x1f), Some(7) | Some(8) | Some(9)) { continue; }
            body.extend_from_slice(&(n.len() as u32).to_be_bytes());
            body.extend_from_slice(n);
        }
        if body.is_empty() { return Ok(()); }
        let first = *self.first_pts.get_or_insert(pts);
        let ticks = ((pts - first).max(0.0) * VIDEO_TIMESCALE as f64).round() as u64;
        let entry = if !self.video_entry_sent {
            let (Some(sps), Some(pps)) = (self.sps.clone(), self.pps.clone()) else {
                return Err(Mp4Error("the first keyframe carried no SPS/PPS".into()));
            };
            self.video_entry_sent = true;
            Some(SampleEntry::Avc1(Avc1Box {
                visual: VisualSampleEntryFields {
                    data_reference_index: VisualSampleEntryFields::DEFAULT_DATA_REFERENCE_INDEX,
                    width: self.width, height: self.height,
                    horizresolution: VisualSampleEntryFields::DEFAULT_HORIZRESOLUTION,
                    vertresolution: VisualSampleEntryFields::DEFAULT_VERTRESOLUTION,
                    frame_count: VisualSampleEntryFields::DEFAULT_FRAME_COUNT,
                    compressorname: VisualSampleEntryFields::NULL_COMPRESSORNAME,
                    depth: VisualSampleEntryFields::DEFAULT_DEPTH,
                },
                avcc_box: AvccBox {
                    avc_profile_indication: sps.get(1).copied().unwrap_or(66),
                    profile_compatibility: sps.get(2).copied().unwrap_or(0),
                    avc_level_indication: sps.get(3).copied().unwrap_or(31),
                    length_size_minus_one: Uint::new(3),
                    sps_list: vec![sps], pps_list: vec![pps],
                    chroma_format: None, bit_depth_luma_minus8: None, bit_depth_chroma_minus8: None,
                    sps_ext_list: Vec::new(),
                },
                unknown_boxes: Vec::new(),
            }))
        } else { None };
        self.release_held(Some(ticks))?;
        self.held = Some(Held { pts: ticks, body, keyframe, entry });
        Ok(())
    }

    /// The held frame now has a duration: queue it, and close the fragment when it is long enough.
    fn release_held(&mut self, next: Option<u64>) -> Result<(), Mp4Error> {
        let Some(h) = self.held.take() else { return Ok(()) };
        let dur = next.map(|n| n.saturating_sub(h.pts)).unwrap_or(self.frame_period).max(1);
        let dur = dur.min(u32::MAX as u64);
        self.pending.push(Pending { kind: TrackKind::Video, entry: h.entry, keyframe: h.keyframe, timescale: VIDEO_TIMESCALE, duration: dur as u32, body: h.body });
        self.pending_video_ticks += dur;
        if self.pending_video_ticks >= FRAGMENT_TICKS {
            let waiting_for_audio = !self.started && self.audio_entry.is_some() && !self.audio_entry_sent
                && self.pending_video_ticks < FIRST_AUDIO_WAIT_TICKS;
            if !waiting_for_audio { self.flush_fragment()?; }
        }
        Ok(())
    }

    /// One Opus packet covering `samples` 48 kHz samples. Audio before the first video frame is
    /// dropped (players start the movie at the first frame).
    pub fn write_audio(&mut self, packet: &[u8], samples: u32) -> Result<(), Mp4Error> {
        if self.first_pts.is_none() || packet.is_empty() { return Ok(()); }
        if self.audio_entry.is_none() { return Ok(()); }
        let entry = if !self.audio_entry_sent { self.audio_entry_sent = true; self.audio_entry.clone() } else { None };
        self.pending.push(Pending { kind: TrackKind::Audio, entry, keyframe: true, timescale: OPUS_RATE, duration: samples, body: packet.to_vec() });
        Ok(())
    }

    /// Write the open fragment (moof + mdat) and flush it to disk. Payloads are laid out track by
    /// track (all video, then all audio), as the muxer requires.
    fn flush_fragment(&mut self) -> Result<(), Mp4Error> {
        if self.pending.is_empty() { return Ok(()); }
        let mut samples = Vec::with_capacity(self.pending.len());
        let mut offset = 0u64;
        for kind in [TrackKind::Video, TrackKind::Audio] {
            for p in self.pending.iter().filter(|p| p.kind == kind) {
                samples.push(Sample {
                    track_kind: kind,
                    sample_entry: p.entry.clone(),
                    keyframe: p.keyframe,
                    timescale: NonZeroU32::new(p.timescale).unwrap(),
                    duration: p.duration,
                    composition_time_offset: None,
                    data_offset: offset,
                    data_size: p.body.len(),
                });
                offset += p.body.len() as u64;
            }
        }
        let meta = self.muxer.create_media_segment_metadata(&samples).map_err(|e| err(format!("{e:?}")))?;
        if !self.started {
            let init = self.muxer.init_segment_bytes().map_err(|e| err(format!("{e:?}")))?;
            self.file.write_all(&init).map_err(err)?;
            self.started = true;
        }
        let mut out = Vec::with_capacity(meta.len() + offset as usize);
        out.extend_from_slice(&meta);
        for kind in [TrackKind::Video, TrackKind::Audio] {
            for p in self.pending.iter().filter(|p| p.kind == kind) { out.extend_from_slice(&p.body); }
        }
        self.file.write_all(&out).map_err(err)?;
        // On disk now, not just in the page cache: this is the crash-safety guarantee.
        self.file.sync_data().map_err(err)?;
        self.pending.clear();
        self.pending_video_ticks = 0;
        Ok(())
    }

    /// Write the last fragment and the random-access index, and close. `end` (seconds, same clock
    /// as the frames) sets how long the last frame lasts -- a still screen sends nothing after it, so
    /// without this it would be one frame long.
    pub fn finish(mut self, end: Option<f64>) -> Result<(), Mp4Error> {
        let end_ticks = match (end, self.first_pts) {
            (Some(e), Some(f)) => Some(((e - f).max(0.0) * VIDEO_TIMESCALE as f64).round() as u64),
            _ => None,
        };
        let next = end_ticks.filter(|e| self.held.as_ref().map_or(false, |h| *e > h.pts));
        if let Some(h) = self.held.take() {
            let dur = next.map(|n| n.saturating_sub(h.pts)).unwrap_or(self.frame_period).max(1).min(u32::MAX as u64);
            self.pending.push(Pending { kind: TrackKind::Video, entry: h.entry, keyframe: h.keyframe, timescale: VIDEO_TIMESCALE, duration: dur as u32, body: h.body });
        }
        if !self.video_entry_sent { return Err(Mp4Error("no video was recorded".into())); }
        self.flush_fragment()?;
        // mfra: lets players seek quickly in a long file (optional for playback).
        if let Ok(mfra) = self.muxer.mfra_bytes() { self.file.write_all(&mfra).map_err(err)?; }
        self.file.sync_all().map_err(err)
    }
}
