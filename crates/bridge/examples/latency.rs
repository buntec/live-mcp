//! Times requests to a running LiveMCP script: `cargo run --release --example latency`.

use std::time::Instant;

use live_mcp_bridge::{Bridge, GetOptions, Options};
use live_mcp_protocol::{BatchOp, Target};
use serde_json::json;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let t = Instant::now();
    let live = Bridge::connect(&Options::default()).await?;
    println!("connect + hello             {:>7.1} ms", ms(t));
    let info = live.info().await?;
    println!("script {} on Live {}, linger: {}", info["script_version"], info["live_version"], info["linger"]);

    let tempo = Target::path("live_set tempo");
    let mut samples = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        live.get(&tempo, GetOptions::default()).await?;
        samples.push(ms(t));
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "back-to-back get, each      median {:>6.1} ms  min {:>6.1}  max {:>6.1}",
        samples[10], samples[0], samples[19]
    );

    let mut samples = Vec::new();
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(237)).await;
        let t = Instant::now();
        live.get(&tempo, GetOptions::default()).await?;
        samples.push(ms(t));
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "isolated get, after idle    median {:>6.1} ms  min {:>6.1}  max {:>6.1}",
        samples[10], samples[0], samples[19]
    );

    let t = Instant::now();
    let requests: Vec<_> = (0..50).map(|_| live.get(&tempo, GetOptions::default())).collect();
    for result in futures::future::join_all(requests).await {
        result?;
    }
    println!("50 pipelined gets           {:>7.1} ms total", ms(t));

    let ops: Vec<BatchOp> =
        (0..50).map(|_| BatchOp { method: "get".into(), params: json!({"target": "live_set tempo"}) }).collect();
    let t = Instant::now();
    live.batch(&ops, false, true).await?;
    println!("batch of 50 gets            {:>7.1} ms total", ms(t));

    let status = live.status().await?;
    println!("tick rate                   {:>7.2} Hz", status["tick_rate"].as_f64().unwrap_or_default());
    Ok(())
}
