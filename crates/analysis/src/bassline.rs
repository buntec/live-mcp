//! Bass pitch: the fundamental of a (mostly monophonic) bass line, by YIN (de Cheveigné &
//! Kawahara, 2002) on the low-passed mid signal, decimated to about 3 kHz, about 94 times a
//! second. Chroma cannot do this in the bass: a low note's harmonics outweigh it, and a third of a
//! semitone at 40 Hz is narrower than any usable FFT bin.

use crate::filter::Cascade2;

const TARGET_RATE: f64 = 3_000.0;
const LOWPASS_HZ: f64 = 350.0;
/// Fundamentals tracked: about B0 (30.9 Hz) to E4 (330 Hz) in scientific pitch, which is MIDI
/// 23–64 (B-1 to E3 in Live's naming).
const MIN_HZ: f64 = 30.0;
const MAX_HZ: f64 = 340.0;
pub const LOWEST_NOTE: u8 = 23;
pub const HIGHEST_NOTE: u8 = 64;
pub const NOTES: usize = (HIGHEST_NOTE - LOWEST_NOTE + 1) as usize;
/// Samples summed in the difference function (~43 ms), and samples between frames (~11 ms).
const WINDOW: usize = 128;
const HOP: usize = 32;
/// YIN's absolute threshold on the cumulative mean normalised difference: the first dip below it
/// is the period. Above it everywhere, the frame is unvoiced. Chords over a bass make its period
/// less clean, and YIN's usual 0.1–0.15 left many such 16ths without a note or on a subharmonic.
/// 0.2 was the highest that changed no key on the test set (0.25 caught a drum loop's falling
/// kick well enough to give the loop a key).
const THRESHOLD: f32 = 0.2;
/// Frames quieter than this mean square (about −60 dBFS) are unvoiced.
const SILENCE: f32 = 1e-6;

/// One analysis frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BassFrame {
    /// Fundamental, Hz.
    pub hz: f32,
    /// 0 = no pitch (silence, noise, a chord), up to 1 = a clean periodic tone.
    pub confidence: f32,
}

impl BassFrame {
    /// The nearest MIDI note and the offset from it in cents, if voiced and in range.
    pub fn note(&self) -> Option<(u8, f32)> {
        if self.confidence <= 0.0 || self.hz <= 0.0 {
            return None;
        }
        let midi = 69.0 + 12.0 * (self.hz / 440.0).log2();
        let nearest = midi.round();
        (nearest >= LOWEST_NOTE as f32 && nearest <= HIGHEST_NOTE as f32)
            .then_some((nearest as u8, (midi - nearest) * 100.0))
    }
}

#[derive(Debug)]
pub struct BassTracker {
    decimation: usize,
    rate: f32,
    antialias: Cascade2,
    phase: usize,
    ring: Vec<f32>,
    write: usize,
    filled: usize,
    since_hop: usize,
    tau_min: usize,
    tau_max: usize,
    frame: Vec<f32>,
    difference: Vec<f32>,
}

impl BassTracker {
    pub fn new(sample_rate: f64) -> Self {
        let decimation = (sample_rate / TARGET_RATE).round().max(1.0) as usize;
        let rate = sample_rate / decimation as f64;
        let tau_max = (rate / MIN_HZ).ceil() as usize;
        let tau_min = ((rate / MAX_HZ).floor() as usize).max(2);
        let len = WINDOW + tau_max + 2;
        Self {
            decimation,
            rate: rate as f32,
            antialias: Cascade2::lr4_lowpass(LOWPASS_HZ, sample_rate),
            phase: 0,
            ring: vec![0.0; len],
            write: 0,
            filled: 0,
            since_hop: 0,
            tau_min,
            tau_max,
            frame: vec![0.0; len],
            difference: vec![0.0; tau_max + 2],
        }
    }

    pub fn reset(&mut self) {
        self.antialias.reset();
        self.phase = 0;
        self.ring.fill(0.0);
        self.write = 0;
        self.filled = 0;
        self.since_hop = 0;
    }

    /// Input samples from the newest sample to the centre of a frame.
    pub fn centre_delay(&self) -> usize {
        self.ring.len() / 2 * self.decimation
    }

    /// Feeds one sample of the mid signal; returns a frame every `HOP` decimated samples once a
    /// full window has arrived.
    #[inline]
    pub fn push(&mut self, x: f32) -> Option<BassFrame> {
        let filtered = self.antialias.process(x as f64) as f32;
        self.phase += 1;
        if self.phase < self.decimation {
            return None;
        }
        self.phase = 0;
        let len = self.ring.len();
        self.ring[self.write] = filtered;
        self.write = (self.write + 1) % len;
        self.filled = (self.filled + 1).min(len);
        self.since_hop += 1;
        if self.since_hop < HOP || self.filled < len {
            return None;
        }
        self.since_hop = 0;
        Some(self.analyse())
    }

    fn analyse(&mut self) -> BassFrame {
        let (older, newer) = self.ring.split_at(self.write);
        for (slot, x) in self.frame.iter_mut().zip(newer.iter().chain(older)) {
            *slot = *x;
        }
        let x = &self.frame;
        let energy = x[..WINDOW].iter().map(|v| v * v).sum::<f32>() / WINDOW as f32;
        if energy < SILENCE {
            return BassFrame { hz: 0.0, confidence: 0.0 };
        }
        // Difference function, then its cumulative mean normalisation (d'(0) = 1).
        let d = &mut self.difference;
        d[0] = 1.0;
        let mut running = 0.0f32;
        for tau in 1..=self.tau_max + 1 {
            let raw: f32 = (0..WINDOW).map(|j| (x[j] - x[j + tau]) * (x[j] - x[j + tau])).sum();
            running += raw;
            d[tau] = if running > 0.0 { raw * tau as f32 / running } else { 1.0 };
        }
        let mut chosen = None;
        let mut tau = self.tau_min;
        while tau <= self.tau_max {
            if d[tau] < THRESHOLD {
                while tau < self.tau_max && d[tau + 1] < d[tau] {
                    tau += 1;
                }
                chosen = Some(tau);
                break;
            }
            tau += 1;
        }
        let Some(tau) = chosen else { return BassFrame { hz: 0.0, confidence: 0.0 } };
        // Parabolic interpolation around the dip.
        let (a, b, c) = (d[tau - 1], d[tau], d[tau + 1]);
        let curve = a - 2.0 * b + c;
        let shift = if curve > 0.0 { (0.5 * (a - c) / curve).clamp(-0.5, 0.5) } else { 0.0 };
        BassFrame { hz: self.rate / (tau as f32 + shift), confidence: (1.0 - b).clamp(0.0, 1.0) }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    fn notes(sample_rate: f64, seconds: f64, signal: impl Fn(f64) -> f64) -> Vec<Option<(u8, f32)>> {
        let mut tracker = BassTracker::new(sample_rate);
        (0..(seconds * sample_rate) as usize)
            .filter_map(|n| tracker.push(signal(n as f64 / sample_rate) as f32))
            .map(|f| f.note())
            .collect()
    }

    fn hz(midi: f64) -> f64 {
        440.0 * 2f64.powf((midi - 69.0) / 12.0)
    }

    fn saw(f: f64, t: f64) -> f64 {
        (1..=20).map(|h| (2.0 * PI * f * h as f64 * t).sin() / h as f64).sum::<f64>() * 0.3
    }

    #[test]
    fn sine_saw_and_square_basses_give_their_fundamental() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            for midi in [28.0, 33.0, 40.0, 45.0, 52.0, 60.0] {
                let f = hz(midi);
                let sine = notes(rate, 0.5, |t| 0.5 * (2.0 * PI * f * t).sin());
                let saws = notes(rate, 0.5, |t| saw(f, t));
                let square = notes(rate, 0.5, |t| if (f * t).fract() < 0.5 { 0.3 } else { -0.3 });
                for (kind, found) in [("sine", sine), ("saw", saws), ("square", square)] {
                    let right = found.iter().filter(|n| n.map(|(m, _)| m) == Some(midi as u8)).count();
                    assert!(right * 10 >= found.len() * 9, "{kind} MIDI {midi} at {rate}: {found:?}");
                }
            }
        }
    }

    #[test]
    fn cents_follow_detuning_and_silence_is_unvoiced() {
        let f = hz(45.0) * 2f64.powf(30.0 / 1200.0);
        let found = notes(48_000.0, 0.5, |t| saw(f, t));
        let (m, cents) = found.last().unwrap().unwrap();
        assert_eq!(m, 45);
        assert!((cents - 30.0).abs() < 5.0, "{cents}");
        assert!(notes(48_000.0, 0.3, |_| 0.0).iter().all(Option::is_none));
    }

    #[test]
    fn noise_is_mostly_unvoiced() {
        let mut state = 0x9e37_79b9u32;
        let mut tracker = BassTracker::new(48_000.0);
        let mut frames = 0;
        let mut voiced = 0;
        for _ in 0..48_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if let Some(f) = tracker.push((state as f32 / u32::MAX as f32 - 0.5) * 0.5) {
                frames += 1;
                voiced += f.note().is_some() as usize;
            }
        }
        assert!(voiced * 5 < frames, "{voiced} of {frames} noise frames voiced");
    }
}
