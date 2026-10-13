//! Voice EQ for the microphone: the standard "clear speech" tone curve.
//!
//! Screen-capture APIs hand over the RAW microphone, without the voice shaping a call app applies.
//! Benjamin's MacBook voice measured bass-heavy and short on presence (2026-10-13: <250 Hz only 2 dB
//! under the 250-1000 Hz body, 1-4 kHz 11 dB under it) and he heard it as "a deep voice, serious".
//! This is the usual broadcast/podcast correction:
//!
//! - high-pass 80 Hz: rumble, desk thumps, fan drone carry no speech;
//! - low-shelf 200 Hz -3 dB: the "boom" a laptop mic adds;
//! - peak 2.5 kHz +3 dB (broad): presence, where intelligibility lives;
//! - high-shelf 4.5 kHz +6 dB: the consonants and "air" the raw mic was missing most.
//!
//! Tuned to the MEASURED gap, not by ear: against a clear reference voice his recording was 4 dB
//! short at 1-4 kHz and 12 dB short at 4-8 kHz (relative to the 250-1000 Hz body). A first, gentler
//! curve (+5 dB at 3.5 kHz, +2 dB above 9 kHz) closed only ~4 dB of the 12.
//!
//! RBJ "Audio EQ Cookbook" biquads, direct form I, f32. Any buffer size, same length out as in.

use std::f32::consts::PI;

#[derive(Clone, Copy)]
struct Biquad { b0: f32, b1: f32, b2: f32, a1: f32, a2: f32, x1: f32, x2: f32, y1: f32, y2: f32 }

impl Biquad {
    fn from(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }
    fn highpass(rate: f32, f: f32, q: f32) -> Self {
        let w = 2.0 * PI * f / rate; let (s, c) = w.sin_cos(); let al = s / (2.0 * q);
        Self::from((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }
    fn peak(rate: f32, f: f32, q: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w = 2.0 * PI * f / rate; let (s, c) = w.sin_cos(); let al = s / (2.0 * q);
        Self::from(1.0 + al * a, -2.0 * c, 1.0 - al * a, 1.0 + al / a, -2.0 * c, 1.0 - al / a)
    }
    fn shelf(rate: f32, f: f32, db: f32, high: bool) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w = 2.0 * PI * f / rate; let (s, c) = w.sin_cos();
        let al = s / 2.0 * (2.0f32).sqrt(); // shelf slope S = 1
        let sq = 2.0 * a.sqrt() * al;
        if high {
            Self::from(a * ((a + 1.0) + (a - 1.0) * c + sq), -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                       a * ((a + 1.0) + (a - 1.0) * c - sq), (a + 1.0) - (a - 1.0) * c + sq,
                       2.0 * ((a - 1.0) - (a + 1.0) * c), (a + 1.0) - (a - 1.0) * c - sq)
        } else {
            Self::from(a * ((a + 1.0) - (a - 1.0) * c + sq), 2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                       a * ((a + 1.0) - (a - 1.0) * c - sq), (a + 1.0) + (a - 1.0) * c + sq,
                       -2.0 * ((a - 1.0) + (a + 1.0) * c), (a + 1.0) + (a - 1.0) * c - sq)
        }
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1; self.x1 = x; self.y2 = self.y1; self.y1 = y;
        y
    }
}

pub struct VoiceEq { stages: [Biquad; 4] }

impl VoiceEq {
    pub fn new(sample_rate: u32) -> Self {
        let r = sample_rate.max(16_000) as f32;
        // The shelf must sit well under Nyquist; at low rates it is simply moved down.
        let air = 4_500f32.min(r * 0.4);
        Self { stages: [
            Biquad::highpass(r, 80.0, 0.707),
            Biquad::shelf(r, 200.0, -3.0, false),
            Biquad::peak(r, 2_500.0, 0.8, 3.0),
            Biquad::shelf(r, air, 6.0, true),
        ] }
    }

    /// Filter mono float samples IN PLACE.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let mut v = *s;
            for st in self.stages.iter_mut() { v = st.run(v); }
            *s = v;
        }
    }
}
