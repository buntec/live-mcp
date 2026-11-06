//! Gated integrated loudness (ITU-R BS.1770-4) and loudness range (EBU Tech 3342) over a
//! whole programme.
//!
//! Unlike [`crate::BarAnalyzer`], this keeps per-block energies, so memory grows with the
//! programme's length (about 80 bytes per second). It is meant for offline analysis, not
//! the audio thread.

use crate::filter::Cascade2;
use crate::meter::FLOOR_DB;

/// Energies are collected in 100 ms steps. Loudness blocks are 400 ms (4 steps), short-term
/// windows for loudness range are 3 s (30 steps).
const STEP_SECONDS: f64 = 0.1;
const BLOCK_STEPS: usize = 4;
const SHORT_TERM_STEPS: usize = 30;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;

#[derive(Debug, Clone)]
pub struct ProgrammeLoudness {
    k_weighting: [Cascade2; 2],
    step_len: usize,
    in_step: usize,
    step_energy: f64,
    /// Sum over both channels of K-weighted mean square, per 100 ms step.
    steps: Vec<f64>,
}

fn lufs(mean_square: f64) -> f64 {
    if mean_square > 0.0 { -0.691 + 10.0 * mean_square.log10() } else { f64::NEG_INFINITY }
}

impl ProgrammeLoudness {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            k_weighting: [Cascade2::k_weighting(sample_rate); 2],
            step_len: (sample_rate * STEP_SECONDS).round() as usize,
            in_step: 0,
            step_energy: 0.0,
            steps: Vec::new(),
        }
    }

    /// Adds stereo audio (pass the same slice twice for mono).
    pub fn feed(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            let kl = self.k_weighting[0].process(l as f64);
            let kr = self.k_weighting[1].process(r as f64);
            self.step_energy += kl * kl + kr * kr;
            self.in_step += 1;
            if self.in_step == self.step_len {
                self.steps.push(self.step_energy / self.step_len as f64);
                self.step_energy = 0.0;
                self.in_step = 0;
            }
        }
    }

    /// Mean energy of each window of `steps` steps, advancing one step at a time.
    fn windows(&self, steps: usize) -> Vec<f64> {
        if self.steps.len() < steps {
            return Vec::new();
        }
        self.steps.windows(steps).map(|w| w.iter().sum::<f64>() / steps as f64).collect()
    }

    /// Gated integrated loudness in LUFS (BS.1770-4: 400 ms blocks with 75% overlap, absolute
    /// gate at −70 LUFS, relative gate 10 LU below the absolute-gated loudness).
    pub fn integrated(&self) -> f32 {
        let blocks = self.windows(BLOCK_STEPS);
        let above_absolute: Vec<f64> = blocks.into_iter().filter(|&e| lufs(e) > ABSOLUTE_GATE_LUFS).collect();
        if above_absolute.is_empty() {
            return FLOOR_DB;
        }
        let relative_gate = lufs(mean(&above_absolute)) - 10.0;
        let gated: Vec<f64> = above_absolute.into_iter().filter(|&e| lufs(e) > relative_gate).collect();
        (lufs(mean(&gated)) as f32).max(FLOOR_DB)
    }

    /// Loudness range in LU (EBU Tech 3342: 3 s short-term loudness, absolute gate at −70 LUFS,
    /// relative gate 20 LU below the gated mean, the spread between the 10th and 95th
    /// percentiles).
    pub fn range(&self) -> f32 {
        let short_term = self.windows(SHORT_TERM_STEPS);
        let above_absolute: Vec<f64> = short_term.into_iter().filter(|&e| lufs(e) > ABSOLUTE_GATE_LUFS).collect();
        if above_absolute.is_empty() {
            return 0.0;
        }
        let relative_gate = lufs(mean(&above_absolute)) - 20.0;
        let mut levels: Vec<f64> = above_absolute.into_iter().map(lufs).filter(|&l| l > relative_gate).collect();
        levels.sort_by(f64::total_cmp);
        (percentile(&levels, 0.95) - percentile(&levels, 0.10)) as f32
    }

    /// The loudest 3 s window, in LUFS ("maximum short-term loudness").
    pub fn max_short_term(&self) -> f32 {
        let loudest = self.windows(SHORT_TERM_STEPS).into_iter().fold(0.0, f64::max);
        (lufs(loudest) as f32).max(FLOOR_DB)
    }
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    const SR: f64 = 48_000.0;

    /// A stereo 1 kHz programme: (seconds, level in dBFS) segments, phase continuous.
    fn programme(segments: &[(f64, f64)]) -> ProgrammeLoudness {
        let mut meter = ProgrammeLoudness::new(SR);
        let mut i = 0usize;
        for &(seconds, dbfs) in segments {
            let amplitude = 10f64.powf(dbfs / 20.0);
            let n = (seconds * SR) as usize;
            let tone: Vec<f32> =
                (i..i + n).map(|k| (amplitude * (2.0 * PI * 1000.0 * k as f64 / SR).sin()) as f32).collect();
            meter.feed(&tone, &tone);
            i += n;
        }
        meter
    }

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn ebu_tech_3341_integrated_loudness_cases() {
        // Cases 1–4 of EBU Tech 3341: the expected value is −23.0 LUFS (±0.1), except case 2.
        let case1 = programme(&[(20.0, -23.0)]).integrated();
        let case2 = programme(&[(20.0, -33.0)]).integrated();
        // Relative gate: the −36 dBFS parts are more than 10 LU below and are ignored.
        let case3 = programme(&[(10.0, -36.0), (60.0, -23.0), (10.0, -36.0)]).integrated();
        // Absolute gate: the −72 dBFS parts fall below −70 LUFS and are ignored.
        let case4 =
            programme(&[(10.0, -72.0), (10.0, -36.0), (60.0, -23.0), (10.0, -36.0), (10.0, -72.0)]).integrated();
        assert!(close(case1, -23.0, 0.1), "case 1: {case1}");
        assert!(close(case2, -33.0, 0.1), "case 2: {case2}");
        assert!(close(case3, -23.0, 0.1), "case 3: {case3}");
        assert!(close(case4, -23.0, 0.1), "case 4: {case4}");
    }

    #[test]
    fn ebu_tech_3342_loudness_range_cases() {
        // EBU Tech 3342 cases 1–3: two 20 s halves at different levels, LRA within ±1 LU.
        let case1 = programme(&[(20.0, -20.0), (20.0, -30.0)]).range();
        let case2 = programme(&[(20.0, -20.0), (20.0, -15.0)]).range();
        let case3 = programme(&[(20.0, -40.0), (20.0, -20.0)]).range();
        assert!(close(case1, 10.0, 1.0), "case 1: {case1}");
        assert!(close(case2, 5.0, 1.0), "case 2: {case2}");
        assert!(close(case3, 20.0, 1.0), "case 3: {case3}");
    }

    #[test]
    fn short_term_maximum_and_silence() {
        let meter = programme(&[(10.0, -30.0), (5.0, -18.0), (10.0, -30.0)]);
        assert!(close(meter.max_short_term(), -18.0, 0.1), "{}", meter.max_short_term());
        let silent = programme(&[(5.0, -150.0)]);
        assert_eq!((silent.integrated(), silent.range(), silent.max_short_term()), (FLOOR_DB, 0.0, FLOOR_DB));
        let short = programme(&[(0.2, -20.0)]);
        assert_eq!(short.integrated(), FLOOR_DB, "shorter than one 400 ms block");
    }
}
