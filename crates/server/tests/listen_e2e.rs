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

/// What the default fake plugin hears: a 220 Hz sine at amplitude 0.25 (−12 dBFS).
const TONE_220: &[(f64, f64)] = &[(220.0, -12.04)];

/// One bar of the sum of sines (Hz, dBFS), as the real analyzer would report it.
fn synthetic_bar(bar: i64, partials: &[(f64, f64)]) -> BarReport {
    let sr = 48_000.0;
    let mut analyzer = BarAnalyzer::new(sr as f32);
    let samples: Vec<f32> = (0..96_000)
        .map(|i| {
            partials.iter().map(|(f, db)| 10f64.powf(db / 20.0) * (2.0 * PI * f * i as f64 / sr).sin()).sum::<f64>()
                as f32
        })
        .collect();
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
async fn fake_plugin(
    state: PathBuf,
    live: Bridge,
    tag_param: &'static str,
    instance: u64,
    partials: &'static [(f64, f64)],
) {
    let token = std::fs::read_to_string(state.join("token")).unwrap().trim().to_owned();
    let endpoint: EarEndpoint =
        serde_json::from_str(&std::fs::read_to_string(state.join("ear.json")).unwrap()).unwrap();
    let mut socket = TcpStream::connect(("127.0.0.1", endpoint.port)).await.unwrap();
    let hello = EarMessage::Hello {
        token,
        protocol: EAR_PROTOCOL_VERSION,
        instance,
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
                send(&mut socket, &EarMessage::Bar { tag, report: synthetic_bar(bar, partials) }).await;
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
        99,
        TONE_220,
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
    // Each listen is kept as a snapshot; a snapshot compared with itself shows no differences.
    let snapshot = heard["snapshot"].as_str().unwrap().to_owned();
    let same = h.ok("compare", json!({"a": snapshot, "b": snapshot})).await;
    assert_eq!(same["loudness_lufs"]["delta"], 0.0);
    assert!(same["notes"][0].as_str().unwrap().contains("no large differences"), "{same}");
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

#[tokio::test]
async fn listens_to_several_tracks_and_finds_shared_bands() {
    let Some(h) = Harness::start().await else { return };
    const BASS: &[(f64, f64)] = &[(70.0, -12.0), (220.0, -26.0)];
    const KICK: &[(f64, f64)] = &[(55.0, -13.0), (3000.0, -30.0)];
    const BOTH: &[(f64, f64)] = &[(70.0, -12.0), (220.0, -26.0), (55.0, -13.0), (3000.0, -30.0)];
    let ear = json!(["plugins", "live-mcp", "LiveMCP Ear"]);
    for track in [json!(0), json!(1), json!("master")] {
        h.ok("load_device", json!({"track": track, "path": ear})).await;
    }
    // Track 0 has Drift first, so its Ear is device 1.
    let mut plugins = Vec::new();
    for (param, instance, partials) in [
        ("live_set tracks 0 devices 1 parameters 1 value", 1, BASS),
        ("live_set tracks 1 devices 0 parameters 1 value", 2, KICK),
        ("live_set master_track devices 0 parameters 1 value", 3, BOTH),
    ] {
        plugins.push(tokio::spawn(fake_plugin(h.state_dir(), h.fake.connect().await, param, instance, partials)));
    }

    let heard = h.ok("listen", json!({"tracks": ["all"], "start_bar": 1, "bars": 2})).await;
    let tracks = heard["tracks"].as_array().unwrap();
    let names: Vec<&str> = tracks.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["1-MIDI", "2-Audio", "Main"], "{heard}");
    assert!(tracks.iter().all(|t| t["volume"] == "0.0 dB" && t["bars"].as_array().unwrap().len() == 2), "{heard}");
    let snapshots: Vec<&str> = tracks.iter().map(|t| t["snapshot"].as_str().unwrap()).collect();
    assert_eq!(snapshots.len(), 3, "one snapshot per track");
    let contested: Vec<&str> =
        heard["masking"]["contested"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    assert_eq!(contested.len(), 1, "{contested:?}");
    assert!(
        contested[0].starts_with("low (<120 Hz):")
            && contested[0].contains("1-MIDI")
            && contested[0].contains("2-Audio")
    );
    let low = &heard["masking"]["bands"][0]["tracks"];
    assert!(low.as_array().unwrap().iter().all(|t| t["name"] != "Main"), "the master Ear is not a voice: {low}");

    // Pull the kick's fader down 20 dB: the Ear hears it unchanged, but in the mix the bass now
    // carries the low band alone.
    h.ok("set", json!({"target": "live_set tracks 1 mixer_device volume", "prop": "value", "value": 0.085})).await;
    let heard = h.ok("listen", json!({"tracks": [0, 1], "start_bar": 3, "bars": 1})).await;
    assert_eq!(heard["tracks"][1]["volume"], "-20.0 dB");
    assert_eq!(heard["masking"]["contested"], json!([]), "{}", heard["masking"]);

    // Solo the kick: the bass is silent in the mix, so nothing is contested.
    h.ok("set", json!({"target": "live_set tracks 1 mixer_device volume", "prop": "value", "value": 0.85})).await;
    h.ok("set", json!({"target": "live_set tracks 1", "prop": "solo", "value": true})).await;
    let heard = h.ok("listen", json!({"tracks": [0, 1], "start_bar": 3, "bars": 1})).await;
    assert_eq!(heard["tracks"][1]["solo"], true, "{heard}");
    assert_eq!(heard["masking"]["contested"], json!([]), "{}", heard["masking"]);
    assert_eq!(
        heard["masking"]["excluded"],
        json!([{"track": 0, "name": "1-MIDI", "why": "another track is soloed"}]),
        "{}",
        heard["masking"]
    );
    h.ok("set", json!({"target": "live_set tracks 1", "prop": "solo", "value": false})).await;

    // A single-track list still gets the multi-track shape, without a masking analysis.
    let heard = h.ok("listen", json!({"tracks": ["master"], "start_bar": 1, "bars": 1})).await;
    assert!(heard.get("masking").is_none() && heard["masking_note"].is_string());
    let e = h.err("listen", json!({"tracks": [0, "Nope"]})).await;
    assert!(e.contains("no track named"), "{e}");
    for plugin in plugins {
        plugin.abort();
    }
}
