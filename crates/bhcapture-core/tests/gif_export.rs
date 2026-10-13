use bhcapture_core::gif_export::*;

fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    (0..w * h).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect()
}

/// Frame rectangles (left, top, width, height) of a GIF.
fn rects(bytes: &[u8]) -> Vec<(u16, u16, u16, u16)> {
    let mut d = gif::DecodeOptions::new().read_info(bytes).unwrap();
    let mut v = vec![];
    while let Some(f) = d.read_next_frame().unwrap() { v.push((f.left, f.top, f.width, f.height)); }
    v
}

/// Decode a GIF: (frame count, delays in 1/100 s, first pixel of each frame).
fn read(bytes: &[u8]) -> (usize, Vec<u16>, Vec<[u8; 3]>) {
    let mut o = gif::DecodeOptions::new();
    o.set_color_output(gif::ColorOutput::RGBA);
    let mut d = o.read_info(bytes).unwrap();
    let (mut n, mut delays, mut px) = (0, vec![], vec![]);
    while let Some(f) = d.read_next_frame().unwrap() {
        n += 1;
        delays.push(f.delay);
        px.push([f.buffer[0], f.buffer[1], f.buffer[2]]);
    }
    (n, delays, px)
}

#[test]
fn still_frames_are_merged_and_keep_their_real_duration() {
    let mut out = Vec::new();
    let mut g = GifWriter::new(&mut out, 20, 10).unwrap();
    // 1 s of red at 12 fps, then 0.5 s of blue.
    for i in 0..12 { g.push(&solid(20, 10, [255, 0, 0]), i as f64 / 12.0).unwrap(); }
    for i in 12..18 { g.push(&solid(20, 10, [0, 0, 255]), i as f64 / 12.0).unwrap(); }
    let (inn, outn) = g.finish(1.5).unwrap();
    assert_eq!((inn, outn), (18, 2), "18 frames in, 2 distinct out");
    let (n, delays, px) = read(&out);
    assert_eq!(n, 2);
    assert_eq!(delays, vec![100, 50], "red lasts 1.00 s, blue 0.50 s");
    assert!(px[0][0] > 200 && px[0][2] < 50, "first frame is red: {:?}", px[0]);
    assert!(px[1][2] > 200 && px[1][0] < 50, "second frame is blue: {:?}", px[1]);
}

#[test]
fn a_wrong_sized_frame_is_refused_not_written() {
    let mut out = Vec::new();
    let mut g = GifWriter::new(&mut out, 20, 10).unwrap();
    assert!(g.push(&solid(10, 10, [0, 0, 0]), 0.0).is_err());
}

#[test]
fn output_size_keeps_aspect_and_never_upscales() {
    assert_eq!(gif_size(2704, 1756, 960), (960, 623));
    assert_eq!(gif_size(1756, 2704, 960), (623, 960));
    assert_eq!(gif_size(640, 400, 960), (640, 400));
}

#[test]
fn it_loops_forever() {
    let mut out = Vec::new();
    let g = GifWriter::new(&mut out, 4, 4).unwrap();
    g.finish(0.0).unwrap();
    // NETSCAPE2.0 application extension with loop count 0 = infinite.
    let s = out.windows(11).any(|w| w == b"NETSCAPE2.0");
    assert!(s, "repeat extension present");
}

#[test]
fn invisible_decoding_noise_does_not_make_a_new_frame() {
    // What decoded video really looks like on a still screen: the same picture, off by a few levels.
    let mut out = Vec::new();
    let mut g = GifWriter::new(&mut out, 16, 16).unwrap();
    for i in 0..24 {
        let n = (i % 5) as u8; // 0..4 levels of wobble
        g.push(&solid(16, 16, [100 + n, 120 - n, 140 + n]), i as f64 / 12.0).unwrap();
    }
    let (_, outn) = g.finish(2.0).unwrap();
    assert_eq!(outn, 1, "a still screen with codec noise is ONE frame");
}

#[test]
fn only_the_changed_rectangle_is_stored() {
    let mut out = Vec::new();
    let mut g = GifWriter::new(&mut out, 100, 80).unwrap();
    let base = solid(100, 80, [240, 240, 240]);
    g.push(&base, 0.0).unwrap();
    // A "cursor" appears: a 6x6 block at (40, 30).
    let mut moved = base.clone();
    for y in 30..36 { for x in 40..46 { let i = (y * 100 + x) * 4; moved[i..i + 3].copy_from_slice(&[0, 0, 0]); } }
    g.push(&moved, 0.5).unwrap();
    g.finish(1.0).unwrap();
    let r = rects(&out);
    assert_eq!(r[0], (0, 0, 100, 80), "first frame is the whole picture");
    assert_eq!(r[1], (40, 30, 6, 6), "second frame is just the change");
}
