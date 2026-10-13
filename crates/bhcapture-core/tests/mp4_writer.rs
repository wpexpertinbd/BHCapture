use bhcapture_core::mp4_writer::{annexb_nals, Mp4Writer};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bhc-mp4-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("out.mp4")
}

/// A 320x240 BGRA frame with a white square at x = `pos`.
fn frame(pos: usize) -> Vec<u8> {
    let (w, h) = (320, 240);
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h { for x in 0..w {
        let i = (y * w + x) * 4;
        let on = x >= pos && x < pos + 40 && (100..140).contains(&y);
        let c = if on { 255 } else { 40 };
        v[i..i + 4].copy_from_slice(&[c, c, c, 255]);
    } }
    v
}

fn encode(n: usize) -> Vec<Vec<u8>> {
    let cfg = EncoderConfig::new().bitrate(BitRate::from_bps(500_000)).max_frame_rate(FrameRate::from_hz(30.0))
        .usage_type(UsageType::ScreenContentRealTime);
    let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).unwrap();
    (0..n).map(|i| {
        let px = frame(i * 4 % 280);
        let yuv = YUVBuffer::from_rgb_source(BgraSliceU8::new(&px, (320, 240)));
        enc.encode(&yuv).unwrap().to_vec()
    }).collect()
}

fn boxes(bytes: &[u8]) -> Vec<String> {
    let mut v = Vec::new();
    let mut i = 0usize;
    while i + 8 <= bytes.len() {
        let mut size = u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()) as u64;
        let name = String::from_utf8_lossy(&bytes[i + 4..i + 8]).to_string();
        if size == 1 { size = u64::from_be_bytes(bytes[i + 8..i + 16].try_into().unwrap()); }
        v.push(name);
        if size < 8 { break; }
        i += size as usize;
    }
    v
}

#[test]
fn annexb_split_handles_3_and_4_byte_start_codes() {
    let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
    let n = annexb_nals(&data);
    assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
}

#[test]
fn writes_a_complete_mp4_with_video_and_audio_tracks() {
    let p = tmp("av");
    let frames = encode(60);
    let mut w = Mp4Writer::create(&p, 320, 240, 30, Some((2, 312))).unwrap();
    for (i, f) in frames.iter().enumerate() {
        w.write_video(i as f64 / 30.0, f).unwrap();
        // 20 ms Opus-sized packets alongside (content is opaque to the container).
        if i % 2 == 0 { w.write_audio(&[0xfc, 0xff, 0xfe], 960).unwrap(); w.write_audio(&[0xfc, 0xff, 0xfe], 960).unwrap(); }
    }
    w.finish(Some(2.0)).unwrap();
    let bytes = std::fs::read(&p).unwrap();
    let top = boxes(&bytes);
    assert_eq!(top.first().map(String::as_str), Some("ftyp"), "{top:?}");
    assert!(top.iter().any(|b| b == "mdat") && top.iter().any(|b| b == "moov"), "{top:?}");
    assert!(top.iter().any(|b| b == "moof"), "fragmented: {top:?}");
    assert_eq!(top.last().map(String::as_str), Some("mfra"), "seek index at the end: {top:?}");
    let s = String::from_utf8_lossy(&bytes);
    for tag in ["avc1", "avcC", "Opus", "dOps", "mvex", "trun"] { assert!(s.contains(tag), "missing {tag}"); }
}

#[test]
fn a_crash_leaves_every_finished_fragment_playable() {
    // 4 s of video, then the writer is dropped WITHOUT finish(): what a crash or power cut does.
    let p = tmp("crash");
    let frames = encode(120);
    {
        let mut w = Mp4Writer::create(&p, 320, 240, 30, Some((2, 312))).unwrap();
        for (i, f) in frames.iter().enumerate() {
            w.write_video(i as f64 / 30.0, f).unwrap();
            w.write_audio(&[0xfc, 0xff, 0xfe], 1600).unwrap();
        }
        // dropped here
    }
    let bytes = std::fs::read(&p).unwrap();
    let top = boxes(&bytes);
    assert_eq!(&top[..2], &["ftyp".to_string(), "moov".to_string()], "{top:?}");
    let fragments = top.iter().filter(|b| *b == "moof").count();
    assert!(fragments >= 1, "at least the first 2 s are on disk: {top:?}");
    // Every box is complete: the walk ends exactly at the end of the file.
    let mut i = 0usize;
    while i + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        assert!(size >= 8 && i + size <= bytes.len(), "box at {i} runs past the end");
        i += size;
    }
    assert_eq!(i, bytes.len(), "no partial box at the end");
}

#[test]
fn memory_holds_at_most_one_fragment_the_file_grows_as_frames_arrive() {
    // 5 s of video, no finish yet: the first two 2-second fragments must already be on disk.
    let p = tmp("stream");
    let frames = encode(150);
    let mut w = Mp4Writer::create(&p, 320, 240, 30, None).unwrap();
    for (i, f) in frames.iter().enumerate() { w.write_video(i as f64 / 30.0, f).unwrap(); }
    let bytes = std::fs::read(&p).unwrap();
    let on_disk_frames = boxes(&bytes).iter().filter(|b| *b == "moof").count();
    assert!(on_disk_frames >= 2, "fragments on disk before finish: {:?}", boxes(&bytes));
    let encoded: usize = frames.iter().map(|f| f.len()).sum();
    let last_2s: usize = frames[90..].iter().map(|f| f.len()).sum();
    assert!(bytes.len() + last_2s + 4096 >= encoded * 9 / 10, "only {} of {} bytes written", bytes.len(), encoded);
    w.finish(Some(5.0)).unwrap();
}

#[test]
fn a_file_with_no_keyframe_is_refused_not_written_broken() {
    let p = tmp("nokey");
    let mut w = Mp4Writer::create(&p, 320, 240, 30, None).unwrap();
    // A P-slice only (NAL type 1): nothing can decode it.
    w.write_video(0.0, &[0, 0, 0, 1, 0x41, 1, 2, 3]).unwrap();
    assert!(w.finish(Some(1.0)).is_err());
}
