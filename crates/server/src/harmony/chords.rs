//! Chord recognition: templates scored against each beat's chroma, smoothed over beats with
//! Viterbi so that labels change only when the music clearly does.

use super::key::Key;
use super::names::Spelling;
use super::{Beat, Source};

pub struct Quality {
    pub suffix: &'static str,
    pub intervals: &'static [u8],
}

pub const QUALITIES: &[Quality] = &[
    Quality { suffix: "", intervals: &[0, 4, 7] },
    Quality { suffix: "m", intervals: &[0, 3, 7] },
    Quality { suffix: "7", intervals: &[0, 4, 7, 10] },
    Quality { suffix: "maj7", intervals: &[0, 4, 7, 11] },
    Quality { suffix: "m7", intervals: &[0, 3, 7, 10] },
    Quality { suffix: "dim", intervals: &[0, 3, 6] },
    Quality { suffix: "m7b5", intervals: &[0, 3, 6, 10] },
    Quality { suffix: "aug", intervals: &[0, 4, 8] },
    Quality { suffix: "sus4", intervals: &[0, 5, 7] },
    Quality { suffix: "sus2", intervals: &[0, 2, 7] },
    Quality { suffix: "5", intervals: &[0, 7] },
];

/// Chord states; one more state (`NO_CHORD`) stands for silence, noise or a single note.
const CHORDS: usize = 12 * QUALITIES.len();
const NO_CHORD: usize = CHORDS;
const STATES: usize = CHORDS + 1;

/// Below this template match, a beat is not called a chord.
const NO_CHORD_SCORE: f32 = 0.55;
/// Added when the strongest bass pitch class is the chord's root.
const BASS_BONUS: f32 = 0.06;
/// Added for chords whose notes are all in the key. Small, so borrowed chords still win.
const KEY_BONUS: f32 = 0.03;
/// Softmax temperature for per-beat confidence.
const TEMPERATURE: f32 = 0.05;
/// Other chords whose mean score is this close to the chosen one are listed as alternatives.
const ALT_MARGIN: f32 = 0.05;
/// Harmonic h of a note weighs DECAY^(h−1) in audio templates.
const HARMONIC_DECAY: f32 = 0.6;
/// In audio a root's harmonics look much like a major triad, so a single note must beat the
/// chords by this much before a beat is called no chord.
const SINGLE_NOTE_MARGIN_AUDIO: f32 = 0.08;
/// Likewise a power chord (root and fifth) in audio is close to a triad over a strong root; it
/// must win by this much.
const POWER_CHORD_MARGIN_AUDIO: f32 = 0.03;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub root: u8,
    /// Index into [`QUALITIES`].
    pub quality: usize,
    /// A chord tone other than the root in the bass (an inversion).
    pub bass: Option<u8>,
}

impl Chord {
    fn from_state(state: usize) -> Option<Self> {
        (state < CHORDS).then_some(Chord { root: (state % 12) as u8, quality: state / 12, bass: None })
    }

    pub fn tones(&self) -> impl Iterator<Item = u8> + '_ {
        QUALITIES[self.quality].intervals.iter().map(|i| (self.root + i) % 12)
    }

    pub fn name(&self, spelling: Spelling) -> String {
        let mut name = format!("{}{}", spelling[self.root as usize], QUALITIES[self.quality].suffix);
        if let Some(bass) = self.bass {
            name.push('/');
            name.push_str(spelling[bass as usize]);
        }
        name
    }
}

/// Chord names, with "N" for no chord.
pub fn label(chord: Option<Chord>, spelling: Spelling) -> String {
    chord.map_or_else(|| "N".to_owned(), |c| c.name(spelling))
}

/// Pitch-class weights of a set of notes, normalised to unit length. Audio templates include
/// each note's first six harmonics, as heard in a real spectrum.
fn template(root: u8, intervals: &[u8], source: Source) -> [f32; 12] {
    // Pitch class offsets of harmonics 1–6: unison, octave, fifth, octave, major third, fifth.
    const HARMONICS: [u8; 6] = [0, 0, 7, 0, 4, 7];
    let mut t = [0.0f32; 12];
    for interval in intervals {
        let pc = (root + interval) % 12;
        match source {
            Source::Midi => t[pc as usize] = 1.0,
            Source::Audio => {
                for (h, offset) in HARMONICS.iter().enumerate() {
                    t[((pc + offset) % 12) as usize] += HARMONIC_DECAY.powi(h as i32);
                }
            }
        }
    }
    let norm = t.iter().map(|x| x * x).sum::<f32>().sqrt();
    t.map(|x| x / norm)
}

fn cosine(a: &[f32; 12], unit: &[f32; 12]) -> f32 {
    let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= 0.0 { 0.0 } else { a.iter().zip(unit).map(|(x, y)| x * y).sum::<f32>() / norm }
}

fn argmax(values: &[f32; 12]) -> Option<u8> {
    let (i, max) = values.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    (*max > 0.0).then_some(i as u8)
}

/// Each beat's best chord after smoothing, with its confidence and the scores behind it.
pub struct Labels {
    states: Vec<usize>,
    pub confidence: Vec<f32>,
    scores: Vec<[f32; STATES]>,
}

impl Labels {
    pub fn chord(&self, beat: usize) -> Option<Chord> {
        Chord::from_state(self.states[beat])
    }

    pub fn same(&self, a: usize, b: usize) -> bool {
        self.states[a] == self.states[b]
    }
}

/// Labels every beat. `change_cost[t]` is the price of a new chord at beat t (lower on strong
/// beats); `key` slightly favours chords in the key.
pub fn label_beats(beats: &[Beat], change_cost: &[f32], key: Option<Key>, source: Source) -> Labels {
    let templates: Vec<[f32; 12]> =
        (0..CHORDS).map(|s| template((s % 12) as u8, QUALITIES[s / 12].intervals, source)).collect();
    let notes: Vec<[f32; 12]> = (0..12).map(|pc| template(pc, &[0], source)).collect();
    let in_key: Vec<bool> = (0..CHORDS)
        .map(|s| {
            let scale = key.map(Key::scale);
            let chord = Chord::from_state(s).expect("a chord state");
            scale.is_some_and(|scale| chord.tones().all(|pc| scale[pc as usize]))
        })
        .collect();

    let scores: Vec<[f32; STATES]> = beats
        .iter()
        .map(|beat| {
            let mut e = [0.0f32; STATES];
            if !beat.pitched {
                e[NO_CHORD] = 1.0;
                return e;
            }
            let bass = argmax(&beat.bass);
            for s in 0..CHORDS {
                e[s] = cosine(&beat.chroma, &templates[s]);
                if bass == Some((s % 12) as u8) {
                    e[s] += BASS_BONUS;
                }
                if in_key[s] {
                    e[s] += KEY_BONUS;
                }
                if source == Source::Audio && QUALITIES[s / 12].intervals.len() == 2 {
                    e[s] -= POWER_CHORD_MARGIN_AUDIO;
                }
            }
            let single = notes.iter().map(|t| cosine(&beat.chroma, t)).fold(0.0, f32::max);
            let margin = if source == Source::Audio { SINGLE_NOTE_MARGIN_AUDIO } else { 0.0 };
            e[NO_CHORD] = NO_CHORD_SCORE.max(single + BASS_BONUS - margin);
            e
        })
        .collect();

    // Viterbi. A change costs the same whatever it changes to, so the best predecessor is either
    // the same state or the overall best one, and each step is O(states).
    let n = beats.len();
    let mut states = vec![NO_CHORD; n];
    if n > 0 {
        let mut delta = scores[0];
        let mut back = vec![[0u16; STATES]; n];
        for t in 1..n {
            let consecutive = beats[t].index == beats[t - 1].index + 1;
            let cost = if consecutive { change_cost[t] } else { 0.0 };
            let best = (0..STATES).max_by(|&a, &b| delta[a].total_cmp(&delta[b])).expect("states");
            let mut next = [0.0f32; STATES];
            for s in 0..STATES {
                let (from, value) =
                    if delta[s] >= delta[best] - cost { (s, delta[s]) } else { (best, delta[best] - cost) };
                back[t][s] = from as u16;
                next[s] = value + scores[t][s];
            }
            delta = next;
        }
        let mut s = (0..STATES).max_by(|&a, &b| delta[a].total_cmp(&delta[b])).expect("states");
        for t in (0..n).rev() {
            states[t] = s;
            s = back[t][s] as usize;
        }
    }

    let confidence = states
        .iter()
        .zip(&scores)
        .map(|(&s, e)| {
            let top = e.iter().copied().fold(f32::MIN, f32::max);
            let sum: f32 = e.iter().map(|x| ((x - top) / TEMPERATURE).exp()).sum();
            ((e[s] - top) / TEMPERATURE).exp() / sum
        })
        .collect();
    Labels { states, confidence, scores }
}

/// A run of beats with one chord.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Position of the first beat in the input slice.
    pub first: usize,
    pub beats: usize,
    pub chord: Option<Chord>,
    pub confidence: f32,
    pub alts: Vec<Chord>,
}

/// Splits `range` (positions in the beat slice) into runs of the same chord, with inversions
/// and alternatives worked out per run.
pub fn segments(beats: &[Beat], labels: &Labels, range: std::ops::Range<usize>) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let mut end = start + 1;
        while end < range.end && labels.same(start, end) && beats[end].index == beats[end - 1].index + 1 {
            end += 1;
        }
        let state = labels.states[start];
        let count = (end - start) as f32;
        let mut chord = Chord::from_state(state);
        if let Some(chord) = chord.as_mut() {
            let mut bass = [0.0f32; 12];
            for beat in &beats[start..end] {
                for (sum, b) in bass.iter_mut().zip(&beat.bass) {
                    *sum += b;
                }
            }
            chord.bass = argmax(&bass).filter(|&b| b != chord.root && chord.tones().any(|pc| pc == b));
        }
        let mean = |s: usize| labels.scores[start..end].iter().map(|e| e[s]).sum::<f32>() / count;
        let alts = if chord.is_some() {
            let chosen = mean(state);
            let mut close: Vec<(usize, f32)> = (0..CHORDS)
                .filter(|&s| s != state)
                .map(|s| (s, mean(s)))
                .filter(|&(_, m)| m >= chosen - ALT_MARGIN)
                .collect();
            close.sort_by(|a, b| b.1.total_cmp(&a.1));
            close.into_iter().take(2).filter_map(|(s, _)| Chord::from_state(s)).collect()
        } else {
            Vec::new()
        };
        let confidence = labels.confidence[start..end].iter().sum::<f32>() / count;
        out.push(Segment { first: start, beats: end - start, chord, confidence, alts });
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harmony::names::spelling;

    fn beat(index: i64, pitches: &[u8]) -> Beat {
        let mut b = Beat { index, pitched: !pitches.is_empty(), weight: 1.0, ..Beat::default() };
        for &p in pitches {
            b.chroma[(p % 12) as usize] += 1.0;
        }
        if let Some(&low) = pitches.iter().min() {
            b.bass[(low % 12) as usize] = 1.0;
        }
        b
    }

    fn names(beats: &[Beat], costs: &[f32]) -> Vec<String> {
        let labels = label_beats(beats, costs, None, Source::Midi);
        segments(beats, &labels, 0..beats.len())
            .iter()
            .map(|s| format!("{}x{}", label(s.chord, spelling(None)), s.beats))
            .collect()
    }

    #[test]
    fn triads_sevenths_and_inversions() {
        let beats = [
            beat(0, &[57, 60, 64, 67]), // A C E G: Am7
            beat(1, &[57, 60, 64, 67]),
            beat(2, &[52, 55, 60]), // E G C: C/E
            beat(3, &[52, 55, 60]),
            beat(4, &[]),   // silence
            beat(5, &[62]), // a single note is not a chord
        ];
        assert_eq!(names(&beats, &[0.1; 6]), ["Am7x2", "C/Ex2", "Nx2"]);
    }

    #[test]
    fn a_weak_wobble_does_not_change_the_chord() {
        // C major, then a beat where the fifth drops out (C E: still C, or Am at a stretch).
        let beats = [beat(0, &[48, 52, 55]), beat(1, &[48, 52, 55]), beat(2, &[48, 52]), beat(3, &[48, 52, 55])];
        assert_eq!(names(&beats, &[0.16; 4]), ["Cx4"]);
    }

    #[test]
    fn audio_templates_absorb_harmonics() {
        // A single note's spectrum: strong root, weaker fifth and third from its harmonics.
        let mut note = beat(0, &[]);
        note.pitched = true;
        note.chroma[9] = 1.0;
        note.chroma[4] = 0.4;
        note.chroma[1] = 0.15;
        note.bass[9] = 1.0;
        let labels = label_beats(&[note], &[0.0], None, Source::Audio);
        assert_eq!(labels.chord(0), None);
    }
}
