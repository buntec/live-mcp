//! Beats from the Ear's audio measurements: tuning, folding thirds of a semitone into pitch
//! classes, and deciding which beats have pitch content worth naming.

use live_mcp_analysis::BeatReport;
use live_mcp_analysis::pitch::SUBBINS;

use super::{BassNote, Beat};

/// A beat counts as pitched with at least this tonalness, level and coverage.
const MIN_TONAL: f32 = 0.3;
const MIN_LEVEL_DB: f32 = -50.0;
const MIN_COVERAGE: f32 = 0.25;
/// A beat has pitches to name if its in-tune sub-bins stand out from their neighbours by at least
/// this share of the treble energy (see [`in_tune_peak`]), or if its strongest pitch class is at
/// least `STRONG_CONTRAST` times the mean of all 12. Measured on real loops: drum loops (808s,
/// percussion, sweeps) 0.01–0.03, chordal loops 0.05–0.14, lines and leads mostly from 0.03.
/// Contrast alone cannot tell them apart: drums 1.3–1.5, pads and pianos 1.3–1.8.
const MIN_PEAK: f32 = 0.035;
const STRONG_CONTRAST: f32 = 2.0;
/// A beat whose octaves from C3 (MIDI 60) up hold less than this share of its strongest octave is
/// a bass line; with a confident bass line from the tracker, the looser second limit applies (a
/// bright bass's harmonics reach well above C3).
const BASS_ONLY_UPPER: f32 = 0.1;
/// Measured on real loops: bass lines 0.01–0.18 (median), chordal loops from about 0.7.
const BASS_ONLY_UPPER_WITH_LINE: f32 = 0.22;
/// Bass-line 16ths the tracker was less sure of than this (0–255) are left out.
const MIN_BASS_CONFIDENCE: u8 = 64;
/// Onset strengths below this (on the Ear's 0–255 scale) are the flutter of steady sound, not
/// attacks.
pub const ONSET_FLOOR: f32 = 12.0;
/// Tuning is only reported when the evidence points this consistently one way (resultant
/// length over total weight, 0–1).
const MIN_TUNING_AGREEMENT: f32 = 0.3;
/// Treble bins at least this share of the beat's strongest can be peaks for the tuning estimate.
const PEAK_MIN: f32 = 0.3;

fn amplitude(level_db: f32) -> f32 {
    10f32.powf(level_db / 20.0)
}

fn counts(report: &BeatReport) -> bool {
    report.coverage >= MIN_COVERAGE && report.tonal >= MIN_TONAL && report.level_db >= MIN_LEVEL_DB
}

/// The offset from A = 440 Hz equal temperament, in cents (−50 to 50). Each peak in the treble
/// bins (thirds of a semitone, compressed) is located by parabolic interpolation between its
/// neighbours; the offsets of the peaks within their semitones are then averaged on the circle,
/// weighted by how strong and pitched the beat is.
pub fn tuning_cents(reports: &[BeatReport]) -> Option<f32> {
    const BINS: usize = 12 * SUBBINS;
    let (mut x, mut y, mut total) = (0.0f32, 0.0f32, 0.0f32);
    for report in reports.iter().filter(|r| counts(r)) {
        let weight = amplitude(report.level_db) * report.tonal;
        let bins: Vec<f32> = report.treble.iter().flatten().map(|&v| v as f32 / 255.0).collect();
        for i in 0..BINS {
            let (before, here, after) = (bins[(i + BINS - 1) % BINS], bins[i], bins[(i + 1) % BINS]);
            if here < PEAK_MIN || here <= before || here < after {
                continue;
            }
            let curve = before - 2.0 * here + after;
            let shift = if curve < 0.0 { (0.5 * (before - after) / curve).clamp(-0.5, 0.5) } else { 0.0 };
            // Bin 1 of each semitone is in tune; one bin is a third of a semitone.
            let cents = (i as f32 + shift - 1.0) * 100.0 / SUBBINS as f32;
            let angle = 2.0 * std::f32::consts::PI * cents / 100.0;
            let w = weight * here;
            x += w * angle.cos();
            y += w * angle.sin();
            total += w;
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

/// Chroma with the floor most pitch classes share (the median) taken out, and how many times the
/// mean its strongest pitch class is.
fn contrast(chroma: [f32; 12]) -> ([f32; 12], f32) {
    let mean = chroma.iter().sum::<f32>() / 12.0;
    let max = chroma.iter().copied().fold(0.0, f32::max);
    let mut sorted = chroma;
    sorted.sort_by(f32::total_cmp);
    let median = (sorted[5] + sorted[6]) / 2.0;
    (chroma.map(|v| (v - median).max(0.0)), if mean > 0.0 { max / mean } else { 0.0 })
}

/// How much the sub-bins at in-tune positions (after the tuning offset) rise above the mean of
/// their two neighbours, as a share of all treble energy. Pitched sound concentrates there;
/// noise, drums and pitch sweeps spread evenly across the thirds of a semitone.
fn in_tune_peak(treble: &[[u8; SUBBINS]; 12], cents: f32) -> f32 {
    const BINS: usize = 12 * SUBBINS;
    let bins: Vec<f32> = treble.iter().flatten().map(|&v| v as f32).collect();
    let total: f32 = bins.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    let shift = (cents / (100.0 / SUBBINS as f32)).round() as i64;
    let peak: f32 = (0..12)
        .map(|pc| {
            let at = |offset: i64| bins[((pc * SUBBINS) as i64 + 1 + shift + offset).rem_euclid(BINS as i64) as usize];
            (at(0) - (at(-1) + at(1)) / 2.0).max(0.0)
        })
        .sum();
    peak / total
}

/// The bass line of a report, without the 16ths the tracker was unsure of.
fn bass_line(r: &BeatReport) -> [Option<BassNote>; 4] {
    std::array::from_fn(|s| {
        (r.bass_notes[s] > 0 && r.bass_confidence[s] >= MIN_BASS_CONFIDENCE && r.level_db >= MIN_LEVEL_DB).then(|| {
            BassNote {
                pitch: r.bass_notes[s],
                cents: r.bass_cents[s] as f32,
                confidence: r.bass_confidence[s] as f32 / 255.0,
            }
        })
    })
}

/// Re-rounds bass notes against the section's tuning: the plugin rounds to A = 440 Hz, so a
/// detuned part's notes near a semitone boundary would flip between neighbours.
fn retune_bass(beats: &mut [Beat], tuning: f32) {
    for note in beats.iter_mut().flat_map(|b| b.bass_line.iter_mut().flatten()) {
        let offset = note.cents - tuning;
        if offset > 50.0 && note.pitch < 127 {
            note.pitch += 1;
            note.cents -= 100.0;
        } else if offset < -50.0 && note.pitch > 0 {
            note.pitch -= 1;
            note.cents += 100.0;
        }
    }
}

/// Of a beat's energy per octave, the share an octave needs before a bass note is believed to
/// sound in it.
const RESIDUE_OCTAVE_MIN: f32 = 0.25;
/// A bass note whose octave (after lifting) holds less than this share is dropped.
const BASS_NOTE_OCTAVE_MIN: f32 = 0.1;

/// Moves bass notes up an octave (at most two) when hardly any energy is in their octave but the
/// octave above has more: chords and bright tones make YIN report the residue pitch, the period
/// the partials share, an octave or two below the lowest real note (A and E together repeat at A
/// an octave down).
fn lift_residue_octaves(beats: &mut [Beat]) {
    for beat in beats.iter_mut() {
        let octaves = beat.octaves;
        let max = octaves.iter().copied().fold(0.0, f32::max);
        if max <= 0.0 {
            continue;
        }
        let energy = |pitch: u8| -> f32 {
            let octave = pitch as i32 / 12 - 2;
            if (0..7).contains(&octave) { octaves[octave as usize] / max } else { 0.0 }
        };
        for slot in beat.bass_line.iter_mut() {
            let Some(note) = slot.as_mut() else { continue };
            for _ in 0..2 {
                let (here, above) = (energy(note.pitch), energy(note.pitch + 12));
                // Below the lowest octave measured (C0), any energy above counts.
                let below_range = note.pitch < 24 && above > 0.0;
                if here < RESIDUE_OCTAVE_MIN && (above > here * 2.0 || below_range) && note.pitch + 12 <= 72 {
                    note.pitch += 12;
                } else {
                    break;
                }
            }
            // A note in an octave with hardly any of the beat's energy is not the part's bass:
            // mains hum under a guitar, rumble under a pad.
            if energy(note.pitch) < BASS_NOTE_OCTAVE_MIN {
                *slot = None;
            }
        }
    }
}

/// Fixes the pitch tracker's octave slips (common where a kick lands on a new note): of two
/// neighbouring 16ths with the same pitch class an octave apart, the less confident takes the
/// other's octave; a 16th an octave away from two neighbours that agree takes their pitch.
fn smooth_octaves(beats: &mut [Beat]) {
    let contiguous: Vec<bool> =
        beats.iter().enumerate().map(|(t, b)| t == 0 || b.index == beats[t - 1].index + 1).collect();
    let n = beats.len() * 4;
    let get = |beats: &[Beat], k: usize| beats[k / 4].bass_line[k % 4];
    // Slots k and k + 1 are neighbours unless a gap between beats separates them.
    let adjacent = |k: usize| k % 4 != 3 || contiguous[k / 4 + 1];
    for k in 1..n.saturating_sub(1) {
        if !(adjacent(k - 1) && adjacent(k)) {
            continue;
        }
        if let (Some(a), Some(m), Some(c)) = (get(beats, k - 1), get(beats, k), get(beats, k + 1))
            && a.pitch == c.pitch
            && (m.pitch as i32 - a.pitch as i32).abs() == 12
            && let Some(note) = beats[k / 4].bass_line[k % 4].as_mut()
        {
            note.pitch = a.pitch;
        }
    }
    for k in 0..n.saturating_sub(1) {
        if !adjacent(k) {
            continue;
        }
        if let (Some(a), Some(b)) = (get(beats, k), get(beats, k + 1))
            && (a.pitch as i32 - b.pitch as i32).abs() == 12
        {
            let (weaker, pitch) = if a.confidence < b.confidence { (k, b.pitch) } else { (k + 1, a.pitch) };
            if let Some(note) = beats[weaker / 4].bass_line[weaker % 4].as_mut() {
                note.pitch = pitch;
            }
        }
    }
}

/// A part is a bass line or not for longer than a beat: each pitched beat takes the majority of
/// itself and its pitched neighbours, so chord labels do not flicker in and out of a bass line.
fn smooth_bass_only(beats: &mut [Beat]) {
    let was: Vec<Option<bool>> = beats.iter().map(|b| b.pitched.then_some(b.bass_only)).collect();
    for t in 0..beats.len() {
        if !beats[t].pitched {
            continue;
        }
        let near = |u: usize| beats[u].index.abs_diff(beats[t].index) <= 1;
        let votes: Vec<bool> =
            (t.saturating_sub(1)..(t + 2).min(beats.len())).filter(|&u| near(u)).filter_map(|u| was[u]).collect();
        let yes = votes.iter().filter(|v| **v).count();
        beats[t].bass_only = yes * 2 > votes.len();
    }
}

/// The heard beats, sorted, and the tuning they were folded with.
pub fn beats(reports: &[BeatReport]) -> (Vec<Beat>, Option<f32>) {
    let tuning = tuning_cents(reports);
    let cents = tuning.unwrap_or(0.0);
    let mut beats: Vec<Beat> = reports
        .iter()
        .map(|r| {
            let (chroma, strength) = contrast(fold(&r.treble, cents));
            let (bass, _) = contrast(fold(&r.bass, cents));
            let peaked = strength > 0.0 && (in_tune_peak(&r.treble, cents) >= MIN_PEAK || strength >= STRONG_CONTRAST);
            let line = bass_line(r);
            let has_line = line.iter().any(Option::is_some);
            let upper = r.octaves[3..].iter().map(|&v| v as f32).sum::<f32>() / 255.0;
            let upper_quiet = upper < BASS_ONLY_UPPER || (has_line && upper < BASS_ONLY_UPPER_WITH_LINE);
            Beat {
                index: r.beat,
                coverage: r.coverage,
                weight: amplitude(r.level_db),
                // A confident bass line is pitch content even when the treble (only its
                // harmonics) does not pass; such a beat is a bass line, with no chord.
                pitched: (counts(r) && peaked) || (has_line && r.coverage >= MIN_COVERAGE),
                chroma,
                bass,
                octaves: r.octaves.map(|v| v as f32 / 255.0),
                bass_only: upper_quiet || (has_line && !(counts(r) && peaked)),
                bass_line: line,
                onsets: r.onsets.map(|v| if (v as f32) < ONSET_FLOOR { 0.0 } else { v as f32 }),
                span: None,
                tonal: Some(r.tonal),
            }
        })
        .collect();
    beats.sort_by_key(|b| b.index);
    retune_bass(&mut beats, cents);
    lift_residue_octaves(&mut beats);
    smooth_octaves(&mut beats);
    smooth_bass_only(&mut beats);
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
            bass_notes: [0; 4],
            bass_cents: [0; 4],
            bass_confidence: [0; 4],
        }
    }

    #[test]
    fn in_tune_and_sharp_material() {
        let in_tune = [report(0, &[(0, 1), (4, 1), (7, 1)])];
        assert!(tuning_cents(&in_tune).unwrap().abs() < 0.01);
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

    /// Four bars at 120 BPM of plucked sawtooth chords (MIDI notes per bar) over a bass an octave
    /// down, kicks on every beat and hats on the off-beats, detuned by `cents`, through the real
    /// beat analyzer and the interpreter.
    fn band(chords: [&[f64]; 4], roots: [f64; 4], cents: f64) -> (crate::harmony::Analysis, Option<f32>) {
        use std::f64::consts::PI;

        use live_mcp_analysis::{BeatAnalyzer, Transport};

        use crate::harmony::{BarSpan, Section, Source, analyze};

        let sr = 48_000.0f64;
        let detune = 2f64.powf(cents / 1200.0);
        let hz = |midi: f64| 440.0 * 2f64.powf((midi - 69.0) / 12.0) * detune;
        let saw = |f: f64, t: f64| {
            (1..=16)
                .filter(|h| f * *h as f64 * 2.0 < sr)
                .map(|h| (2.0 * PI * f * h as f64 * t).sin() / h as f64)
                .sum::<f64>()
        };
        let mut noise = 0x2545_f491u32;
        let samples: Vec<f32> = (0..(8.0 * sr) as usize)
            .map(|n| {
                let t = n as f64 / sr;
                let bar = (t / 2.0) as usize;
                let in_beat = t % 0.5;
                let pluck = (-in_beat * 4.0).exp();
                let chord: f64 = chords[bar].iter().map(|&m| saw(hz(m), t)).sum::<f64>() * 0.06 * pluck;
                let bass = saw(hz(roots[bar]), t) * 0.12 * pluck;
                let kick = if in_beat < 0.12 {
                    let f = 50.0 + 70.0 * (-in_beat * 40.0).exp();
                    (2.0 * PI * f * in_beat).sin() * 0.5 * (-in_beat * 25.0).exp()
                } else {
                    0.0
                };
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let hat =
                    if (0.25..0.265).contains(&in_beat) { (noise as f64 / u32::MAX as f64 - 0.5) * 0.15 } else { 0.0 };
                (chord + bass + kick + hat) as f32
            })
            .collect();
        let mut analyzer = BeatAnalyzer::new(sr as f32);
        let mut reports = Vec::new();
        for (k, block) in samples.chunks(512).enumerate() {
            let beat = (k * 512) as f64 / sr * 2.0;
            let t = Transport { playing: true, beat: Some(beat), tempo: Some(120.0), time_signature: Some((4, 4)) };
            analyzer.process(block, block, &t, &mut |r| reports.push(r));
        }
        analyzer.flush(&mut |r| reports.push(r));
        let (heard, tuning) = beats(&reports);
        let spans: Vec<BarSpan> =
            (0..4).map(|i| BarSpan { bar: i + 1, start_beat: i as f64 * 4.0, beats: 4.0 }).collect();
        (analyze(&Section { bars: &spans, beats: &heard, source: Source::Audio, scale: None }), tuning)
    }

    #[test]
    fn a_band_through_the_whole_audio_path() {
        let chords: [&[f64]; 4] = [&[57.0, 60.0, 64.0], &[53.0, 57.0, 60.0], &[55.0, 60.0, 64.0], &[55.0, 59.0, 62.0]];
        let (analysis, tuning) = band(chords, [45.0, 41.0, 48.0, 43.0], 20.0);
        let cents = tuning.expect("a tuning");
        assert!((cents - 20.0).abs() < 8.0, "tuning {cents}");
        let s = &analysis.summary;
        assert_eq!(s["progression"], "Am | F | C | G", "{s:?}");
        assert!(["C major", "A minor"].contains(&s["key"]["name"].as_str().unwrap()), "{s:?}");
        let basses: Vec<&str> = analysis.bars.iter().map(|(_, f)| f["bass"].as_str().unwrap()).collect();
        assert_eq!(basses, ["A", "F", "C", "G"], "{:?}", analysis.bars);
        // Every beat starts with a pluck and a kick; the hats sit on the off-beat 8ths.
        let grid = analysis.bars[1].1["rhythm"].as_str().unwrap();
        assert!(grid.split(' ').all(|beat| beat.starts_with('X')), "{grid}");
        assert!(grid.split(' ').all(|beat| beat.as_bytes()[2] != b'.'), "{grid}");
    }

    #[test]
    fn seventh_chords_through_the_whole_audio_path() {
        // Dm7 | G7 | Cmaj7 | A7, in tune.
        let chords: [&[f64]; 4] = [
            &[57.0, 60.0, 62.0, 65.0],
            &[53.0, 55.0, 59.0, 62.0],
            &[55.0, 59.0, 60.0, 64.0],
            &[55.0, 57.0, 61.0, 64.0],
        ];
        let (analysis, tuning) = band(chords, [38.0, 43.0, 36.0, 45.0], 0.0);
        assert!(tuning.is_none_or(|c| c.abs() < 8.0), "{tuning:?}");
        let s = &analysis.summary;
        assert_eq!(s["progression"], "Dm7 | G7 | Cmaj7 | A7", "{s:?} {:?}", analysis.bars);
        assert!(["C major", "A minor", "D minor"].contains(&s["key"]["name"].as_str().unwrap()), "{s:?}");
    }

    /// A D minor bass line in 8ths with kicks on the beat, through the beat analyzer: the notes
    /// come from the pitch tracker, not from the harmonics, so the key is D minor (a low D's 5th
    /// harmonic is an F#, which used to suggest D major).
    #[test]
    fn a_bass_line_through_the_whole_audio_path() {
        use std::f64::consts::PI;

        use live_mcp_analysis::{BeatAnalyzer, Transport};

        use crate::harmony::{BarSpan, Section, Source, analyze};

        let sr = 48_000.0f64;
        // MIDI: D2 = 38, F2 = 41, G2 = 43, A2 = 45, Bb1 = 34, C3 = 48.
        let bars: [[u8; 8]; 4] = [
            [38, 38, 41, 38, 45, 38, 48, 38],
            [43, 43, 46, 43, 45, 45, 38, 38],
            [38, 38, 41, 38, 45, 38, 48, 38],
            [34, 34, 45, 45, 43, 41, 40, 38],
        ];
        let samples: Vec<f32> = (0..(8.0 * sr) as usize)
            .map(|n| {
                let t = n as f64 / sr;
                let eighth = (t / 0.25) as usize;
                let note = bars[eighth / 8][eighth % 8] as f64;
                let f = 440.0 * 2f64.powf((note - 69.0) / 12.0);
                let pluck = (-(t % 0.25) * 6.0).exp();
                let bass = (1..=12).map(|h| (2.0 * PI * f * h as f64 * t).sin() / h as f64).sum::<f64>() * 0.25 * pluck;
                let in_beat = t % 0.5;
                let kick = if in_beat < 0.1 {
                    (2.0 * PI * (50.0 + 60.0 * (-in_beat * 40.0).exp()) * in_beat).sin() * 0.4 * (-in_beat * 30.0).exp()
                } else {
                    0.0
                };
                (bass + kick) as f32
            })
            .collect();
        let mut analyzer = BeatAnalyzer::new(sr as f32);
        let mut reports = Vec::new();
        for (k, block) in samples.chunks(512).enumerate() {
            let beat = (k * 512) as f64 / sr * 2.0;
            let t = Transport { playing: true, beat: Some(beat), tempo: Some(120.0), time_signature: Some((4, 4)) };
            analyzer.process(block, block, &t, &mut |r| reports.push(r));
        }
        analyzer.flush(&mut |r| reports.push(r));
        let (heard, _) = beats(&reports);
        let spans: Vec<BarSpan> =
            (0..4).map(|i| BarSpan { bar: i + 1, start_beat: i as f64 * 4.0, beats: 4.0 }).collect();
        let analysis = analyze(&Section { bars: &spans, beats: &heard, source: Source::Audio, scale: None });
        let s = &analysis.summary;
        assert_eq!(s["key"]["name"], "D minor", "{s:?}");
        let (_, bar1) = &analysis.bars[0];
        // Repeated 8ths of one note on weak onsets read as held; the rhythm grid has the attacks.
        assert_eq!(bar1["bass_line"], "D1 - - -  F1 - D1 -  A1 - D1 -  C2 - D1 -", "{bar1:?}");
        assert_eq!(bar1["bass"], "D");
        assert_eq!(bar1["range"], "D1–C2");
        assert_eq!(bar1["chord"], "N", "a bass line is not a chord");
    }

    #[test]
    fn evenly_spread_chroma_is_not_pitched() {
        // Energy in every third of a semitone, as from noise or an 808's pitch sweep.
        let all: Vec<(usize, usize)> = (0..12).flat_map(|pc| (0..3).map(move |j| (pc, j))).collect();
        let (heard, _) = beats(&[report(0, &all)]);
        assert!(!heard[0].pitched);
        // The same energy at in-tune positions only is twelve notes: pitched.
        let in_tune: Vec<(usize, usize)> = (0..12).map(|pc| (pc, 1)).collect();
        let (heard, _) = beats(&[report(0, &in_tune)]);
        assert!(heard[0].pitched);
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
