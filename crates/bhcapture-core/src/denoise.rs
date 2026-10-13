//! Microphone noise filter: RNNoise (Xiph), via the pure-Rust `nnnoiseless` port.
//!
//! A small recurrent network that keeps speech and suppresses steady background noise -- fans, AC,
//! hum, room hiss. Portable on purpose: macOS's own Voice Isolation cannot be switched on by an app
//! (only the user can, from Control Center, and only for voice-call audio), and most Windows/Linux
//! machines have nothing built in, so one filter in core serves every OS the same way.
//!
//! RNNoise works on fixed 480-sample frames at 48 kHz; capture buffers arrive in any size. So the
//! filter keeps a FIFO each way and is primed with one frame of silence: every call returns exactly
//! as many samples as it was given, delayed by a constant 10 ms. Callers keep their timestamps --
//! 10 ms is below what anyone hears as lip-sync error.

use nnnoiseless::DenoiseState;
use std::collections::VecDeque;

/// RNNoise's frame: 10 ms at 48 kHz.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;
/// The only rate the model is trained for. Callers with another rate must not use the filter.
pub const SAMPLE_RATE: u32 = 48_000;

/// RNNoise expects samples in 16-bit range, not -1.0..1.0.
const SCALE: f32 = 32_768.0;

pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    pending: VecDeque<f32>,
    ready: VecDeque<f32>,
    inbuf: Vec<f32>,
    outbuf: Vec<f32>,
}

impl Default for Denoiser {
    fn default() -> Self { Self::new() }
}

impl Denoiser {
    pub fn new() -> Self {
        let mut ready = VecDeque::with_capacity(FRAME * 4);
        ready.extend(std::iter::repeat(0.0).take(FRAME)); // the constant 10 ms delay
        Self { state: DenoiseState::new(), pending: VecDeque::with_capacity(FRAME * 4), ready,
               inbuf: vec![0.0; FRAME], outbuf: vec![0.0; FRAME] }
    }

    /// Filter mono float samples (-1.0..1.0) IN PLACE. Same length out as in.
    pub fn process(&mut self, samples: &mut [f32]) {
        self.pending.extend(samples.iter().map(|s| s * SCALE));
        while self.pending.len() >= FRAME {
            for (i, v) in self.pending.drain(..FRAME).enumerate() { self.inbuf[i] = v; }
            self.state.process_frame(&mut self.outbuf, &self.inbuf);
            self.ready.extend(self.outbuf.iter().map(|s| (s / SCALE).clamp(-1.0, 1.0)));
        }
        for s in samples.iter_mut() {
            // Always enough: primed with one frame, and every FRAME in produces FRAME out.
            *s = self.ready.pop_front().unwrap_or(0.0);
        }
    }
}
