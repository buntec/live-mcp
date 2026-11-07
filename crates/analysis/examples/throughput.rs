//! How much of one CPU core `BarAnalyzer` and `BeatAnalyzer` need:
//! `cargo run --release --example throughput`.

use std::time::Instant;

use live_mcp_analysis::{BarAnalyzer, BeatAnalyzer, Transport};

fn main() {
    let sample_rate = 48_000.0;
    let seconds = 120.0;
    let block = 256;
    let left: Vec<f32> = (0..block).map(|i| (i as f32 * 0.031).sin() * 0.4).collect();
    let right: Vec<f32> = (0..block).map(|i| (i as f32 * 0.047).sin() * 0.4).collect();
    let blocks = (seconds * sample_rate as f64) as usize / block;
    let transport = |k: usize| {
        let beat = (k * block) as f64 * 120.0 / 60.0 / sample_rate as f64;
        Transport { playing: true, beat: Some(beat), tempo: Some(120.0), time_signature: Some((4, 4)) }
    };
    let report = |what: &str, elapsed: f64, reports: usize| {
        println!(
            "{what}: {seconds} s of stereo audio at {sample_rate} Hz in {:.1} ms: {:.2}% of one core, {reports} reports",
            elapsed * 1e3,
            elapsed / seconds * 100.0
        )
    };

    let mut bars = BarAnalyzer::new(sample_rate);
    let mut reports = 0;
    let start = Instant::now();
    for k in 0..blocks {
        bars.process(&left, &right, &transport(k), &mut |_| reports += 1);
    }
    report("bars", start.elapsed().as_secs_f64(), reports);

    let mut beats = BeatAnalyzer::new(sample_rate);
    let mut reports = 0;
    let start = Instant::now();
    for k in 0..blocks {
        beats.process(&left, &right, &transport(k), &mut |_| reports += 1);
    }
    report("beats", start.elapsed().as_secs_f64(), reports);
}
