//! Streaming linear resampler with channel folding, for audio devices that do not run at 48 kHz.
//!
//! Windows (and later Linux) hands audio over in whatever rate and layout the device mixes at --
//! 44.1 kHz stereo, 48 kHz 7.1, a mono 16 kHz headset. Everything BHCapture records is 48 kHz, and
//! the microphone chain (RNNoise) only works at 48 kHz, so each input goes through one of these.
//! Linear interpolation is plenty for speech and system sound at these ratios; it is continuous
//! across packets (the last input frame is carried over), so packet boundaries leave no clicks.

pub struct Resampler {
    in_ch: usize,
    step: f64,     // input frames per output frame
    pos: f64,      // position of the next output frame, in input frames; -1.0 = the carried frame
    prev: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, in_ch: usize) -> Self {
        let in_ch = in_ch.max(1);
        Self { in_ch, step: in_rate.max(1) as f64 / out_rate.max(1) as f64, pos: 0.0, prev: vec![0.0; in_ch] }
    }

    /// Resample interleaved `input` (`in_ch` channels) and append to `out` as `out_ch` channels:
    /// 1 = mono (average of all), 2 = stereo (odd channels left, even right -- a 5.1/7.1 mix folds down).
    pub fn process(&mut self, input: &[f32], out_ch: usize, out: &mut Vec<f32>) {
        let ch = self.in_ch;
        let frames = input.len() / ch;
        if frames == 0 { return; }
        let at = |i: isize, c: usize, prev: &[f32]| if i < 0 { prev[c] } else { input[i as usize * ch + c] };
        while self.pos <= (frames - 1) as f64 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for c in 0..ch {
                let a = at(i, c, &self.prev);
                // Exactly on the packet's last frame (t = 0) the next one is not needed.
                let b = if ((i + 1) as usize) < frames { at(i + 1, c, &self.prev) } else { a };
                let v = a + (b - a) * t;
                if ch == 1 { l = v; r = v; } else if c % 2 == 0 { l += v; } else { r += v; }
            }
            if ch > 2 { let k = 2.0 / ch as f32; l *= k; r *= k; }
            if out_ch == 1 { out.push((l + r) * 0.5); } else { out.push(l); out.push(r); }
            self.pos += self.step;
        }
        // Re-base onto the next packet: this packet's last frame becomes index -1 there.
        self.pos -= frames as f64;
        self.prev.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
    }
}
