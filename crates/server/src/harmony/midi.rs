//! Beats from MIDI notes: exact input for the interpreter.

use super::{BassNote, Beat};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiNote {
    pub pitch: u8,
    pub start: f64,
    pub duration: f64,
    /// 1–127.
    pub velocity: f64,
}

/// Points per beat at which the lowest sounding note is sampled for the bass.
const BASS_SAMPLES: usize = 8;

/// One beat per quarter note that overlaps `[from, to)`, with chroma and octaves weighted by
/// velocity × sounding time, the bass taken from the lowest sounding note, and onsets from note
/// starts.
pub fn beats(notes: &[MidiNote], from: f64, to: f64) -> Vec<Beat> {
    if from.is_nan() || to.is_nan() || to <= from {
        return Vec::new();
    }
    let first = from.floor() as i64;
    let last = (to.ceil() as i64).max(first + 1);
    (first..last)
        .filter_map(|index| {
            let lo = (index as f64).max(from);
            let hi = ((index + 1) as f64).min(to);
            (hi > lo).then(|| beat(notes, index, lo, hi))
        })
        .collect()
}

fn beat(notes: &[MidiNote], index: i64, lo: f64, hi: f64) -> Beat {
    let mut beat = Beat { index, coverage: (hi - lo) as f32, ..Beat::default() };
    let mut span: Option<(u8, u8)> = None;
    for note in notes {
        let end = note.start + note.duration;
        let overlap = end.min(hi) - note.start.max(lo);
        let level = (note.velocity / 127.0) as f32;
        if overlap > 0.0 {
            let w = overlap as f32 * level;
            beat.chroma[(note.pitch % 12) as usize] += w;
            let octave = (note.pitch as i32 / 12 - 2).clamp(0, 6) as usize;
            beat.octaves[octave] += w;
            span = Some(span.map_or((note.pitch, note.pitch), |(l, h)| (l.min(note.pitch), h.max(note.pitch))));
        }
        if note.start >= lo && note.start < hi {
            let slot = (((note.start - index as f64) * 4.0) as usize).min(3);
            beat.onsets[slot] += level;
        }
    }
    for k in 0..BASS_SAMPLES {
        let t = lo + (k as f64 + 0.5) * (hi - lo) / BASS_SAMPLES as f64;
        let lowest = notes.iter().filter(|n| n.start <= t && t < n.start + n.duration).min_by_key(|n| n.pitch);
        if let Some(n) = lowest {
            beat.bass[(n.pitch % 12) as usize] += (n.velocity / 127.0) as f32 / BASS_SAMPLES as f32;
        }
    }
    // The bass line: the lowest note sounding just after each 16th starts.
    for s in 0..4 {
        let t = index as f64 + s as f64 / 4.0 + 0.01;
        if t < lo || t >= hi {
            continue;
        }
        let lowest = notes.iter().filter(|n| n.start <= t && t < n.start + n.duration).min_by_key(|n| n.pitch);
        beat.bass_line[s] = lowest.map(|n| BassNote { pitch: n.pitch, cents: 0.0, confidence: 1.0 });
    }
    beat.weight = beat.chroma.iter().sum();
    beat.pitched = beat.weight > 0.0;
    beat.span = span;
    beat
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pitch: u8, start: f64, duration: f64) -> MidiNote {
        MidiNote { pitch, start, duration, velocity: 127.0 }
    }

    #[test]
    fn notes_spread_over_the_beats_they_sound_in() {
        let notes = [note(48, 0.0, 2.0), note(64, 0.5, 0.25), note(67, 1.75, 1.0)];
        let beats = beats(&notes, 0.0, 3.0);
        assert_eq!(beats.len(), 3);
        assert_eq!(beats[0].chroma[0], 1.0);
        assert_eq!(beats[0].chroma[4], 0.25);
        assert_eq!(beats[0].onsets, [1.0, 0.0, 1.0, 0.0]);
        assert_eq!(beats[1].onsets, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(beats[1].span, Some((48, 67)));
        assert_eq!(beats[2].bass[7], 0.75); // only the G sounds in beat 3, for 3/4 of it
        assert_eq!(beats[2].octaves[3], 0.75);
        let line: Vec<Option<u8>> = beats[1].bass_line.iter().map(|n| n.map(|n| n.pitch)).collect();
        assert_eq!(line, [Some(48), Some(48), Some(48), Some(48)]);
        let line: Vec<Option<u8>> = beats[2].bass_line.iter().map(|n| n.map(|n| n.pitch)).collect();
        assert_eq!(line, [Some(67), Some(67), Some(67), None]);
    }

    #[test]
    fn the_region_cuts_beats() {
        let beats = beats(&[note(60, 0.0, 8.0)], 1.5, 3.0);
        assert_eq!(beats.iter().map(|b| (b.index, b.coverage)).collect::<Vec<_>>(), [(1, 0.5), (2, 1.0)]);
    }
}
