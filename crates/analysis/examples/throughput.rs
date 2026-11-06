//! How much of one CPU core `BarAnalyzer` needs: `cargo run --release --example throughput`.

use std::time::Instant;

use live_mcp_analysis::{BarAnalyzer, Transport};

fn main() {
    let sample_rate = 48_000.0;
    let seconds = 120.0;
    let block = 256;
    let left: Vec<f32> = (0..block).map(|i| (i as f32 * 0.031).sin() * 0.4).collect();
    let right: Vec<f32> = (0..block).map(|i| (i as f32 * 0.047).sin() * 0.4).collect();
    let mut analyzer = BarAnalyzer::new(sample_rate);
    let blocks = (seconds * sample_rate as f64) as usize / block;
    let mut reports = 0;
    let start = Instant::now();
    for k in 0..blocks {
        let beat = (k * block) as f64 * 120.0 / 60.0 / sample_rate as f64;
        let t = Transport { playing: true, beat: Some(beat), tempo: Some(120.0), time_signature: Some((4, 4)) };
        analyzer.process(&left, &right, &t, &mut |_| reports += 1);
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{seconds} s of stereo audio at {sample_rate} Hz in {:.1} ms: {:.2}% of one core, {reports} bars",
        elapsed * 1e3,
        elapsed / seconds * 100.0
    );
}
