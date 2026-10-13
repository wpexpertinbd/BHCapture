use bhcapture_core::denoise::*;

/// Deterministic white noise, so the test never flakes.
fn noise(n: usize, amp: f32) -> Vec<f32> {
    let mut x: u32 = 12345;
    (0..n).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp }).collect()
}
fn rms(v: &[f32]) -> f32 { (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt() }
fn db(r: f32) -> f32 { 20.0 * r.max(1e-9).log10() }

#[test]
fn output_length_always_equals_input_length() {
    let mut d = Denoiser::new();
    for n in [1usize, 7, 479, 480, 481, 1024, 3000] {
        let mut b = noise(n, 0.1);
        d.process(&mut b);
        assert_eq!(b.len(), n);
    }
}

#[test]
fn chunk_size_does_not_change_the_result() {
    // Capture buffers arrive in arbitrary sizes; the FIFO must make that invisible.
    let src = noise(48_000, 0.05);
    let mut whole = src.clone();
    Denoiser::new().process(&mut whole);
    let mut pieces = src.clone();
    let mut d = Denoiser::new();
    let mut i = 0;
    for step in [100usize, 333, 480, 7, 1500].iter().cycle() {
        if i >= pieces.len() { break; }
        let j = (i + step).min(pieces.len());
        d.process(&mut pieces[i..j]);
        i = j;
    }
    assert_eq!(whole, pieces);
}

#[test]
fn the_first_frame_is_the_fixed_delay() {
    let mut b = noise(FRAME * 3, 0.2);
    Denoiser::new().process(&mut b);
    assert!(b[..FRAME].iter().all(|&s| s == 0.0), "the primed 10 ms of silence comes out first");
}

#[test]
fn steady_noise_is_strongly_reduced() {
    // A fan/hiss stand-in: 4 s of white noise at about -32 dBFS.
    let src = noise(48_000 * 4, 0.04);
    let mut out = src.clone();
    Denoiser::new().process(&mut out);
    // Judge after the first second, once the network has settled.
    let before = db(rms(&src[48_000..]));
    let after = db(rms(&out[48_000..]));
    println!("noise: {before:.1} dB -> {after:.1} dB");
    assert!(before - after > 10.0, "expected >10 dB less noise (measured 13.4 dB on white noise, the hardest case), got {:.1} dB", before - after);
}
