use bhcapture_core::resample::Resampler;

fn sine(rate: u32, f: f32, secs: f32, ch: usize) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    (0..n).flat_map(|i| {
        let v = (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32).sin() * 0.5;
        std::iter::repeat(v).take(ch)
    }).collect()
}

/// Dominant frequency by zero crossings (a clean sine has two per cycle).
fn freq(v: &[f32], rate: u32) -> f32 {
    let z = v.windows(2).filter(|w| (w[0] <= 0.0) != (w[1] <= 0.0)).count();
    z as f32 / 2.0 / (v.len() as f32 / rate as f32)
}

#[test]
fn same_rate_is_a_pass_through_after_the_first_frame() {
    let src = sine(48_000, 440.0, 0.1, 1);
    let mut out = Vec::new();
    Resampler::new(48_000, 48_000, 1).process(&src, 1, &mut out);
    assert_eq!(out.len(), src.len());
    // Output frame k is the input at position k-1 interpolated with k... at step 1.0 exactly input.
    for (a, b) in out.iter().zip(src.iter()) { assert!((a - b).abs() < 1e-6); }
}

#[test]
fn rate_conversion_keeps_duration_and_pitch() {
    // 1 s of 440 Hz at 44.1 kHz in uneven packets -> 1 s at 48 kHz, still 440 Hz.
    let src = sine(44_100, 440.0, 1.0, 2);
    let mut r = Resampler::new(44_100, 48_000, 2);
    let mut out = Vec::new();
    let mut i = 0;
    for step in [441usize, 1000, 7, 2205].iter().cycle() {
        if i >= src.len() / 2 { break; }
        let j = (i + step).min(src.len() / 2);
        r.process(&src[i * 2..j * 2], 1, &mut out);
        i = j;
    }
    let secs = out.len() as f32 / 48_000.0;
    assert!((secs - 1.0).abs() < 0.002, "duration {secs}");
    let f = freq(&out, 48_000);
    assert!((f - 440.0).abs() < 2.0, "pitch {f}");
}

#[test]
fn packet_boundaries_leave_no_jumps() {
    // A smooth sine split into tiny packets: no output step may be larger than the signal allows.
    let src = sine(44_100, 200.0, 0.5, 1);
    let mut r = Resampler::new(44_100, 48_000, 1);
    let mut out = Vec::new();
    for c in src.chunks(37) { r.process(c, 1, &mut out); }
    let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
    // 200 Hz at 0.5 amplitude moves at most 2*pi*200*0.5/48000 ~= 0.013 per sample.
    assert!(max_step < 0.02, "a discontinuity of {max_step}");
}

#[test]
fn surround_folds_down_to_stereo_without_clipping() {
    // 8 channels all at 0.5: left and right must stay at 0.5, not 4x.
    let src = vec![0.5f32; 8 * 480];
    let mut out = Vec::new();
    Resampler::new(48_000, 48_000, 8).process(&src, 2, &mut out);
    assert!(out.iter().all(|v| (v - 0.5).abs() < 1e-6), "{:?}", &out[..4]);
}
