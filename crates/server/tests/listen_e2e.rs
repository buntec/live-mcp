//! `listen` and `ears` end to end: an MCP client, `live-mcp serve`, the fake Live, and a
//! scripted stand-in for the LiveMCP Ear plugin that speaks the real plugin protocol.

mod common;

use std::f64::consts::PI;
use std::path::PathBuf;
use std::time::Duration;

use common::Harness;
use live_mcp_analysis::{BarAnalyzer, BarReport, Transport};
use live_mcp_bridge::{Bridge, GetOptions};
use live_mcp_protocol::Target;
use live_mcp_protocol::ear::{EAR_PROTOCOL_VERSION, EarEndpoint, EarMessage, TAG_MAX};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// A bar of a 220 Hz sine at amplitude 0.25, as the real analyzer would report it.
fn synthetic_bar(bar: i64) -> BarReport {
    let sr = 48_000.0;
    let mut analyzer = BarAnalyzer::new(sr as f32);
    let samples: Vec<f32> = (0..96_000).map(|i| (0.25 * (2.0 * PI * 220.0 * i as f64 / sr).sin()) as f32).collect();
    let mut out = None;
    let start = (bar - 1) as f64 * 4.0;
    let t = Transport { playing: true, beat: Some(start), tempo: Some(120.0), time_signature: Some((4, 4)) };
    analyzer.process(&samples, &samples, &t, &mut |r| out = Some(r));
    out.expect("a full bar")
}

async fn send(socket: &mut TcpStream, message: &EarMessage) {
    let mut line = serde_json::to_vec(message).unwrap();
    line.push(b'\n');
    socket.write_all(&line).await.unwrap();
}

/// Plays the plugin's part: reads its Tag parameter and the transport from the fake Live (as a
/// host would pass them), sends heartbeats, and reports eight bars whenever playback starts.
async fn fake_plugin(state: PathBuf, live: Bridge, tag_param: &'static str) {
    let token = std::fs::read_to_string(state.join("token")).unwrap().trim().to_owned();
    let endpoint: EarEndpoint =
        serde_json::from_str(&std::fs::read_to_string(state.join("ear.json")).unwrap()).unwrap();
    let mut socket = TcpStream::connect(("127.0.0.1", endpoint.port)).await.unwrap();
    let hello = EarMessage::Hello {
        token,
        protocol: EAR_PROTOCOL_VERSION,
        instance: 99,
        plugin_version: "test".into(),
        sample_rate: 48_000.0,
    };
    send(&mut socket, &hello).await;
    let mut reported: Option<f64> = None;
    loop {
        let value = live.get(&Target::path(tag_param), GetOptions::default()).await.unwrap();
        let tag = (value.as_f64().unwrap() * TAG_MAX as f64).round() as u32;
        let song =
            live.get(&Target::path("live_set"), GetOptions::props(["is_playing", "current_song_time"])).await.unwrap();
        let playing = song["is_playing"] == true;
        let beat = song["current_song_time"].as_f64();
        send(&mut socket, &EarMessage::Heartbeat { tag, playing, beat }).await;
        // Report eight bars from wherever playback starts or jumps to. (The fake Live's clock
        // does not advance, so a changed position while playing is a jump.)
        let from = if playing { beat } else { None };
        if let Some(start) = from
            && from != reported
        {
            let first = (start / 4.0).floor() as i64 + 1;
            for bar in first..first + 8 {
                send(&mut socket, &EarMessage::Bar { tag, report: synthetic_bar(bar) }).await;
            }
        }
        reported = from;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn listen_through_an_ear_on_the_master_track() {
    let Some(h) = Harness::start().await else { return };

    let e = h.err("listen", json!({})).await;
    assert!(e.contains("no LiveMCP Ear on track \"master\""), "{e}");
    let ears = h.ok("ears", json!({})).await;
    assert_eq!(ears["ears"], json!([]));
    assert!(ears["hint"].as_str().unwrap().contains("load_device"));

    h.ok("load_device", json!({"track": "master", "path": ["plugins", "live-mcp", "LiveMCP Ear"]})).await;
    let ears = h.ok("ears", json!({})).await;
    assert_eq!(ears["ears"], json!([{"track": "master", "device": 0, "tag": 1, "connected": false}]));
    let e = h.err("listen", json!({})).await;
    assert!(e.contains("not connected"), "{e}");

    // Now the plugin reports in. It reads the tag the server wrote into its parameter.
    let plugin = tokio::spawn(fake_plugin(
        h.state_dir(),
        h.fake.connect().await,
        "live_set master_track devices 0 parameters 1 value",
    ));
    let ears = h.ok("ears", json!({})).await;
    assert_eq!(ears["ears"][0]["connected"], true, "{ears}");
    assert_eq!(ears["ears"][0]["tag"], 1);

    // Bar 10 is beyond the eight bars the fake reports from position 0, so this only passes if
    // listen moved the song position before starting playback.
    let heard = h.ok("listen", json!({"start_bar": 10, "bars": 2})).await;
    let bars: Vec<i64> = heard["bars"].as_array().unwrap().iter().map(|b| b["bar"].as_i64().unwrap()).collect();
    assert_eq!(bars, [10, 11], "{heard}");
    let bar = &heard["bars"][0];
    // A 0.25 sine: peak −12.0 dBFS, RMS −15.1; 220 Hz sits in the low-mid band.
    assert_eq!((bar["peak"].clone(), bar["rms"].clone()), (json!(-12.0), json!(-15.1)), "{bar}");
    assert!(bar["bands"]["low_mid"].as_f64().unwrap() > -17.0, "{bar}");
    assert_eq!(bar["correlation"], 1.0);
    assert!(bar.get("coverage").is_none(), "full bars carry no coverage field");
    assert_eq!(heard["summary"]["peak"], -12.0);
    assert!(heard.get("missing_bars").is_none());

    // Straight after a listen that stopped playback, Live still reports is_playing for a tick;
    // a second listen must still start playback from its own bar.
    let again = h.ok("listen", json!({"start_bar": 2, "bars": 1})).await;
    assert_eq!(again["bars"][0]["bar"], 2, "{again}");

    // listen started playback, so it stopped it again.
    let state = h.ok("transport", json!({})).await;
    assert_eq!(state["playing"], false, "{state}");

    // A second Ear: tags stay unique, and the existing one keeps its tag.
    h.ok("load_device", json!({"track": 0, "path": ["plugins", "live-mcp", "LiveMCP Ear"]})).await;
    let ears = h.ok("ears", json!({})).await;
    let tags: Vec<(Value, Value)> =
        ears["ears"].as_array().unwrap().iter().map(|e| (e["track"].clone(), e["tag"].clone())).collect();
    assert_eq!(tags, [(json!(0), json!(2)), (json!("master"), json!(1))]);

    let e = h.err("listen", json!({"bars": 100})).await;
    assert!(e.contains("at most 60"), "{e}");
    plugin.abort();
}
