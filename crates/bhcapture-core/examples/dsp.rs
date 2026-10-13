//! Run raw mono f32 (48 kHz, little-endian) from stdin through the mic chain, to stdout.
//! `dsp none|denoise|level|both|eq`
use std::io::{Read, Write};
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).unwrap();
    let mut s: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let mut dn = bhcapture_core::denoise::Denoiser::new();
    let mut lv = bhcapture_core::level::AutoLevel::new(48_000);
    let mut eq = bhcapture_core::voice_eq::VoiceEq::new(48_000);
    // Feed in capture-sized chunks (~10.7 ms), as the recorder does.
    for c in s.chunks_mut(512) {
        if mode == "denoise" || mode == "both" { dn.process(c); }
        if mode == "eq" { eq.process(c); }
        if mode == "level" || mode == "both" { lv.process(c); }
    }
    let out: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::io::stdout().write_all(&out).unwrap();
}
