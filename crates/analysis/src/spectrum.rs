//! A sliding short-time power spectrum that never allocates after construction.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

/// Hann-windowed STFT over a ring buffer. Push samples one at a time; every `hop` samples a new
/// power spectrum is ready in [`Stft::power`].
pub struct Stft {
    size: usize,
    hop: usize,
    ring: Vec<f32>,
    write: usize,
    filled: usize,
    since_hop: usize,
    window: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    power: Vec<f32>,
    /// Scales |X|² so that a full-scale sine's peak bin reads about 1.
    norm: f32,
}

impl std::fmt::Debug for Stft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stft").field("size", &self.size).field("hop", &self.hop).finish_non_exhaustive()
    }
}

impl Stft {
    pub fn new(size: usize, hop: usize) -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(size);
        let window: Vec<f32> =
            (0..size).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / size as f32).cos()).collect();
        let gain: f32 = window.iter().sum::<f32>() / 2.0;
        Self {
            size,
            hop,
            ring: vec![0.0; size],
            write: 0,
            filled: 0,
            since_hop: 0,
            window,
            input: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            power: vec![0.0; size / 2 + 1],
            fft,
            norm: 1.0 / (gain * gain),
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Forgets all input; the next frames see silence before the new audio.
    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write = 0;
        self.filled = 0;
        self.since_hop = 0;
    }

    /// Adds a sample. Returns true when a new spectrum is ready: every `hop` samples, once at
    /// least half a window has arrived since the last reset (the frame centre is then real audio).
    #[inline]
    pub fn push(&mut self, x: f32) -> bool {
        self.ring[self.write] = x;
        self.write = (self.write + 1) % self.size;
        self.filled = (self.filled + 1).min(self.size);
        self.since_hop += 1;
        if self.since_hop < self.hop || self.filled < self.size / 2 {
            return false;
        }
        self.since_hop = 0;
        self.compute();
        true
    }

    fn compute(&mut self) {
        let (older, newer) = self.ring.split_at(self.write);
        for ((slot, x), w) in self.input.iter_mut().zip(newer.iter().chain(older)).zip(&self.window) {
            *slot = x * w;
        }
        if self.fft.process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch).is_err() {
            self.power.fill(0.0);
            return;
        }
        for (p, c) in self.power.iter_mut().zip(&self.spectrum) {
            *p = c.norm_sqr() * self.norm;
        }
    }

    /// The latest power spectrum, `size / 2 + 1` bins from 0 Hz to Nyquist.
    pub fn power(&self) -> &[f32] {
        &self.power
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_scale_sine_peaks_near_one_in_its_bin() {
        let (size, rate) = (1024, 1024.0);
        let mut stft = Stft::new(size, 256);
        let mut frames = 0;
        for n in 0..4096 {
            let x = (2.0 * std::f32::consts::PI * 100.0 * n as f32 / rate).sin();
            if stft.push(x) {
                frames += 1;
            }
        }
        // Frames from half a window in, then every hop.
        assert_eq!(frames, 1 + (4096 - 512) / 256);
        let peak = stft.power()[100];
        assert!((peak - 1.0).abs() < 0.05, "{peak}");
        assert!(stft.power()[200] < 1e-6);
    }
}
