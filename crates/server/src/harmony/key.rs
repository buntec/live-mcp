//! Key estimation: correlating a pitch-class profile with the Krumhansl–Kessler key profiles.

use super::names::spelling;

/// Krumhansl & Kessler (1982) probe-tone ratings, tonic first.
const MAJOR_PROFILE: [f32; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const MINOR_PROFILE: [f32; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

/// Softmax temperature over the 24 correlations. A margin of 0.1 in correlation is a factor of
/// e² ≈ 7 in confidence.
const TEMPERATURE: f32 = 0.05;
/// A runner-up at least this likely is reported as an alternative.
const ALT_MIN: f32 = 0.15;
/// A key needs at least this many pitch classes with at least `PRESENT` of the strongest one's
/// weight: one or two notes do not imply a key.
const MIN_PITCH_CLASSES: usize = 3;
const PRESENT: f32 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// Pitch class of the tonic, 0 = C.
    pub tonic: u8,
    pub minor: bool,
}

impl Key {
    pub fn name(self) -> String {
        let mode = if self.minor { "minor" } else { "major" };
        format!("{} {mode}", spelling(Some(self))[self.tonic as usize])
    }

    /// The key's scale as a pitch-class set. Minor includes the raised 7th (harmonic minor), so
    /// V and V7 count as in the key.
    pub fn scale(self) -> [bool; 12] {
        let steps: &[u8] = if self.minor { &[0, 2, 3, 5, 7, 8, 10, 11] } else { &[0, 2, 4, 5, 7, 9, 11] };
        let mut set = [false; 12];
        for step in steps {
            set[((self.tonic + step) % 12) as usize] = true;
        }
        set
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyEstimate {
    pub key: Key,
    /// Share of the softmax over all 24 keys, 0–1.
    pub confidence: f32,
    pub alt: Option<Key>,
}

fn pearson(a: &[f32; 12], b: &[f32; 12]) -> f32 {
    let mean = |x: &[f32; 12]| x.iter().sum::<f32>() / 12.0;
    let (ma, mb) = (mean(a), mean(b));
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for i in 0..12 {
        let (x, y) = (a[i] - ma, b[i] - mb);
        num += x * y;
        da += x * x;
        db += y * y;
    }
    if da <= 0.0 || db <= 0.0 { 0.0 } else { num / (da * db).sqrt() }
}

/// The likeliest key for a pitch-class profile, or `None` if the profile is flat, empty, or has
/// too few pitch classes.
pub fn estimate(profile: &[f32; 12]) -> Option<KeyEstimate> {
    let total: f32 = profile.iter().sum();
    let max = profile.iter().copied().fold(0.0, f32::max);
    let present = profile.iter().filter(|w| **w >= PRESENT * max).count();
    if total.is_nan() || total <= 0.0 || max * 12.0 <= total * 1.0001 || present < MIN_PITCH_CLASSES {
        return None;
    }
    let mut scored: Vec<(Key, f32)> = Vec::with_capacity(24);
    for minor in [false, true] {
        let base = if minor { &MINOR_PROFILE } else { &MAJOR_PROFILE };
        for tonic in 0..12u8 {
            let mut rotated = [0.0; 12];
            for (i, value) in base.iter().enumerate() {
                rotated[(i + tonic as usize) % 12] = *value;
            }
            scored.push((Key { tonic, minor }, pearson(profile, &rotated)));
        }
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top = scored[0].1;
    let weights: Vec<f32> = scored.iter().map(|(_, r)| ((r - top) / TEMPERATURE).exp()).collect();
    let sum: f32 = weights.iter().sum();
    let alt = (weights[1] / sum >= ALT_MIN).then_some(scored[1].0);
    Some(KeyEstimate { key: scored[0].0, confidence: weights[0] / sum, alt })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(pcs: &[(usize, f32)]) -> [f32; 12] {
        let mut p = [0.0; 12];
        for &(pc, w) in pcs {
            p[pc] += w;
        }
        p
    }

    #[test]
    fn a_cadence_in_g_major() {
        // G B D, C E G, D F# A, G B D.
        let p = profile(&[(7, 3.0), (11, 2.0), (2, 2.0), (0, 1.0), (4, 1.0), (6, 1.0), (9, 1.0)]);
        let estimate = estimate(&p).unwrap();
        assert_eq!(estimate.key, Key { tonic: 7, minor: false });
        assert_eq!(estimate.key.name(), "G major");
    }

    #[test]
    fn harmonic_minor_with_its_leading_tone() {
        // A minor: A C E, D F A, E G# B, A C E.
        let p = profile(&[(9, 3.0), (0, 2.0), (4, 3.0), (2, 1.0), (5, 1.0), (8, 1.0), (11, 1.0)]);
        let estimate = estimate(&p).unwrap();
        assert_eq!(estimate.key, Key { tonic: 9, minor: true });
        assert!(estimate.confidence > 0.5, "{estimate:?}");
    }

    #[test]
    fn flat_or_empty_profiles_have_no_key() {
        assert_eq!(estimate(&[0.0; 12]), None);
        assert_eq!(estimate(&[1.0; 12]), None);
        assert_eq!(estimate(&profile(&[(3, 1.0), (10, 0.3)])), None);
    }

    #[test]
    fn names_use_the_keys_spelling() {
        assert_eq!(Key { tonic: 10, minor: false }.name(), "Bb major");
        assert_eq!(Key { tonic: 6, minor: true }.name(), "F# minor");
        assert_eq!(Key { tonic: 3, minor: true }.name(), "D# minor");
    }
}
