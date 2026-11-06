//! Running sums over a stretch of audio, and the measurements derived from them.

use serde::{Deserialize, Serialize};

use crate::filter::Cascade2;

/// Levels below this are reported as this value instead of −∞, so results stay valid JSON.
pub const FLOOR_DB: f32 = -120.0;

/// Band edges in Hz: <120, 120–500, 500–2k, 2k–6k, >6k.
pub const BAND_EDGES_HZ: [f64; 4] = [120.0, 500.0, 2_000.0, 6_000.0];
pub const BAND_NAMES: [&str; 5] = ["low", "low_mid", "mid", "high_mid", "high"];

/// Samples at or above this magnitude count as clipped.
const CLIP_LEVEL: f32 = 0.999;

/// Accumulated sums for one stretch of audio. Cheap to copy; no allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sums {
    pub samples: u64,
    peak: [f32; 2],
    square: [f64; 2],
    k_square: [f64; 2],
    band_square: [f64; 5],
    left_right: f64,
    mid_square: f64,
    side_square: f64,
    clipped: u32,
}

/// Filter state that persists across stretches (the filters must not restart at each bar).
#[derive(Debug, Clone)]
pub struct SignalMeter {
    k_weighting: [Cascade2; 2],
    /// Band filters run on the mid (L+R)/2 signal: low-pass, (high-pass, low-pass) × 3, high-pass.
    low: Cascade2,
    pass: [(Cascade2, Cascade2); 3],
    high: Cascade2,
}

impl SignalMeter {
    pub fn new(sample_rate: f64) -> Self {
        let [e0, e1, e2, e3] = BAND_EDGES_HZ;
        let hp = |f| Cascade2::lr4_highpass(f, sample_rate);
        let lp = |f| Cascade2::lr4_lowpass(f, sample_rate);
        Self {
            k_weighting: [Cascade2::k_weighting(sample_rate); 2],
            low: lp(e0),
            pass: [(hp(e0), lp(e1)), (hp(e1), lp(e2)), (hp(e2), lp(e3))],
            high: hp(e3),
        }
    }

    pub fn reset(&mut self) {
        for f in &mut self.k_weighting {
            f.reset();
        }
        self.low.reset();
        for (a, b) in &mut self.pass {
            a.reset();
            b.reset();
        }
        self.high.reset();
    }

    /// Runs `left`/`right` (equal lengths; pass the same slice twice for mono) through the
    /// filters and adds the results to `sums`.
    pub fn feed(&mut self, left: &[f32], right: &[f32], sums: &mut Sums) {
        debug_assert_eq!(left.len(), right.len());
        for (&l, &r) in left.iter().zip(right) {
            let (lf, rf) = (l as f64, r as f64);
            sums.peak[0] = sums.peak[0].max(l.abs());
            sums.peak[1] = sums.peak[1].max(r.abs());
            if l.abs() >= CLIP_LEVEL || r.abs() >= CLIP_LEVEL {
                sums.clipped += 1;
            }
            sums.square[0] += lf * lf;
            sums.square[1] += rf * rf;
            let kl = self.k_weighting[0].process(lf);
            let kr = self.k_weighting[1].process(rf);
            sums.k_square[0] += kl * kl;
            sums.k_square[1] += kr * kr;
            sums.left_right += lf * rf;
            let mid = (lf + rf) * 0.5;
            let side = (lf - rf) * 0.5;
            sums.mid_square += mid * mid;
            sums.side_square += side * side;
            let low = self.low.process(mid);
            sums.band_square[0] += low * low;
            for (i, (hp, lp)) in self.pass.iter_mut().enumerate() {
                let band = lp.process(hp.process(mid));
                sums.band_square[i + 1] += band * band;
            }
            let high = self.high.process(mid);
            sums.band_square[4] += high * high;
        }
        sums.samples += left.len() as u64;
    }
}

/// Measurements over one stretch of audio.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// Loudness per ITU-R BS.1770 (K-weighted, ungated) over the stretch, in LUFS.
    pub loudness_lufs: f32,
    /// Sample peak per channel (L, R), dBFS. Not oversampled, so true peaks can be slightly higher.
    pub peak_dbfs: [f32; 2],
    /// RMS per channel (L, R), dBFS.
    pub rms_dbfs: [f32; 2],
    /// Peak minus RMS of the louder channel, dB. Low values mean heavy compression/limiting.
    pub crest_db: f32,
    /// RMS of the mid signal in each band (see [`BAND_NAMES`]), dBFS.
    pub bands_dbfs: [f32; 5],
    /// Pearson correlation of L and R: 1 = mono, 0 = unrelated, −1 = out of phase.
    pub correlation: f32,
    /// Share of energy in the side signal: 0 = mono, 0.5 = wide/unrelated, 1 = out of phase.
    pub width: f32,
    /// Samples at or above −0.01 dBFS in either channel.
    pub clipped_samples: u32,
}

fn to_db(power_ratio: f64) -> f32 {
    if power_ratio > 0.0 { ((10.0 * power_ratio.log10()) as f32).max(FLOOR_DB) } else { FLOOR_DB }
}

impl Sums {
    pub fn stats(&self) -> Stats {
        let n = self.samples.max(1) as f64;
        let peak_dbfs = self.peak.map(|p| to_db((p as f64) * (p as f64)));
        let rms_dbfs = self.square.map(|s| to_db(s / n));
        let louder = if rms_dbfs[0] >= rms_dbfs[1] { 0 } else { 1 };
        let mean_square_k = (self.k_square[0] + self.k_square[1]) / n;
        let loudness_lufs =
            if mean_square_k > 0.0 { ((-0.691 + 10.0 * mean_square_k.log10()) as f32).max(FLOOR_DB) } else { FLOOR_DB };
        let energy = (self.square[0] * self.square[1]).sqrt();
        let correlation = if energy > 1e-20 { (self.left_right / energy).clamp(-1.0, 1.0) as f32 } else { 0.0 };
        let total = self.mid_square + self.side_square;
        let width = if total > 1e-20 { (self.side_square / total) as f32 } else { 0.0 };
        Stats {
            loudness_lufs,
            peak_dbfs,
            rms_dbfs,
            crest_db: if self.samples > 0 { peak_dbfs[louder] - rms_dbfs[louder] } else { 0.0 },
            bands_dbfs: self.band_square.map(|s| to_db(s / n)),
            correlation,
            width,
            clipped_samples: self.clipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    const SR: f64 = 48_000.0;

    fn sine(freq: f64, amplitude: f64, seconds: f64) -> Vec<f32> {
        (0..(SR * seconds) as usize).map(|i| (amplitude * (2.0 * PI * freq * i as f64 / SR).sin()) as f32).collect()
    }

    fn measure(left: &[f32], right: &[f32]) -> Stats {
        let mut meter = SignalMeter::new(SR);
        // Let the filters settle first, as they do in a running plugin.
        meter.feed(left, right, &mut Sums::default());
        let mut sums = Sums::default();
        meter.feed(left, right, &mut sums);
        sums.stats()
    }

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn ebu_reference_sine_measures_minus_23_lufs() {
        // EBU Tech 3341, case 1: stereo 1 kHz sine at −23 dBFS reads −23.0 LUFS (±0.1).
        let tone = sine(1000.0, 10f64.powf(-23.0 / 20.0), 2.0);
        let stats = measure(&tone, &tone);
        assert!(close(stats.loudness_lufs, -23.0, 0.1), "{}", stats.loudness_lufs);
        assert!(close(stats.peak_dbfs[0], -23.0, 0.01));
        assert!(close(stats.rms_dbfs[0], -26.01, 0.02), "a sine's RMS is 3 dB below its peak");
        assert!(close(stats.crest_db, 3.01, 0.02));
        assert!(close(stats.correlation, 1.0, 1e-6) && stats.width < 1e-6);
        assert_eq!(stats.clipped_samples, 0);
    }

    #[test]
    fn energy_lands_in_the_right_band() {
        for (freq, band) in [(60.0, 0), (250.0, 1), (1000.0, 2), (3500.0, 3), (12_000.0, 4)] {
            let tone = sine(freq, 0.5, 1.0);
            let bands = measure(&tone, &tone).bands_dbfs;
            let loudest = (0..5).max_by(|&a, &b| bands[a].total_cmp(&bands[b])).unwrap();
            assert_eq!(loudest, band, "{freq} Hz: {bands:?}");
            // A sine inside a band keeps most of its level there (a 0.5 sine is −9 dBFS RMS). The
            // LR4 edges are −6 dB at their cutoffs, so a tone between two edges loses a little.
            assert!(bands[band] > -12.0, "{freq} Hz: {bands:?}");
        }
    }

    #[test]
    fn stereo_correlation_and_width() {
        let left = sine(440.0, 0.5, 1.0);
        let inverted: Vec<f32> = left.iter().map(|x| -x).collect();
        let stats = measure(&left, &inverted);
        assert!(close(stats.correlation, -1.0, 1e-6) && close(stats.width, 1.0, 1e-6));
        let other = sine(1013.0, 0.5, 1.0);
        let stats = measure(&left, &other);
        assert!(stats.correlation.abs() < 0.05 && close(stats.width, 0.5, 0.05), "{stats:?}");
        let silence = vec![0.0; left.len()];
        let stats = measure(&left, &silence);
        assert_eq!(stats.rms_dbfs[1], FLOOR_DB);
        assert!(close(stats.width, 0.5, 1e-6), "one-sided signal: equal mid and side");
    }

    #[test]
    fn silence_and_clipping() {
        let silence = vec![0.0; 4800];
        let stats = measure(&silence, &silence);
        assert_eq!(stats.loudness_lufs, FLOOR_DB);
        assert_eq!(stats.peak_dbfs, [FLOOR_DB; 2]);
        assert_eq!(stats.bands_dbfs, [FLOOR_DB; 5]);
        assert_eq!((stats.correlation, stats.width, stats.crest_db), (0.0, 0.0, 0.0));
        let square: Vec<f32> = (0..4800).map(|i| if (i / 24) % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let stats = measure(&square, &square);
        assert_eq!(stats.clipped_samples, 4800);
        assert!(close(stats.crest_db, 0.0, 1e-3), "a full-scale square wave has no crest");
    }
}
