//! Automatic voice level for the microphone.
//!
//! Screen-capture APIs hand over the RAW microphone: no automatic gain like a call app has. On a
//! MacBook a normal speaking voice peaked at about -21 dBFS (Benjamin's recording, 2026-10-13), so
//! playback needed full volume. This brings speech up to a standard level:
//!
//! - an envelope follower (fast attack, slow release) tracks how loud the voice is;
//! - a second, FAST follower says whether someone is speaking right now. The gain only moves while
//!   it is: in the tail after a word the slow follower is still fading, and adjusting then would
//!   raise the gain into the pause (found by the silence test);
//! - the gain moves towards `TARGET / envelope`: DOWN quickly (a sudden shout must not distort),
//!   UP slowly (no pumping between words);
//! - below the gate (silence, breath, what the noise filter left) the gain is HELD -- never boosted,
//!   which is what makes an auto-level sound like hiss rising in every pause;
//! - a soft limiter from -3 dBFS, never passing -1 dBFS, catches what the gain could not react to
//!   in time (word onsets are faster than any envelope).
//!
//! Pure per-sample arithmetic, any buffer size, same length out as in.

/// Where speech peaks should sit: -6 dBFS.
pub const TARGET_PEAK: f32 = 0.5;
/// Never quieter than the input, never more than +24 dB.
pub const MAX_GAIN: f32 = 16.0;
/// Below this envelope (-50 dBFS) the input is treated as silence and the gain is held.
pub const GATE: f32 = 0.003;
/// Starting gain (+12 dB): a typical laptop voice, so the first words are not quiet while it settles.
pub const START_GAIN: f32 = 4.0;
/// The limiter starts bending here (-3 dBFS)...
const KNEE: f32 = 0.71;
/// ...and output never reaches past this (-1 dBFS). Headroom matters: AAC encoding overshoots a
/// signal that touches full scale, and a real voice through this measured 0.0 dBFS peaks with the
/// limiter at full scale (2026-10-13).
pub const OUTPUT_CEILING: f32 = 0.89;

pub struct AutoLevel {
    env: f32,
    fast: f32,
    fast_attack: f32,
    fast_release: f32,
    gain: f32,
    attack: f32,
    release: f32,
    gain_down: f32,
    gain_up: f32,
}

/// One-pole smoothing coefficient for a time constant of `ms` at `rate` Hz.
fn coef(ms: f32, rate: f32) -> f32 { 1.0 - (-1.0 / (ms * 0.001 * rate)).exp() }

impl AutoLevel {
    pub fn new(sample_rate: u32) -> Self {
        let r = sample_rate.max(8_000) as f32;
        Self {
            env: 0.0,
            fast: 0.0,
            fast_attack: coef(2.0, r),
            fast_release: coef(60.0, r),
            gain: START_GAIN,
            attack: coef(5.0, r),
            release: coef(400.0, r),
            gain_down: coef(20.0, r),
            gain_up: coef(1500.0, r),
        }
    }

    pub fn gain(&self) -> f32 { self.gain }

    /// Level mono float samples (-1.0..1.0) IN PLACE.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let a = s.abs();
            let k = if a > self.env { self.attack } else { self.release };
            self.env += (a - self.env) * k;
            let kf = if a > self.fast { self.fast_attack } else { self.fast_release };
            self.fast += (a - self.fast) * kf;
            // Speaking now: above the gate AND not just the fading tail of what came before.
            if self.fast > GATE && self.fast > self.env * 0.3 {
                let want = (TARGET_PEAK / self.env).clamp(1.0, MAX_GAIN);
                let k = if want < self.gain { self.gain_down } else { self.gain_up };
                self.gain += (want - self.gain) * k;
            }
            *s = soft_limit(*s * self.gain);
        }
    }
}

/// Unchanged up to the knee, then bends smoothly towards the ceiling instead of clipping hard.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE { return x; }
    let head = OUTPUT_CEILING - KNEE;
    x.signum() * (KNEE + head * ((a - KNEE) / head).tanh())
}
