//! Pitch content: a short-time spectrum of the mid signal, decimated to about 12 kHz, mapped onto
//! bins a third of a semitone wide and folded into chroma (energy per pitch class).

use std::ops::Range;

use crate::filter::Cascade2;
use crate::spectrum::Stft;

/// Bins per semitone: the equal-tempered pitch (A = 440 Hz) and a third of a semitone either
/// side, so the server can measure tuning before folding to 12 pitch classes.
pub const SUBBINS: usize = 3;
pub const OCTAVES: usize = 7;

/// Notes analysed, as MIDI numbers: C0 to B6 in Live's naming (C1 to B7 in scientific pitch).
const LOW_NOTE: i32 = 24;
const HIGH_NOTE: i32 = 107;
/// The bass chroma takes notes up to here (B2 in Live's naming), the treble chroma from here
/// (C2) up.
const BASS_TOP: i32 = 59;
const TREBLE_BOTTOM: i32 = 48;

const TARGET_RATE: f64 = 12_000.0;
const ANTIALIAS_HZ: f64 = 4_500.0;
/// ~340 ms at 12 kHz: 2.9 Hz bins, enough to tell semitones apart from about 55 Hz up.
pub const FFT_SIZE: usize = 4096;
/// ~85 ms at 12 kHz.
pub const HOP: usize = 1024;
/// Chroma values are ln(1 + COMPRESSION · magnitude), so quiet notes still count.
const COMPRESSION: f32 = 100.0;
/// Spectral flatness is measured over this range to tell pitched sound from noise.
const FLATNESS_HZ: (f64, f64) = (100.0, 4_000.0);
/// Flatness of white noise's power spectrum (e^−γ): tonalness 0.
const NOISE_FLATNESS: f32 = 0.56;

/// What one spectrum frame holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchFrame {
    /// Compressed energy per third of a semitone, folded over octaves: index 3·pc + j, where j = 1
    /// is the equal-tempered pitch class pc and j = 0, 2 are a third of a semitone below and above.
    pub treble: [f32; 12 * SUBBINS],
    pub bass: [f32; 12 * SUBBINS],
    /// Power per octave, Live's octaves 0–6 (C0 = MIDI 24 … B6 = MIDI 107).
    pub octaves: [f32; OCTAVES],
    /// 0 = noise, 1 = clearly pitched.
    pub tonal: f32,
    /// Total power over the analysed notes.
    pub power: f32,
}

#[derive(Debug, Clone)]
struct Band {
    note: i32,
    sub: usize,
    first_bin: usize,
    weights: Range<usize>,
}

#[derive(Debug)]
pub struct PitchMeter {
    decimation: usize,
    antialias: Cascade2,
    phase: usize,
    stft: Stft,
    bands: Vec<Band>,
    weights: Vec<f32>,
    flatness_bins: Range<usize>,
}

fn note_hz(note: f64) -> f64 {
    440.0 * 2f64.powf((note - 69.0) / 12.0)
}

impl PitchMeter {
    pub fn new(sample_rate: f64) -> Self {
        let decimation = (sample_rate / TARGET_RATE).round().max(1.0) as usize;
        let rate = sample_rate / decimation as f64;
        let bin_hz = rate / FFT_SIZE as f64;
        let mut bands = Vec::new();
        let mut weights = Vec::new();
        for note in LOW_NOTE..=HIGH_NOTE {
            for sub in 0..SUBBINS {
                let centre = note as f64 + (sub as f64 - 1.0) / SUBBINS as f64;
                let (lo, mid, hi) =
                    (note_hz(centre - 1.0 / SUBBINS as f64), note_hz(centre), note_hz(centre + 1.0 / SUBBINS as f64));
                let start = weights.len();
                let (first, last) = ((lo / bin_hz).ceil() as usize, (hi / bin_hz).floor() as usize);
                let first_bin = if last > first {
                    // A triangle peaking at 1 on the band centre.
                    for k in first..=last {
                        let f = k as f64 * bin_hz;
                        let w = if f <= mid { (f - lo) / (mid - lo) } else { (hi - f) / (hi - mid) };
                        weights.push(w.max(0.0) as f32);
                    }
                    first
                } else {
                    // Narrower than a bin: interpolate between the two nearest bins.
                    let position = mid / bin_hz;
                    let k = position.floor();
                    weights.push((1.0 - (position - k)) as f32);
                    weights.push((position - k) as f32);
                    k as usize
                };
                bands.push(Band { note, sub, first_bin, weights: start..weights.len() });
            }
        }
        let flatness_bins = (FLATNESS_HZ.0 / bin_hz) as usize..(FLATNESS_HZ.1 / bin_hz) as usize;
        Self {
            decimation,
            antialias: Cascade2::lr4_lowpass(ANTIALIAS_HZ.min(0.45 * rate), sample_rate),
            phase: 0,
            stft: Stft::new(FFT_SIZE, HOP),
            bands,
            weights,
            flatness_bins,
        }
    }

    pub fn reset(&mut self) {
        self.antialias.reset();
        self.phase = 0;
        self.stft.reset();
    }

    /// Input samples from the newest sample to the centre of a frame.
    pub fn centre_delay(&self) -> usize {
        FFT_SIZE / 2 * self.decimation
    }

    /// Feeds one sample of the mid signal; returns a frame every `HOP` decimated samples.
    #[inline]
    pub fn push(&mut self, x: f32) -> Option<PitchFrame> {
        let filtered = self.antialias.process(x as f64) as f32;
        self.phase += 1;
        if self.phase < self.decimation {
            return None;
        }
        self.phase = 0;
        self.stft.push(filtered).then(|| self.frame())
    }

    fn frame(&self) -> PitchFrame {
        let power = self.stft.power();
        let mut frame =
            PitchFrame { treble: [0.0; 36], bass: [0.0; 36], octaves: [0.0; OCTAVES], tonal: 0.0, power: 0.0 };
        for band in &self.bands {
            let energy: f32 = self.weights[band.weights.clone()]
                .iter()
                .enumerate()
                .map(|(j, w)| w * power.get(band.first_bin + j).copied().unwrap_or(0.0))
                .sum();
            let compressed = (1.0 + COMPRESSION * energy.sqrt()).ln();
            let index = (band.note.rem_euclid(12) as usize) * SUBBINS + band.sub;
            if band.note >= TREBLE_BOTTOM {
                frame.treble[index] += compressed;
            }
            if band.note <= BASS_TOP {
                frame.bass[index] += compressed;
            }
            frame.octaves[((band.note - LOW_NOTE) / 12) as usize] += energy;
            frame.power += energy;
        }
        let bins = &power[self.flatness_bins.start.min(power.len())..self.flatness_bins.end.min(power.len())];
        if !bins.is_empty() {
            let mean = bins.iter().sum::<f32>() / bins.len() as f32;
            if mean > 1e-12 {
                let log_mean = bins.iter().map(|p| (p + 1e-12).ln()).sum::<f32>() / bins.len() as f32;
                let flatness = log_mean.exp() / mean;
                frame.tonal = (1.0 - flatness / NOISE_FLATNESS).clamp(0.0, 1.0);
            }
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(sample_rate: f64, seconds: f64, signal: impl Fn(f64) -> f64) -> Vec<PitchFrame> {
        let mut meter = PitchMeter::new(sample_rate);
        (0..(seconds * sample_rate) as usize)
            .filter_map(|n| meter.push(signal(n as f64 / sample_rate) as f32))
            .collect()
    }

    fn sine(hz: f64) -> impl Fn(f64) -> f64 {
        move |t| 0.5 * (2.0 * std::f64::consts::PI * hz * t).sin()
    }

    fn argmax(values: &[f32]) -> usize {
        values.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0
    }

    #[test]
    fn a_sine_lands_on_its_pitch_class_and_octave() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            let last = *frames(rate, 1.0, sine(440.0)).last().unwrap();
            assert_eq!(argmax(&last.treble), 9 * 3 + 1, "A at {rate}");
            assert_eq!(argmax(&last.octaves), 3, "A440 is A3 in Live"); // MIDI 69
            assert!(last.tonal > 0.9, "{}", last.tonal);
        }
    }

    #[test]
    fn detuned_notes_fall_in_the_side_bins_and_bass_is_separate() {
        let sharp = 440.0 * 2f64.powf(0.33 / 12.0); // +33 cents
        let last = *frames(48_000.0, 1.0, sine(sharp)).last().unwrap();
        assert_eq!(argmax(&last.treble), 9 * 3 + 2);
        let low = *frames(48_000.0, 1.0, sine(note_hz(36.0))).last().unwrap(); // C1 in Live
        assert_eq!(argmax(&low.bass) / 3, 0);
        assert_eq!(argmax(&low.octaves), 1);
    }

    #[test]
    fn noise_is_not_tonal() {
        let mut state = 0x1234_5678u32;
        let noise = move |_t: f64| {
            // xorshift; the closure is called once per sample in order.
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f64 / u32::MAX as f64 - 0.5) * 0.5
        };
        let mut meter = PitchMeter::new(48_000.0);
        let mut noise = noise;
        let last = (0..48_000).filter_map(|n| meter.push(noise(n as f64) as f32)).last().unwrap();
        assert!(last.tonal < 0.2, "{}", last.tonal);
    }
}
