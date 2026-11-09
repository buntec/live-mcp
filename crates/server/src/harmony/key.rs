//! Key estimation: correlating a pitch-class profile with Temperley's key profiles.

use super::chords::{Chord, QUALITIES};
use super::names::spelling;

/// Temperley's (2007) profiles from the Kostka–Payne corpus, tonic first. They penalise notes
/// outside the scale more than Krumhansl & Kessler's probe-tone ratings, which matters for
/// short loops: one F natural should rule out G major.
const MAJOR_PROFILE: [f32; 12] = [0.748, 0.060, 0.488, 0.082, 0.670, 0.460, 0.096, 0.715, 0.104, 0.366, 0.057, 0.400];
const MINOR_PROFILE: [f32; 12] = [0.712, 0.084, 0.474, 0.618, 0.049, 0.460, 0.105, 0.747, 0.404, 0.067, 0.133, 0.330];

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
#[cfg(test)]
pub fn estimate(profile: &[f32; 12]) -> Option<KeyEstimate> {
    estimate_with_tonic_hint(profile, None)
}

/// Weight of [`harmony_fit`] in a key's score.
pub(crate) const HARMONY_WEIGHT: f32 = 0.7;

/// A chord's triad: major, minor or diminished, or neither (sus and power chords, augmented).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Triad {
    Major,
    Minor,
    Diminished,
    Open,
}

fn triad(chord: &Chord) -> Triad {
    let has = |i: u8| QUALITIES[chord.quality].intervals.contains(&i);
    match (has(3), has(4), has(6), has(7)) {
        (false, true, _, true) => Triad::Major,
        (true, false, _, true) => Triad::Minor,
        (true, false, true, false) => Triad::Diminished,
        _ => Triad::Open,
    }
}

/// What a chord does in a key: (root's interval above the tonic, triad, weight). The tonic counts
/// most, then the dominant and the subdominant; minor keys take the major V of harmonic minor and
/// the natural minor's v, VI, III and VII.
const MAJOR_FUNCTIONS: &[(u8, Triad, f32)] = &[
    (0, Triad::Major, 1.0),
    (7, Triad::Major, 0.7),
    (5, Triad::Major, 0.6),
    (2, Triad::Minor, 0.4),
    (9, Triad::Minor, 0.4),
    (4, Triad::Minor, 0.3),
    (11, Triad::Diminished, 0.3),
];
const MINOR_FUNCTIONS: &[(u8, Triad, f32)] = &[
    (0, Triad::Minor, 1.0),
    (7, Triad::Major, 0.7),
    (5, Triad::Minor, 0.6),
    (8, Triad::Major, 0.4),
    (3, Triad::Major, 0.3),
    (10, Triad::Major, 0.3),
    (7, Triad::Minor, 0.3),
    (2, Triad::Diminished, 0.3),
    (11, Triad::Diminished, 0.3),
];
/// A chord that is none of the key's.
const OUT_OF_KEY: f32 = -0.4;

/// How well a sequence of chords (one per beat) serves `key` as its harmony: the mean of each
/// chord's weight by function. Pitch counts alone cannot tell C major from F major in C G Am F (it
/// is I V vi IV in C, but V II iii I in F, with a G major chord F major does not have), nor a minor
/// key from its relative major when the leading tone is brief.
pub(crate) fn harmony_fit(key: Key, chords: &[Chord]) -> f32 {
    if chords.is_empty() {
        return 0.0;
    }
    let functions = if key.minor { MINOR_FUNCTIONS } else { MAJOR_FUNCTIONS };
    let total: f32 = chords
        .iter()
        .map(|c| {
            let degree = (c.root + 12 - key.tonic) % 12;
            let weight = |t: Triad| functions.iter().find(|f| f.0 == degree && f.1 == t).map(|f| f.2);
            match triad(c) {
                Triad::Open => {
                    let best = [Triad::Major, Triad::Minor]
                        .into_iter()
                        .filter_map(weight)
                        .fold(None, |m: Option<f32>, w| Some(m.map_or(w, |m| m.max(w))));
                    best.map_or(OUT_OF_KEY / 2.0, |w| w / 2.0)
                }
                t => weight(t).unwrap_or(OUT_OF_KEY),
            }
        })
        .sum();
    total / chords.len() as f32
}

/// All 24 keys with their scores, best first, or `None` if the profile is flat, empty, or has too
/// few pitch classes. `chords` (one per beat) add their [`harmony_fit`].
pub(crate) fn ranking(
    profile: &[f32; 12],
    tonic_hint: Option<&[f32; 12]>,
    chords: &[Chord],
) -> Option<Vec<(Key, f32)>> {
    ranking_weighted(profile, tonic_hint, chords, HARMONY_WEIGHT)
}

pub(crate) fn ranking_weighted(
    profile: &[f32; 12],
    tonic_hint: Option<&[f32; 12]>,
    chords: &[Chord],
    harmony_weight: f32,
) -> Option<Vec<(Key, f32)>> {
    if !enough_pitch_classes(profile) {
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
            let key = Key { tonic, minor };
            let hint = tonic_hint.map_or(0.0, |h| h[tonic as usize]);
            let harmony = if harmony_weight > 0.0 { harmony_weight * harmony_fit(key, chords) } else { 0.0 };
            scored.push((key, pearson(profile, &rotated) + TONIC_HINT_WEIGHT * hint + harmony));
        }
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    Some(scored)
}

/// Whether a profile can name a key: not empty, not flat, with enough pitch classes present.
pub(crate) fn enough_pitch_classes(profile: &[f32; 12]) -> bool {
    let total: f32 = profile.iter().sum();
    let max = profile.iter().copied().fold(0.0, f32::max);
    let present = profile.iter().filter(|w| **w >= PRESENT * max).count();
    !(total.is_nan() || total <= 0.0 || max * 12.0 <= total * 1.0001 || present < MIN_PITCH_CLASSES)
}

/// Added to a key's correlation in proportion to the hint's share on its tonic.
const TONIC_HINT_WEIGHT: f32 = 0.12;

/// Like [`estimate`], with evidence for the tonic beyond pitch counts: `tonic_hint` is the share
/// of each pitch class among the bass notes on bars' downbeats. Correlation alone cannot tell a D
/// minor bass line without an F from G minor; the D on every downbeat can.
pub fn estimate_with_tonic_hint(profile: &[f32; 12], tonic_hint: Option<&[f32; 12]>) -> Option<KeyEstimate> {
    estimate_with_harmony(profile, tonic_hint, &[])
}

/// Like [`estimate_with_tonic_hint`], with the chords heard (one per beat) as evidence too.
pub fn estimate_with_harmony(
    profile: &[f32; 12],
    tonic_hint: Option<&[f32; 12]>,
    chords: &[Chord],
) -> Option<KeyEstimate> {
    let scored = ranking(profile, tonic_hint, chords)?;
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

    fn chord(root: u8, quality: usize) -> Chord {
        Chord { root, quality, bass: None }
    }

    #[test]
    fn chords_tell_a_key_from_its_subdominant() {
        // C G Am F: I V vi IV in C major; in F major V, a G major F does not have, iii and I.
        let pop = [chord(0, 0), chord(7, 0), chord(9, 1), chord(5, 0)];
        let (c, f) = (Key { tonic: 0, minor: false }, Key { tonic: 5, minor: false });
        assert!(harmony_fit(c, &pop) > harmony_fit(f, &pop) + 0.2, "0.675 against 0.4");
        // Gm Eb F D: i VI VII V in G minor; the D major is out of Bb major.
        let minor = [chord(7, 1), chord(3, 0), chord(5, 0), chord(2, 0)];
        let (gm, bb) = (Key { tonic: 7, minor: true }, Key { tonic: 10, minor: false });
        assert!(harmony_fit(gm, &minor) > harmony_fit(bb, &minor));
        // A sus chord counts half, by its root; no chords, no evidence.
        assert_eq!(harmony_fit(c, &[chord(0, 8)]), 0.5);
        assert_eq!(harmony_fit(c, &[]), 0.0);
    }

    #[test]
    fn names_use_the_keys_spelling() {
        assert_eq!(Key { tonic: 10, minor: false }.name(), "Bb major");
        assert_eq!(Key { tonic: 6, minor: true }.name(), "F# minor");
        assert_eq!(Key { tonic: 3, minor: true }.name(), "D# minor");
    }
}
