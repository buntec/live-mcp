//! Several tracks heard together: their combined sound, semitone clashes between them, and
//! octaves that several tracks crowd.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::chords::Chord;
use super::names::Spelling;
use super::{BarSpan, Beat};

/// Pitch classes at least this share of a track's strongest in the beat count as sounding.
const STRONG: f32 = 0.5;
/// A track this far below the beat's loudest counted track is too quiet to clash.
const CLASH_RANGE_DB: f32 = 20.0;
/// Two tracks share an octave when each has at least this share of its strongest octave there.
const SHARED_OCTAVE: f32 = 0.25;
/// A track carries an octave with at least this share of the octave's energy (−6 dB).
const CARRY_SHARE: f32 = 0.25;
/// This many tracks carrying one octave crowd it.
const CROWD: usize = 3;
const MAX_CLASHES: usize = 12;

/// One counted track's beats, with the gain from its Ear to the mix.
pub struct Voice<'a> {
    pub name: String,
    pub gain: f32,
    pub beats: &'a [Beat],
}

fn shape<const N: usize>(values: &[f32; N]) -> [f32; N] {
    let max = values.iter().copied().fold(0.0, f32::max);
    if max > 0.0 { values.map(|v| v / max) } else { [0.0; N] }
}

/// The voices summed beat by beat, as if one Ear heard only them.
pub fn combined(voices: &[Voice]) -> Vec<Beat> {
    let mut by_index: BTreeMap<i64, Beat> = BTreeMap::new();
    for voice in voices {
        for beat in voice.beats {
            let sum = by_index.entry(beat.index).or_insert_with(|| Beat { index: beat.index, ..Beat::default() });
            let w = beat.weight * voice.gain;
            sum.coverage = sum.coverage.max(beat.coverage);
            if beat.pitched {
                sum.pitched = true;
                sum.weight += w;
                for (s, v) in sum.chroma.iter_mut().zip(shape(&beat.chroma)) {
                    *s += v * w;
                }
                for (s, v) in sum.bass.iter_mut().zip(shape(&beat.bass)) {
                    *s += v * w;
                }
            }
            for (s, v) in sum.octaves.iter_mut().zip(shape(&beat.octaves)) {
                *s += v * w * w;
            }
            for (s, v) in sum.onsets.iter_mut().zip(&beat.onsets) {
                *s = s.max(*v);
            }
            // The lowest confident note any voice plays is the bass line.
            for (slot, note) in sum.bass_line.iter_mut().zip(&beat.bass_line) {
                if let Some(n) = note
                    && slot.is_none_or(|s| n.pitch < s.pitch)
                {
                    *slot = Some(*n);
                }
            }
            let tonal = beat.tonal.unwrap_or(0.0);
            sum.tonal = Some(sum.tonal.map_or(tonal, |t| t.max(tonal)));
        }
    }
    by_index.into_values().collect()
}

fn bar_of(bars: &[BarSpan], beat: i64) -> Option<&BarSpan> {
    bars.iter().find(|b| b.start_beat <= beat as f64 + 1e-9 && (beat as f64) < b.start_beat + b.beats - 1e-9)
}

/// "5" or "5–6".
fn bar_range(bars: &BTreeSet<i64>) -> String {
    let mut runs: Vec<(i64, i64)> = Vec::new();
    for &b in bars {
        match runs.last_mut() {
            Some((_, end)) if *end + 1 == b => *end = b,
            _ => runs.push((b, b)),
        }
    }
    runs.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}–{b}") }).collect::<Vec<_>>().join(", ")
}

/// Semitone clashes between voices, beat by beat: a pitch class sounding in one voice and the one
/// a semitone away in another, both loud enough, in a shared octave, and not both in the chord the
/// voices make together. Tritones are left alone (they belong in dominant chords).
pub fn clashes(
    voices: &[Voice],
    chords_by_beat: &BTreeMap<i64, Option<Chord>>,
    bars: &[BarSpan],
    spelling: Spelling,
) -> Value {
    // (voice a, voice b, pc in a, pc in b) → beats.
    let mut found: BTreeMap<(usize, usize, u8, u8), BTreeSet<i64>> = BTreeMap::new();
    let lookup: Vec<BTreeMap<i64, &Beat>> =
        voices.iter().map(|v| v.beats.iter().filter(|b| b.pitched).map(|b| (b.index, b)).collect()).collect();
    for (&index, chord) in chords_by_beat {
        let loudest = voices
            .iter()
            .zip(&lookup)
            .filter_map(|(v, l)| l.get(&index).map(|b| b.weight * v.gain))
            .fold(0.0f32, f32::max);
        if loudest <= 0.0 {
            continue;
        }
        let floor = loudest * 10f32.powf(-CLASH_RANGE_DB / 20.0);
        let tones: Vec<u8> = chord.map(|c| c.tones().collect()).unwrap_or_default();
        for a in 0..voices.len() {
            for b in a + 1..voices.len() {
                let (Some(x), Some(y)) = (lookup[a].get(&index), lookup[b].get(&index)) else { continue };
                if x.weight * voices[a].gain < floor || y.weight * voices[b].gain < floor {
                    continue;
                }
                let (ox, oy) = (shape(&x.octaves), shape(&y.octaves));
                if !(0..ox.len()).any(|o| ox[o] >= SHARED_OCTAVE && oy[o] >= SHARED_OCTAVE) {
                    continue;
                }
                let (cx, cy) = (shape(&x.chroma), shape(&y.chroma));
                for p in 0..12u8 {
                    if cx[p as usize] < STRONG {
                        continue;
                    }
                    for q in [(p + 1) % 12, (p + 11) % 12] {
                        let explained = tones.contains(&p) && tones.contains(&q);
                        if cy[q as usize] >= STRONG && !explained && cy[p as usize] < STRONG && cx[q as usize] < STRONG
                        {
                            found.entry((a, b, p, q)).or_default().insert(index);
                        }
                    }
                }
            }
        }
    }
    let mut list: Vec<(usize, Value)> = found
        .into_iter()
        .map(|((a, b, p, q), beats)| {
            let bar_numbers: BTreeSet<i64> = beats.iter().filter_map(|&i| bar_of(bars, i).map(|b| b.bar)).collect();
            let entry = json!({
                "tracks": [voices[a].name, voices[b].name],
                "notes": format!("{} against {}", spelling[p as usize], spelling[q as usize]),
                "bars": bar_range(&bar_numbers),
                "beats": beats.len(),
            });
            (beats.len(), entry)
        })
        .collect();
    list.sort_by_key(|x| std::cmp::Reverse(x.0));
    Value::Array(list.into_iter().take(MAX_CLASHES).map(|(_, v)| v).collect())
}

/// Octaves (per bar) carried by three or more voices.
pub fn crowded(voices: &[Voice], bars: &[BarSpan]) -> Value {
    // (octave, carriers) → bars.
    let mut found: BTreeMap<(usize, Vec<usize>), BTreeSet<i64>> = BTreeMap::new();
    for bar in bars {
        let energy: Vec<[f32; 7]> = voices
            .iter()
            .map(|v| {
                let mut e = [0.0f32; 7];
                for beat in v.beats.iter().filter(|b| bar_of(std::slice::from_ref(bar), b.index).is_some()) {
                    let total: f32 = beat.octaves.iter().sum();
                    if total > 0.0 {
                        let level = beat.weight * v.gain;
                        for (s, o) in e.iter_mut().zip(&beat.octaves) {
                            *s += o / total * level * level;
                        }
                    }
                }
                e
            })
            .collect();
        for octave in 0..7 {
            let total: f32 = energy.iter().map(|e| e[octave]).sum();
            if total <= 0.0 {
                continue;
            }
            let carriers: Vec<usize> =
                (0..voices.len()).filter(|&k| energy[k][octave] >= CARRY_SHARE * total).collect();
            if carriers.len() >= CROWD {
                found.entry((octave, carriers)).or_default().insert(bar.bar);
            }
        }
    }
    let list: Vec<Value> = found
        .into_iter()
        .map(|((octave, carriers), bars)| {
            json!({
                "octave": format!("C{octave}–B{octave}"),
                "tracks": carriers.iter().map(|&k| voices[k].name.clone()).collect::<Vec<_>>(),
                "bars": bar_range(&bars),
            })
        })
        .collect();
    Value::Array(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harmony::names::spelling;

    fn beat(index: i64, pcs: &[usize], octave: usize) -> Beat {
        let mut b = Beat { index, coverage: 1.0, weight: 0.5, pitched: true, tonal: Some(0.9), ..Beat::default() };
        for &pc in pcs {
            b.chroma[pc] = 1.0;
            b.bass[pc] = 1.0;
        }
        b.octaves[octave] = 1.0;
        b
    }

    #[test]
    fn a_semitone_rub_in_a_shared_octave_is_a_clash() {
        let bars = [BarSpan { bar: 1, start_beat: 0.0, beats: 4.0 }];
        let pad: Vec<Beat> = (0..4).map(|i| beat(i, &[5, 9, 0], 2)).collect(); // F A C
        let bass: Vec<Beat> = (0..4).map(|i| beat(i, &[4], 2)).collect(); // E
        let far: Vec<Beat> = (0..4).map(|i| beat(i, &[4], 5)).collect(); // E, three octaves up
        let voices = [
            Voice { name: "Pad".into(), gain: 1.0, beats: &pad },
            Voice { name: "Bass".into(), gain: 1.0, beats: &bass },
            Voice { name: "Lead".into(), gain: 1.0, beats: &far },
        ];
        let chords: BTreeMap<i64, Option<Chord>> = (0..4).map(|i| (i, None)).collect();
        let found = clashes(&voices, &chords, &bars, spelling(None));
        assert_eq!(found, json!([{"tracks": ["Pad", "Bass"], "notes": "F against E", "bars": "1", "beats": 4}]));
        // Inside a chord that has both notes (Fmaj7 has E and F), it is not a clash.
        let fmaj7 = Chord { root: 5, quality: 3, bass: None };
        let chords: BTreeMap<i64, Option<Chord>> = (0..4).map(|i| (i, Some(fmaj7))).collect();
        assert_eq!(clashes(&voices, &chords, &bars, spelling(None)), json!([]));
    }

    #[test]
    fn three_tracks_in_one_octave_crowd_it() {
        let bars = [BarSpan { bar: 1, start_beat: 0.0, beats: 4.0 }, BarSpan { bar: 2, start_beat: 4.0, beats: 4.0 }];
        let parts: Vec<Vec<Beat>> = (0..3).map(|_| (0..8).map(|i| beat(i, &[0], 3)).collect()).collect();
        let voices: Vec<Voice> =
            parts.iter().enumerate().map(|(k, beats)| Voice { name: format!("T{k}"), gain: 1.0, beats }).collect();
        assert_eq!(crowded(&voices, &bars), json!([{"octave": "C3–B3", "tracks": ["T0", "T1", "T2"], "bars": "1–2"}]));
    }
}
