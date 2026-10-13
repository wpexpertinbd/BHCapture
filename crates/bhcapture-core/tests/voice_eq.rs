use bhcapture_core::voice_eq::VoiceEq;

/// Gain in dB the EQ applies to a steady sine at `f` Hz (measured after it settles).
fn gain_at(f: f32) -> f32 {
    let rate = 48_000.0;
    let n = 48_000;
    let src: Vec<f32> = (0..n).map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / rate).sin() * 0.1).collect();
    let mut out = src.clone();
    VoiceEq::new(48_000).process(&mut out);
    let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
    20.0 * (rms(&out[24_000..]) / rms(&src[24_000..])).log10()
}

#[test]
fn rumble_below_the_voice_is_removed() {
    let g = gain_at(30.0);
    println!("30 Hz: {g:.1} dB");
    assert!(g < -12.0, "30 Hz should be cut hard, got {g:.1} dB");
}

#[test]
fn the_boom_region_is_trimmed() {
    let g = gain_at(150.0);
    println!("150 Hz: {g:.1} dB");
    assert!((-5.0..=-1.0).contains(&g), "150 Hz should come down a few dB, got {g:.1}");
}

#[test]
fn the_voice_body_is_left_alone() {
    let g = gain_at(700.0);
    println!("700 Hz: {g:.1} dB");
    assert!(g.abs() < 1.5, "700 Hz should be about unchanged, got {g:.1}");
}

#[test]
fn presence_is_lifted() {
    let g = gain_at(2_500.0);
    println!("2.5 kHz: {g:.1} dB");
    assert!((3.0..=7.0).contains(&g), "2.5 kHz should gain a few dB, got {g:.1}");
}

#[test]
fn consonants_and_air_are_lifted_most() {
    let g = gain_at(7_000.0);
    println!("7 kHz: {g:.1} dB");
    assert!((5.0..=8.0).contains(&g), "7 kHz should gain about +6 dB, got {g:.1}");
}

#[test]
fn length_is_unchanged() {
    let mut eq = VoiceEq::new(48_000);
    for n in [1usize, 37, 512] { let mut b = vec![0.1f32; n]; eq.process(&mut b); assert_eq!(b.len(), n); }
}
