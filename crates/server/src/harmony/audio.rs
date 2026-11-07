//! Beats from the Ear's audio measurements: tuning, folding thirds of a semitone into pitch
//! classes, and deciding which beats have pitch content worth naming.

use live_mcp_analysis::BeatReport;
use live_mcp_analysis::pitch::SUBBINS;

use super::Beat;

/// A beat counts as pitched with at least this tonalness, level and coverage.
const MIN_TONAL: f32 = 0.3;
const MIN_LEVEL_DB: f32 = -50.0;
const MIN_COVERAGE: f32 = 0.25;
/// Onset strengths below this (on the Ear's 0–255 scale) are the flutter of steady sound, not
/// attacks.
pub const ONSET_FLOOR: f32 = 12.0;
/// Tuning is only reported when the evidence points this consistently one way (resultant
/// length over total weight, 0–1).
const MIN_TUNING_AGREEMENT: f32 = 0.2;

fn amplitude(level_db: f32) -> f32 {
    10f32.powf(level_db / 20.0)
}

fn counts(report: &BeatReport) -> bool {
    report.coverage >= MIN_COVERAGE && report.tonal >= MIN_TONAL && report.level_db >= MIN_LEVEL_DB
}

/// The offset from A = 440 Hz equal temperament, in cents (−50 to 50), from where energy sits
/// within each semitone: a circular mean over the treble bins of pitched beats.
pub fn tuning_cents(reports: &[BeatReport]) -> Option<f32> {
    let (mut x, mut y, mut total) = (0.0f32, 0.0f32, 0.0f32);
    for report in reports.iter().filter(|r| counts(r)) {
        let weight = amplitude(report.level_db) * report.tonal;
        for bins in &report.treble {
            for (j, &v) in bins.iter().enumerate() {
                let w = weight * v as f32 / 255.0;
                let angle = 2.0 * std::f32::consts::PI * (j as f32 - 1.0) / SUBBINS as f32;
                x += w * angle.cos();
                y += w * angle.sin();
                total += w;
            }
        }
    }
    if total <= 0.0 || (x * x + y * y).sqrt() / total < MIN_TUNING_AGREEMENT {
        return None;
    }
    Some(y.atan2(x) / (2.0 * std::f32::consts::PI) * 100.0)
}

/// Folds thirds of a semitone into 12 pitch classes: each bin goes to the semitone nearest to it
/// once the tuning offset is taken out. (Spreading bins over two semitones would smear a note's
/// spectral leakage onto its neighbours.)
fn fold(bins: &[[u8; SUBBINS]; 12], cents: f32) -> [f32; 12] {
    let mut out = [0.0f32; 12];
    for (pc, sub) in bins.iter().enumerate() {
        for (j, &v) in sub.iter().enumerate() {
            let position = pc as f32 + (j as f32 - 1.0) / SUBBINS as f32 - cents / 100.0;
            out[(position.round() as i64).rem_euclid(12) as usize] += v as f32 / 255.0;
        }
    }
    out
}

/// The heard beats, sorted, and the tuning they were folded with.
pub fn beats(reports: &[BeatReport]) -> (Vec<Beat>, Option<f32>) {
    let tuning = tuning_cents(reports);
    let cents = tuning.unwrap_or(0.0);
    let mut beats: Vec<Beat> = reports
        .iter()
        .map(|r| {
            let chroma = fold(&r.treble, cents);
            Beat {
                index: r.beat,
                coverage: r.coverage,
                weight: amplitude(r.level_db),
                pitched: counts(r) && chroma.iter().any(|v| *v > 0.0),
                chroma,
                bass: fold(&r.bass, cents),
                octaves: r.octaves.map(|v| v as f32 / 255.0),
                onsets: r.onsets.map(|v| if (v as f32) < ONSET_FLOOR { 0.0 } else { v as f32 }),
                span: None,
                tonal: Some(r.tonal),
            }
        })
        .collect();
    beats.sort_by_key(|b| b.index);
    (beats, tuning)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn report(beat: i64, pcs: &[(usize, usize)]) -> BeatReport {
        let mut treble = [[0u8; 3]; 12];
        for &(pc, j) in pcs {
            treble[pc][j] = 255;
        }
        BeatReport {
            beat,
            coverage: 1.0,
            level_db: -18.0,
            tonal: 0.9,
            treble,
            bass: treble,
            octaves: [0, 0, 255, 0, 0, 0, 0],
            onsets: [100, 0, 0, 0],
        }
    }

    #[test]
    fn in_tune_and_sharp_material() {
        let in_tune = [report(0, &[(0, 1), (4, 1), (7, 1)])];
        assert_eq!(tuning_cents(&in_tune), Some(0.0));
        let (heard, _) = beats(&in_tune);
        assert_eq!(heard[0].chroma[4], 1.0);
        assert!(heard[0].pitched);

        // Everything a third of a semitone sharp: +33 cents, folded back onto C, E, G.
        let sharp = [report(0, &[(0, 2), (4, 2), (7, 2)])];
        let cents = tuning_cents(&sharp).unwrap();
        assert!((cents - 33.3).abs() < 0.5, "{cents}");
        let (heard, _) = beats(&sharp);
        assert!((heard[0].chroma[7] - 1.0).abs() < 1e-3, "{:?}", heard[0].chroma);
    }

    #[test]
    fn noise_and_silence_are_not_pitched() {
        let mut noisy = report(0, &[(0, 1)]);
        noisy.tonal = 0.1;
        let mut quiet = report(1, &[(0, 1)]);
        quiet.level_db = -70.0;
        let (heard, tuning) = beats(&[quiet, noisy]);
        assert_eq!(heard.iter().map(|b| (b.index, b.pitched)).collect::<Vec<_>>(), [(0, false), (1, false)]);
        assert_eq!(tuning, None);
    }
}
