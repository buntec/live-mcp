//! Per-beat pitch and rhythm features: what the server needs to name keys, chords and grooves.

use serde::{Deserialize, Serialize};

use crate::bars::Transport;
use crate::bassline::{BassFrame, BassTracker, LOWEST_NOTE, NOTES};
use crate::meter::FLOOR_DB;
use crate::onset::OnsetMeter;
use crate::pitch::{OCTAVES, PitchFrame, PitchMeter, SUBBINS};

/// A jump in the host position larger than this (in beats) is a seek or loop restart.
const JUMP_TOLERANCE_BEATS: f64 = 0.01;
/// Beats waiting for their last spectrum frames. A frame's centre lags the input by ~170 ms, so
/// a few beats are open at once at fast tempos.
const MAX_OPEN: usize = 8;
/// Onset flux × this, rounded and capped at 255, is the reported strength.
const ONSET_SCALE: f32 = 200.0;

/// Measurements for one beat (quarter note).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BeatReport {
    /// 0-based beat from the song start (beat 0 is the start of bar 1).
    pub beat: i64,
    /// Fraction of the beat that was heard.
    pub coverage: f32,
    /// RMS of the mid signal, dBFS.
    pub level_db: f32,
    /// 0 = noise or drums, 1 = clearly pitched.
    pub tonal: f32,
    /// Chroma shapes, scaled so the beat's largest value is 255: per pitch class (C first), a
    /// third of a semitone flat, in tune (A = 440 Hz) and a third sharp.
    pub treble: [[u8; SUBBINS]; 12],
    pub bass: [[u8; SUBBINS]; 12],
    /// Energy per octave, Live's octaves 0–6 (C0 = MIDI 24 … B6), largest = 255.
    pub octaves: [u8; OCTAVES],
    /// Onset strength per 16th note, on a fixed scale (comparable across beats and tracks).
    pub onsets: [u8; 4],
    /// The bass line per 16th (protocol 3): the fundamental's MIDI note (0 = none), its offset in
    /// cents, and how sure the tracker was (0–255: the share of the 16th's frames that agreed,
    /// times their periodicity).
    #[serde(default)]
    pub bass_notes: [u8; 4],
    #[serde(default)]
    pub bass_cents: [i8; 4],
    #[serde(default)]
    pub bass_confidence: [u8; 4],
}

#[derive(Debug, Clone, Copy)]
struct OpenBeat {
    index: i64,
    tempo: f64,
    samples: u64,
    square: f64,
    treble: [f32; 12 * SUBBINS],
    bass: [f32; 12 * SUBBINS],
    octaves: [f32; OCTAVES],
    tonal: f32,
    tonal_weight: f32,
    onsets: [f32; 4],
    /// Per 16th: confidence summed per bass note, cents × confidence per note, and frames seen.
    bass_votes: [[f32; NOTES]; 4],
    bass_cents: [[f32; NOTES]; 4],
    bass_frames: [u16; 4],
}

impl OpenBeat {
    fn new(index: i64, tempo: f64) -> Self {
        Self {
            index,
            tempo,
            samples: 0,
            square: 0.0,
            treble: [0.0; 36],
            bass: [0.0; 36],
            octaves: [0.0; OCTAVES],
            tonal: 0.0,
            tonal_weight: 0.0,
            onsets: [0.0; 4],
            bass_votes: [[0.0; NOTES]; 4],
            bass_cents: [[0.0; NOTES]; 4],
            bass_frames: [0; 4],
        }
    }

    fn add_bass(&mut self, sixteenth: usize, frame: &BassFrame) {
        self.bass_frames[sixteenth] = self.bass_frames[sixteenth].saturating_add(1);
        if let Some((note, cents)) = frame.note() {
            let k = (note - LOWEST_NOTE) as usize;
            self.bass_votes[sixteenth][k] += frame.confidence;
            self.bass_cents[sixteenth][k] += cents * frame.confidence;
        }
    }

    fn add_frame(&mut self, frame: &PitchFrame) {
        for (sum, v) in self.treble.iter_mut().zip(&frame.treble) {
            *sum += v;
        }
        for (sum, v) in self.bass.iter_mut().zip(&frame.bass) {
            *sum += v;
        }
        for (sum, v) in self.octaves.iter_mut().zip(&frame.octaves) {
            *sum += v;
        }
        self.tonal += frame.tonal * frame.power;
        self.tonal_weight += frame.power;
    }

    fn report(&self, sample_rate: f64) -> BeatReport {
        fn scaled<const N: usize>(values: &[f32; N]) -> [u8; N] {
            let max = values.iter().copied().fold(0.0, f32::max);
            values.map(|v| if max > 0.0 { (v / max * 255.0).round() as u8 } else { 0 })
        }
        fn grouped(values: &[f32; 36]) -> [[u8; SUBBINS]; 12] {
            let flat = scaled(values);
            std::array::from_fn(|pc| std::array::from_fn(|j| flat[pc * SUBBINS + j]))
        }
        let (mut bass_notes, mut bass_cents, mut bass_confidence) = ([0u8; 4], [0i8; 4], [0u8; 4]);
        for s in 0..4 {
            let votes = &self.bass_votes[s];
            let Some((k, &best)) = votes.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else { continue };
            if best > 0.0 && self.bass_frames[s] > 0 {
                bass_notes[s] = LOWEST_NOTE + k as u8;
                bass_cents[s] = (self.bass_cents[s][k] / best).round().clamp(-50.0, 50.0) as i8;
                bass_confidence[s] = (best / self.bass_frames[s] as f32 * 255.0).round().min(255.0) as u8;
            }
        }
        let beat_samples = 60.0 / self.tempo * sample_rate;
        let mean_square = if self.samples > 0 { self.square / self.samples as f64 } else { 0.0 };
        BeatReport {
            beat: self.index,
            coverage: ((self.samples as f64 / beat_samples) as f32).min(1.0),
            level_db: if mean_square > 0.0 { ((10.0 * mean_square.log10()) as f32).max(FLOOR_DB) } else { FLOOR_DB },
            tonal: if self.tonal_weight > 0.0 { self.tonal / self.tonal_weight } else { 0.0 },
            treble: grouped(&self.treble),
            bass: grouped(&self.bass),
            octaves: scaled(&self.octaves),
            onsets: self.onsets.map(|f| (f * ONSET_SCALE).round().min(255.0) as u8),
            bass_notes,
            bass_cents,
            bass_confidence,
        }
    }
}

/// Follows the host transport and emits a [`BeatReport`] for each beat heard, about a quarter of
/// a second after the beat ends (when the spectrum frames centred in it are in).
///
/// `process` does not allocate, so it can run on the audio thread.
#[derive(Debug)]
pub struct BeatAnalyzer {
    sample_rate: f64,
    pitch: PitchMeter,
    onset: OnsetMeter,
    bass: BassTracker,
    open: [Option<OpenBeat>; MAX_OPEN],
    expected_beat: Option<f64>,
}

impl BeatAnalyzer {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate as f64;
        Self {
            sample_rate,
            pitch: PitchMeter::new(sample_rate),
            onset: OnsetMeter::new(sample_rate),
            bass: BassTracker::new(sample_rate),
            open: [None; MAX_OPEN],
            expected_beat: None,
        }
    }

    /// Drops unfinished beats without reporting them and clears the spectra.
    pub fn reset(&mut self) {
        self.open = [None; MAX_OPEN];
        self.pitch.reset();
        self.onset.reset();
        self.bass.reset();
        self.expected_beat = None;
    }

    /// Reports every open beat (with what was heard of it) and clears the spectra, so frames never
    /// mix audio from both sides of a jump.
    pub fn flush(&mut self, emit: &mut impl FnMut(BeatReport)) {
        let mut open: [Option<OpenBeat>; MAX_OPEN] = [None; MAX_OPEN];
        std::mem::swap(&mut open, &mut self.open);
        open.sort_by_key(|b| b.map_or(i64::MAX, |b| b.index));
        for beat in open.iter().flatten().filter(|b| b.samples > 0) {
            emit(beat.report(self.sample_rate));
        }
        self.pitch.reset();
        self.onset.reset();
        self.bass.reset();
    }

    fn slot(&mut self, index: i64) -> Option<&mut OpenBeat> {
        self.open.iter_mut().flatten().find(|b| b.index == index)
    }

    /// The open beat `index`, opened if needed; when all slots are taken, the oldest is reported.
    fn open_beat(&mut self, index: i64, tempo: f64, emit: &mut impl FnMut(BeatReport)) -> &mut OpenBeat {
        let position = match self.open.iter().position(|b| b.is_some_and(|b| b.index == index)) {
            Some(p) => p,
            None => {
                let p = self.open.iter().position(Option::is_none).unwrap_or_else(|| {
                    let oldest = (0..MAX_OPEN).min_by_key(|&p| self.open[p].map_or(i64::MAX, |b| b.index)).unwrap_or(0);
                    if let Some(beat) = self.open[oldest].take() {
                        emit(beat.report(self.sample_rate));
                    }
                    oldest
                });
                self.open[p] = Some(OpenBeat::new(index, tempo));
                p
            }
        };
        self.open[position].as_mut().expect("just opened")
    }

    /// Reports open beats that end at or before `beat`, in order.
    fn complete_before(&mut self, beat: f64, emit: &mut impl FnMut(BeatReport)) {
        loop {
            let next = (0..MAX_OPEN)
                .filter(|&p| self.open[p].is_some_and(|b| (b.index + 1) as f64 <= beat))
                .min_by_key(|&p| self.open[p].map(|b| b.index));
            let Some(p) = next else { break };
            if let Some(b) = self.open[p].take() {
                emit(b.report(self.sample_rate));
            }
        }
    }

    /// Processes one block. `left` and `right` must have the same length; pass the same slice
    /// twice for mono.
    pub fn process(&mut self, left: &[f32], right: &[f32], transport: &Transport, emit: &mut impl FnMut(BeatReport)) {
        let position = match (transport.playing, transport.beat, transport.tempo) {
            (true, Some(beat), Some(tempo)) if tempo > 0.0 && beat.is_finite() => Some((beat, tempo)),
            _ => None,
        };
        let Some((start_beat, tempo)) = position else {
            if self.expected_beat.take().is_some() || self.open.iter().any(Option::is_some) {
                self.flush(emit);
            }
            return;
        };
        if self.expected_beat.is_some_and(|expected| (start_beat - expected).abs() > JUMP_TOLERANCE_BEATS)
            || self.expected_beat.is_none()
        {
            self.flush(emit);
        }
        let beats_per_sample = tempo / 60.0 / self.sample_rate;
        let onset_delay = self.onset.attack_delay() as f64 * beats_per_sample;
        let pitch_delay = self.pitch.centre_delay() as f64 * beats_per_sample;
        let bass_delay = self.bass.centre_delay() as f64 * beats_per_sample;
        for (i, (&l, &r)) in left.iter().zip(right).enumerate() {
            let beat = start_beat + i as f64 * beats_per_sample;
            let mid = (l + r) * 0.5;
            let open = self.open_beat((beat + 1e-9).floor() as i64, tempo, emit);
            open.samples += 1;
            open.square += (mid as f64) * (mid as f64);

            if let Some(flux) = self.onset.push(mid) {
                // Flux rises as soon as an attack enters the window, so it is placed on the
                // nearest 16th rather than the one the frame centre falls in.
                let at = ((beat - onset_delay) * 4.0).round() as i64;
                let sixteenth = at.rem_euclid(4) as usize;
                if let Some(open) = self.slot(at.div_euclid(4)) {
                    open.onsets[sixteenth] = open.onsets[sixteenth].max(flux);
                }
            }
            if let Some(frame) = self.bass.push(mid) {
                let centre = beat - bass_delay;
                let index = (centre + 1e-9).floor() as i64;
                let sixteenth = (((centre - index as f64) * 4.0) as usize).min(3);
                if let Some(open) = self.slot(index) {
                    open.add_bass(sixteenth, &frame);
                }
            }
            if let Some(frame) = self.pitch.push(mid) {
                let centre = beat - pitch_delay;
                if let Some(open) = self.slot((centre + 1e-9).floor() as i64) {
                    open.add_frame(&frame);
                }
                self.complete_before(centre, emit);
            }
        }
        self.expected_beat = Some(start_beat + left.len() as f64 * beats_per_sample);
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    const SR: f32 = 48_000.0;

    fn play(signal: impl Fn(f64) -> f64, tempo: f64, seconds: f64) -> Vec<BeatReport> {
        let mut analyzer = BeatAnalyzer::new(SR);
        let mut reports = Vec::new();
        let block = 512;
        let blocks = (seconds * SR as f64) as usize / block;
        for k in 0..blocks {
            let samples: Vec<f32> = (0..block).map(|i| signal((k * block + i) as f64 / SR as f64) as f32).collect();
            let beat = (k * block) as f64 * tempo / 60.0 / SR as f64;
            let t = Transport { playing: true, beat: Some(beat), tempo: Some(tempo), time_signature: Some((4, 4)) };
            analyzer.process(&samples, &samples, &t, &mut |r| reports.push(r));
        }
        analyzer.flush(&mut |r| reports.push(r));
        reports
    }

    fn strongest(chroma: &[[u8; 3]; 12]) -> usize {
        (0..12).max_by_key(|&pc| chroma[pc].iter().map(|&v| v as u32).sum::<u32>()).unwrap()
    }

    #[test]
    fn a_chord_change_shows_in_the_beats() {
        // 120 BPM: two beats of A (440 Hz), then two of C (523 Hz).
        let signal = |t: f64| {
            let hz = if (t / 0.5) as usize % 4 < 2 { 440.0 } else { 523.25 };
            0.3 * (2.0 * PI * hz * t).sin()
        };
        let reports = play(signal, 120.0, 4.0);
        let beats: Vec<i64> = reports.iter().map(|r| r.beat).collect();
        assert_eq!(beats, (0..8).collect::<Vec<_>>());
        assert!(reports.iter().all(|r| r.coverage > 0.99), "{reports:?}");
        // Skip beats whose frames straddle the change.
        assert_eq!(strongest(&reports[0].treble), 9);
        assert_eq!(strongest(&reports[3].treble), 0);
        assert_eq!(strongest(&reports[4].treble), 9);
        assert!(reports[1].tonal > 0.8);
        assert!((reports[1].level_db - (-13.5)).abs() < 0.5, "{}", reports[1].level_db); // 0.3/√2 → ≈ −13.5 dB
    }

    #[test]
    fn clicks_on_eighths_land_in_the_right_sixteenths() {
        // 120 BPM: a short click every 8th note (every 0.25 s).
        let signal = |t: f64| if t % 0.25 < 0.002 { 0.8 } else { 0.0 };
        let reports = play(signal, 120.0, 3.0);
        for r in &reports[1..reports.len() - 1] {
            assert!(r.onsets[0] > 20 && r.onsets[2] > 20, "{:?}", r.onsets);
            assert!(r.onsets[1] < r.onsets[0] / 4 && r.onsets[3] < r.onsets[2] / 4, "{:?}", r.onsets);
        }
    }

    #[test]
    fn the_bass_line_comes_out_per_sixteenth() {
        // 120 BPM: a sawtooth bass playing A1 (MIDI 45) for an 8th, then C2 (MIDI 48) for an 8th,
        // over and over.
        let signal = |t: f64| {
            let f = if ((t / 0.25) as usize).is_multiple_of(2) { 110.0 } else { 130.81 };
            (1..=12).map(|h| (2.0 * PI * f * h as f64 * t).sin() / h as f64).sum::<f64>() * 0.2
        };
        let reports = play(signal, 120.0, 3.0);
        for r in &reports[1..reports.len() - 1] {
            assert_eq!(r.bass_notes, [45, 45, 48, 48], "{r:?}");
            assert!(r.bass_confidence.iter().all(|&c| c > 150), "{:?}", r.bass_confidence);
            assert!(r.bass_cents.iter().all(|c| c.abs() < 10), "{:?}", r.bass_cents);
        }
    }

    #[test]
    fn stopping_and_jumping_flush_the_open_beats() {
        let mut analyzer = BeatAnalyzer::new(SR);
        let mut reports = Vec::new();
        let block = vec![0.1f32; 4800]; // 0.1 s
        for k in 0..6 {
            let t = Transport {
                playing: true,
                beat: Some(k as f64 * 0.2),
                tempo: Some(120.0),
                time_signature: Some((4, 4)),
            };
            analyzer.process(&block, &block, &t, &mut |r| reports.push(r));
        }
        assert!(reports.len() < 2);
        let jump = Transport { playing: true, beat: Some(16.0), tempo: Some(120.0), time_signature: Some((4, 4)) };
        analyzer.process(&block, &block, &jump, &mut |r| reports.push(r));
        assert_eq!(reports.last().map(|r| r.beat), Some(1));
        analyzer.process(&block, &block, &Transport::default(), &mut |r| reports.push(r));
        assert_eq!(reports.last().map(|r| r.beat), Some(16));
    }
}
