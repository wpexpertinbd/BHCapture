use bhcapture_core::level::*;

const RATE: u32 = 48_000;

/// "Speech": a 180 Hz tone with harmonics in 300 ms bursts, 200 ms pauses, at a given peak.
fn bursts(seconds: f32, peak: f32) -> Vec<f32> {
    let n = (seconds * RATE as f32) as usize;
    (0..n).map(|i| {
        let t = i as f32 / RATE as f32;
        let on = (t % 0.5) < 0.3;
        if !on { return 0.0; }
        let w = 2.0 * std::f32::consts::PI * 180.0 * t;
        (w.sin() * 0.6 + (2.0 * w).sin() * 0.3 + (3.0 * w).sin() * 0.1) * peak
    }).collect()
}
fn peak(v: &[f32]) -> f32 { v.iter().fold(0.0f32, |m, s| m.max(s.abs())) }
fn db(x: f32) -> f32 { 20.0 * x.max(1e-9).log10() }

#[test]
fn a_quiet_voice_is_brought_up_to_the_target() {
    // Benjamin's MacBook voice peaked at about -21 dBFS.
    let mut v = bursts(6.0, 0.09);
    AutoLevel::new(RATE).process(&mut v);
    let settled = peak(&v[RATE as usize * 4..]);
    println!("quiet voice: -21 dB in -> {:.1} dB out", db(settled));
    assert!((db(settled) - db(TARGET_PEAK)).abs() < 3.0, "peaks should sit near -6 dB, got {:.1}", db(settled));
}

#[test]
fn a_loud_voice_is_not_boosted_and_never_clips() {
    let mut v = bursts(4.0, 0.95);
    AutoLevel::new(RATE).process(&mut v);
    assert!(peak(&v) <= OUTPUT_CEILING, "never past -1 dBFS (headroom for AAC)");
    assert!(peak(&v[RATE as usize * 2..]) <= 0.95 + 1e-3, "a loud voice is never made louder");
}

#[test]
fn silence_between_words_is_not_boosted() {
    // Quiet voice for 3 s (gain rises), then 3 s of near-silence (-70 dB) -- the gain must HOLD,
    // not climb to MAX_GAIN and turn the pause into audible hiss.
    let mut v = bursts(3.0, 0.09);
    v.extend(std::iter::repeat(0.0003f32).take(RATE as usize * 3));
    let mut a = AutoLevel::new(RATE);
    a.process(&mut v[..RATE as usize * 3]);
    let g_speech = a.gain();
    a.process(&mut v[RATE as usize * 3..]);
    assert!((a.gain() - g_speech).abs() < 0.01, "gain moved during silence: {g_speech} -> {}", a.gain());
    assert!(peak(&v[RATE as usize * 4..]) < 0.01, "the pause stays quiet");
}

#[test]
fn a_sudden_shout_is_caught_quickly() {
    // Quiet speech (gain up), then a shout 20 dB louder: within 50 ms the output must be under full scale
    // and within 300 ms back near the target.
    let mut v = bursts(3.0, 0.09);
    let shout = bursts(1.0, 0.9);
    v.extend(shout);
    AutoLevel::new(RATE).process(&mut v);
    let start = RATE as usize * 3;
    assert!(peak(&v[start..]) <= OUTPUT_CEILING);
    let later = peak(&v[start + RATE as usize / 2..start + RATE as usize * 8 / 10]);
    println!("shout settles at {:.1} dB", db(later));
    assert!(db(later) < -2.0, "the shout is brought down, got {:.1} dB", db(later));
}

#[test]
fn length_is_unchanged_for_any_buffer_size() {
    let mut a = AutoLevel::new(RATE);
    for n in [1usize, 13, 480, 4096] {
        let mut b = bursts(0.1, 0.1);
        b.truncate(n);
        a.process(&mut b);
        assert_eq!(b.len(), n);
    }
}

#[test]
fn no_sample_ever_passes_the_ceiling_even_with_huge_gain_and_spikes() {
    // Quiet speech drives the gain up, then single-sample spikes at full scale arrive.
    let mut v = bursts(3.0, 0.02);
    for i in (0..v.len()).step_by(4801) { v[i] = 1.0; }
    AutoLevel::new(RATE).process(&mut v);
    assert!(peak(&v) <= OUTPUT_CEILING, "peak {}", peak(&v));
}
