//! Per-block work, independent of the plugin framework: measure bars and beats, and emit
//! heartbeats.

use live_mcp_analysis::{BarAnalyzer, BarReport, BeatAnalyzer, BeatReport, Transport};
use live_mcp_protocol::ear::EarMessage;

/// Heartbeats per second. They tell the server the instance is alive and which tag it has.
const HEARTBEATS_PER_SECOND: f64 = 4.0;

/// Work for the background thread. Plain data with no heap allocations, so creating one on the
/// audio thread is realtime-safe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EarTask {
    Bar { tag: u32, report: BarReport },
    Beat { tag: u32, report: BeatReport },
    Heartbeat { tag: u32, playing: bool, beat: Option<f64> },
}

impl EarTask {
    pub fn into_message(self) -> EarMessage {
        match self {
            EarTask::Bar { tag, report } => EarMessage::Bar { tag, report },
            EarTask::Beat { tag, report } => EarMessage::Beat { tag, report },
            EarTask::Heartbeat { tag, playing, beat } => EarMessage::Heartbeat { tag, playing, beat },
        }
    }
}

pub struct EarCore {
    analyzer: BarAnalyzer,
    beats: BeatAnalyzer,
    heartbeat_every: usize,
    since_heartbeat: usize,
    last_tag: Option<u32>,
}

impl EarCore {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            analyzer: BarAnalyzer::new(sample_rate),
            beats: BeatAnalyzer::new(sample_rate),
            heartbeat_every: (sample_rate as f64 / HEARTBEATS_PER_SECOND) as usize,
            since_heartbeat: usize::MAX, // send one straight away
            last_tag: None,
        }
    }

    pub fn reset(&mut self) {
        self.analyzer.reset();
        self.beats.reset();
        self.since_heartbeat = usize::MAX;
    }

    /// Processes one block. Does not allocate.
    pub fn process(
        &mut self,
        left: &[f32],
        right: &[f32],
        transport: &Transport,
        tag: u32,
        emit: &mut impl FnMut(EarTask),
    ) {
        self.analyzer.process(left, right, transport, &mut |report| emit(EarTask::Bar { tag, report }));
        self.beats.process(left, right, transport, &mut |report| emit(EarTask::Beat { tag, report }));
        self.since_heartbeat = self.since_heartbeat.saturating_add(left.len());
        if self.since_heartbeat >= self.heartbeat_every || self.last_tag != Some(tag) {
            self.since_heartbeat = 0;
            self.last_tag = Some(tag);
            emit(EarTask::Heartbeat { tag, playing: transport.playing, beat: transport.beat });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeats_on_start_on_schedule_and_on_tag_change() {
        let mut core = EarCore::new(48_000.0);
        let stopped = Transport::default();
        let block = [0.0f32; 480]; // 10 ms
        let mut tasks = Vec::new();
        for _ in 0..100 {
            core.process(&block, &block, &stopped, 0, &mut |t| tasks.push(t));
        }
        // One second: an immediate heartbeat, then one every 250 ms.
        let beats = tasks.iter().filter(|t| matches!(t, EarTask::Heartbeat { .. })).count();
        assert_eq!(beats, 4);
        tasks.clear();
        core.process(&block, &block, &stopped, 7, &mut |t| tasks.push(t));
        assert_eq!(tasks, [EarTask::Heartbeat { tag: 7, playing: false, beat: None }]);
    }

    #[test]
    fn bars_carry_the_current_tag() {
        let mut core = EarCore::new(48_000.0);
        let mut bars = Vec::new();
        let mut beats = Vec::new();
        let block = [0.1f32; 4_800];
        for k in 0..20 {
            // 240 BPM: a 4/4 bar lasts 1 s, 10 blocks.
            let t = Transport {
                playing: true,
                beat: Some(k as f64 * 0.4),
                tempo: Some(240.0),
                time_signature: Some((4, 4)),
            };
            core.process(&block, &block, &t, 3, &mut |task| match task {
                EarTask::Bar { tag, report } => bars.push((tag, report.bar)),
                EarTask::Beat { tag, report } => beats.push((tag, report.beat)),
                EarTask::Heartbeat { .. } => {}
            });
        }
        assert_eq!(bars, [(3, 1), (3, 2)]);
        // Each beat once its spectrum frames are in; the last one or two are still open.
        assert_eq!(beats[..6], [(3, 0), (3, 1), (3, 2), (3, 3), (3, 4), (3, 5)]);
    }
}
