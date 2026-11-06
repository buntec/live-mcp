//! Splitting a stream of audio blocks into bars, using the host's transport position.

use serde::{Deserialize, Serialize};

use crate::meter::{SignalMeter, Stats, Sums};

/// A jump in the host position larger than this (in beats) between consecutive blocks is a
/// seek or a loop restart, not continuous playback.
const JUMP_TOLERANCE_BEATS: f64 = 0.01;

/// What the host reports about the transport at the start of a block.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Transport {
    pub playing: bool,
    /// Position of the block's first sample, in beats (quarter notes) from the song start.
    pub beat: Option<f64>,
    pub tempo: Option<f64>,
    /// Time signature as (numerator, denominator).
    pub time_signature: Option<(u32, u32)>,
}

/// Measurements for one bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BarReport {
    /// 1-based bar number, as Live displays it (beat 0 is bar 1).
    pub bar: i64,
    /// Start of the bar, in beats.
    pub start_beat: f64,
    /// Length of the bar, in beats.
    pub beats: f64,
    pub tempo: f64,
    /// Fraction of the bar that was heard: below 1 if playback started, stopped or jumped
    /// inside it.
    pub coverage: f32,
    pub stats: Stats,
}

#[derive(Debug, Clone, Copy)]
struct OpenBar {
    index: i64,
    beats: f64,
    tempo: f64,
    sums: Sums,
}

/// Feeds audio through a [`SignalMeter`] and emits a [`BarReport`] whenever a bar ends.
///
/// `process` does not allocate, so it can run on the audio thread.
#[derive(Debug, Clone)]
pub struct BarAnalyzer {
    sample_rate: f64,
    meter: SignalMeter,
    open: Option<OpenBar>,
    /// Where the next block should start if playback is continuous.
    expected_beat: Option<f64>,
}

impl BarAnalyzer {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate as f64;
        Self { sample_rate, meter: SignalMeter::new(sample_rate), open: None, expected_beat: None }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Clears filter state and drops any unfinished bar without reporting it.
    pub fn reset(&mut self) {
        self.meter.reset();
        self.open = None;
        self.expected_beat = None;
    }

    /// Ends the current bar early, reporting it with its partial coverage.
    pub fn flush(&mut self, emit: &mut impl FnMut(BarReport)) {
        if let Some(bar) = self.open.take()
            && bar.sums.samples > 0
        {
            let full_bar_samples = bar.beats * 60.0 / bar.tempo * self.sample_rate;
            emit(BarReport {
                bar: bar.index + 1,
                start_beat: bar.index as f64 * bar.beats,
                beats: bar.beats,
                tempo: bar.tempo,
                coverage: ((bar.sums.samples as f64 / full_bar_samples) as f32).min(1.0),
                stats: bar.sums.stats(),
            });
        }
    }

    /// Processes one block. `left` and `right` must have the same length; pass the same slice
    /// twice for mono.
    pub fn process(&mut self, left: &[f32], right: &[f32], transport: &Transport, emit: &mut impl FnMut(BarReport)) {
        let n = left.len();
        let position = match (transport.playing, transport.beat, transport.tempo) {
            (true, Some(beat), Some(tempo)) if tempo > 0.0 && beat.is_finite() => Some((beat, tempo)),
            _ => None,
        };
        let Some((start_beat, tempo)) = position else {
            // Stopped: finish the bar in progress, but keep the filters warm.
            self.flush(emit);
            self.meter.feed(left, right, &mut Sums::default());
            self.expected_beat = None;
            return;
        };
        if self.expected_beat.is_some_and(|expected| (start_beat - expected).abs() > JUMP_TOLERANCE_BEATS) {
            self.flush(emit);
        }
        let (num, den) = transport.time_signature.filter(|(n, d)| *n > 0 && *d > 0).unwrap_or((4, 4));
        let beats_per_bar = num as f64 * 4.0 / den as f64;
        let beats_per_sample = tempo / 60.0 / self.sample_rate;

        let mut i = 0;
        while i < n {
            let beat = start_beat + i as f64 * beats_per_sample;
            let index = (beat / beats_per_bar + 1e-9).floor() as i64;
            let bar_end = (index + 1) as f64 * beats_per_bar;
            // Samples whose position is before the bar end. The small tolerance stops rounding
            // error from counting one sample too many when a bar ends exactly on a block edge.
            let left_in_bar = (((bar_end - beat) / beats_per_sample - 1e-6).ceil() as usize).max(1);
            let end = n.min(i + left_in_bar);

            let reopen = match &self.open {
                Some(bar) => bar.index != index || bar.beats != beats_per_bar,
                None => true,
            };
            if reopen {
                self.flush(emit);
                self.open = Some(OpenBar { index, beats: beats_per_bar, tempo, sums: Sums::default() });
            }
            let bar = self.open.as_mut().expect("just opened");
            self.meter.feed(&left[i..end], &right[i..end], &mut bar.sums);
            if i + left_in_bar <= n {
                self.flush(emit); // the bar ended inside this block
            }
            i = end;
        }
        self.expected_beat = Some(start_beat + n as f64 * beats_per_sample);
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    const SR: f32 = 48_000.0;

    /// Plays `seconds` of a sine through the analyzer in host-sized blocks, starting at `beat`.
    fn play(analyzer: &mut BarAnalyzer, start_beat: f64, tempo: f64, seconds: f64, block: usize) -> Vec<BarReport> {
        let total = (seconds * SR as f64) as usize;
        let mut reports = Vec::new();
        let mut done = 0;
        while done < total {
            let len = block.min(total - done);
            let samples: Vec<f32> =
                (done..done + len).map(|i| (0.5 * (2.0 * PI * 440.0 * i as f64 / SR as f64).sin()) as f32).collect();
            let beat = start_beat + done as f64 * tempo / 60.0 / SR as f64;
            let transport =
                Transport { playing: true, beat: Some(beat), tempo: Some(tempo), time_signature: Some((4, 4)) };
            analyzer.process(&samples, &samples, &transport, &mut |r| reports.push(r));
            done += len;
        }
        reports
    }

    #[test]
    fn reports_each_full_bar() {
        // 120 BPM, 4/4: a bar is 2 s. Eight seconds from the top give exactly bars 1–4.
        let mut analyzer = BarAnalyzer::new(SR);
        let mut reports = play(&mut analyzer, 0.0, 120.0, 8.0, 512);
        analyzer.flush(&mut |r| reports.push(r));
        let bars: Vec<i64> = reports.iter().map(|r| r.bar).collect();
        assert_eq!(bars, [1, 2, 3, 4]);
        for r in &reports {
            assert!((r.coverage - 1.0).abs() < 1e-3, "{r:?}");
            assert_eq!(r.beats, 4.0);
            assert!((r.stats.rms_dbfs[0] - -9.03).abs() < 0.05, "{r:?}");
        }
        assert_eq!(reports[2].start_beat, 8.0);
    }

    #[test]
    fn starting_mid_bar_gives_partial_coverage() {
        let mut analyzer = BarAnalyzer::new(SR);
        // Start at beat 2 (half way through bar 1) and play 3 s: half of bar 1, all of bar 2.
        let reports = play(&mut analyzer, 2.0, 120.0, 3.0, 480);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].bar, 1);
        assert!((reports[0].coverage - 0.5).abs() < 1e-3);
        assert_eq!(reports[1].bar, 2);
        assert!((reports[1].coverage - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_loop_jump_ends_the_bar_early() {
        let mut analyzer = BarAnalyzer::new(SR);
        // Play bar 3 for 1 s (half), then jump back to bar 1 as a loop restart would.
        let mut reports = play(&mut analyzer, 8.0, 120.0, 1.0, 512);
        assert!(reports.is_empty());
        reports.extend(play(&mut analyzer, 0.0, 120.0, 2.0, 512));
        assert_eq!(reports.iter().map(|r| r.bar).collect::<Vec<_>>(), [3, 1]);
        assert!((reports[0].coverage - 0.5).abs() < 2e-3);
    }

    #[test]
    fn stopping_flushes_and_other_meters_work() {
        let mut analyzer = BarAnalyzer::new(SR);
        let mut reports = play(&mut analyzer, 0.0, 90.0, 1.0, 256);
        let stopped = Transport { playing: false, ..Transport::default() };
        analyzer.process(&[0.0; 256], &[0.0; 256], &stopped, &mut |r| reports.push(r));
        assert_eq!(reports.len(), 1, "stopping reports the unfinished bar");
        assert!((reports[0].coverage - 1.0 / (4.0 * 60.0 / 90.0)).abs() < 1e-3);
        assert_eq!(reports[0].tempo, 90.0);
    }

    #[test]
    fn time_signature_sets_the_bar_length() {
        let mut analyzer = BarAnalyzer::new(SR);
        let mut reports = Vec::new();
        // 6/8 at 120 BPM: a bar is 3 quarter notes = 1.5 s. Feed 3 s → bars 1 and 2.
        for k in 0..((3.0 * SR) as usize / 600) {
            let beat = k as f64 * 600.0 * 2.0 / SR as f64;
            let t = Transport { playing: true, beat: Some(beat), tempo: Some(120.0), time_signature: Some((6, 8)) };
            analyzer.process(&[0.1; 600], &[0.1; 600], &t, &mut |r| reports.push(r));
        }
        assert_eq!(reports.iter().map(|r| (r.bar, r.beats)).collect::<Vec<_>>(), [(1, 3.0), (2, 3.0)]);
        assert_eq!(reports[1].start_beat, 3.0);
    }
}
