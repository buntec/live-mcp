//! `listen` and `ears` end to end: an MCP client, `live-mcp serve`, the fake Live, and a
//! scripted stand-in for the LiveMCP Ear plugin that speaks the real plugin protocol.

mod common;

use std::f64::consts::PI;
use std::path::PathBuf;
use std::time::Duration;

use common::Harness;
use live_mcp_analysis::{BarAnalyzer, BarReport, BeatAnalyzer, BeatReport, Transport};
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

/// One beat of the sum of sines, as the real analyzer would report it. The sound is steady, so
/// every beat reads the same apart from its number.
fn synthetic_beat(partials: &[(f64, f64)]) -> BeatReport {
    let sr = 48_000.0;
    let mut analyzer = BeatAnalyzer::new(sr as f32);
    let samples: Vec<f32> = (0..72_000)
        .map(|i| {
            partials.iter().map(|(f, db)| 10f64.powf(db / 20.0) * (2.0 * PI * f * i as f64 / sr).sin()).sum::<f64>()
                as f32
        })
        .collect();
    let mut reports = Vec::new();
    let t = Transport { playing: true, beat: Some(0.0), tempo: Some(120.0), time_signature: Some((4, 4)) };
    analyzer.process(&samples, &samples, &t, &mut |r| reports.push(r));
    reports.into_iter().find(|r| r.beat == 1).expect("beat 1 of 3")
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
    let beat_report = synthetic_beat(partials);
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
                for beat in (bar - 1) * 4..bar * 4 {
                    send(&mut socket, &EarMessage::Beat { tag, report: BeatReport { beat, ..beat_report } }).await;
                }
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
    // A single A is no chord and no key, but its pitch class, register and attack are heard.
    assert_eq!(bar["chord"], "N", "{bar}");
    assert_eq!(bar["pcs"], "A 1.0", "{bar}");
    // 220 Hz is A2 in Live; the bass tracker hears it held through the bar.
    assert_eq!(bar["range"], "A2", "{bar}");
    assert_eq!(bar["bass_line"], "A2 - - -  - - - -  - - - -  - - - -", "{bar}");
    assert!(bar["tonal"].as_f64().unwrap() > 0.9, "{bar}");
    assert_eq!(heard["music"]["key"], Value::Null, "{heard}");
    assert_eq!(heard["music"]["scale"], json!({"set": "C Major", "outside": []}));
    // focus "mix" leaves the music out, focus "music" the mix measurements.
    let mix = h.ok("listen", json!({"start_bar": 20, "bars": 1, "focus": "mix"})).await;
    assert!(mix.get("music").is_none() && mix["bars"][0].get("chord").is_none(), "{mix}");
    let music = h.ok("listen", json!({"start_bar": 30, "bars": 1, "focus": "music"})).await;
    assert!(music.get("summary").is_none() && music["bars"][0].get("lufs").is_none(), "{music}");
    assert_eq!(music["bars"][0]["pcs"], "A 1.0", "{music}");
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
    // A new start bar: the fake plugin reports when playback starts somewhere new.
    let heard = h.ok("listen", json!({"tracks": [0, 1], "start_bar": 4, "bars": 1})).await;
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

#[tokio::test]
async fn hears_chords_and_clashes_between_tracks() {
    let Some(h) = Harness::start().await else { return };
    // An F major triad on one track, F# on the other: the F and F# rub a semitone apart in the
    // same octave. (In F major the F# is spelled Gb.)
    const PAD: &[(f64, f64)] = &[(174.61, -18.0), (220.0, -18.0), (261.63, -18.0)];
    const RUB: &[(f64, f64)] = &[(185.0, -18.0)];
    let ear = json!(["plugins", "live-mcp", "LiveMCP Ear"]);
    for track in [0, 1] {
        h.ok("load_device", json!({"track": track, "path": ear})).await;
    }
    let mut plugins = Vec::new();
    for (param, instance, partials) in [
        ("live_set tracks 0 devices 1 parameters 1 value", 1, PAD),
        ("live_set tracks 1 devices 0 parameters 1 value", 2, RUB),
    ] {
        plugins.push(tokio::spawn(fake_plugin(h.state_dir(), h.fake.connect().await, param, instance, partials)));
    }
    let heard = h.ok("listen", json!({"tracks": [0, 1], "start_bar": 1, "bars": 2, "focus": "music"})).await;
    let pad = &heard["tracks"][0];
    assert_eq!(pad["music"]["progression"], "F | F", "{pad}");
    assert_eq!(pad["bars"][0]["chord"], "F", "{pad}");
    let together = &heard["music"];
    assert_eq!(together["tracks"], json!(["1-MIDI", "2-Audio"]), "{together}");
    // The F# is spelled by the key the two make together, which is no clear key: either way.
    let clashes = together["clashes"].as_array().unwrap();
    assert_eq!(clashes.len(), 1, "{together}");
    assert!(["F against Gb", "F against F#"].contains(&clashes[0]["notes"].as_str().unwrap()), "{together}");
    assert_eq!(clashes[0]["tracks"], json!(["1-MIDI", "2-Audio"]));
    assert_eq!((clashes[0]["bars"].clone(), clashes[0]["beats"].clone()), (json!("1–2"), json!(8)));
    assert!(["Gb", "F#"].contains(&together["scale"]["outside"][0]["pc"].as_str().unwrap()), "{together}");
    assert!(heard["tracks"][0]["music"].get("scale").is_none(), "scale fit is only in the combined music");
    assert_eq!(together["rhythm"]["density"], 0.0, "steady tones have no onsets: {together}");
    assert!(heard.get("masking").is_none(), "focus music leaves masking out");
    for plugin in plugins {
        plugin.abort();
    }
}

#[tokio::test]
async fn records_every_listen_without_replacing_earlier_sessions() {
    let Some(first) = Harness::start().await else { return };
    let Some(second) = Harness::start().await else { return };
    // Both servers record into the first one's folder, as a restarted server would.
    let dir = first.state_dir().join("recordings");
    for (h, bars) in [(&first, [1, 2]), (&second, [20, 21])] {
        let config = json!({"port": 0, "ear_port": 0, "record_beats": dir});
        std::fs::write(h.state_dir().join("config.json"), config.to_string()).unwrap();
        h.ok("load_device", json!({"track": "master", "path": ["plugins", "live-mcp", "LiveMCP Ear"]})).await;
        let param = "live_set master_track devices 0 parameters 1 value";
        let plugin = tokio::spawn(fake_plugin(h.state_dir(), h.fake.connect().await, param, 7, TONE_220));
        let one = h.ok("listen", json!({"start_bar": bars[0], "bars": 1})).await;
        let several = h.ok("listen", json!({"tracks": ["master"], "start_bar": bars[1], "bars": 1})).await;
        assert_eq!((one["snapshot"].as_str(), several["tracks"][0]["snapshot"].as_str()), (Some("s1"), Some("s2")));
        // listen names the recording it made, so a caller can tie it to what was played.
        let recording = one["recording"].as_str().unwrap();
        assert!(recording.ends_with("-s1") && dir.join(format!("{recording}.json")).exists(), "{one}");
        assert!(several["tracks"][0]["recording"].as_str().unwrap().ends_with("-s2"), "{several}");
        plugin.abort();
    }
    let mut recorded: Vec<(String, i64)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
            (name, body["bars"][0]["bar"].as_i64().unwrap())
        })
        .collect();
    recorded.sort();
    let bars: Vec<i64> = recorded.iter().map(|(_, bar)| *bar).collect();
    assert_eq!(bars, [1, 2, 20, 21], "{recorded:?}");
    let session = |name: &str| name.split_once('-').unwrap().0.to_owned();
    assert_eq!(session(&recorded[0].0), session(&recorded[1].0));
    assert_ne!(session(&recorded[0].0), session(&recorded[2].0), "{recorded:?}");
    assert!(recorded.iter().all(|(name, _)| name.ends_with("-s1") || name.ends_with("-s2")), "{recorded:?}");
}

/// Routes `track`'s output to the routing type Live lists under `name`.
async fn route(h: &Harness, track: usize, name: &str) {
    let types = h
        .ok("get", json!({"target": format!("live_set tracks {track} available_output_routing_types"), "fields": ["display_name"]}))
        .await;
    let index = types
        .as_array()
        .unwrap()
        .iter()
        .position(|t| t["fields"]["display_name"] == name)
        .unwrap_or_else(|| panic!("no output {name:?} in {types}"));
    let value = json!({"$ref": format!("live_set tracks {track} available_output_routing_types {index}")});
    h.ok("set", json!({"target": format!("live_set tracks {track}"), "prop": "output_routing_type", "value": value}))
        .await;
}

/// The names in a masking analysis's contested low band, or none.
fn contested_low(heard: &Value) -> Vec<String> {
    let contested = heard["masking"]["contested"].as_array().unwrap();
    let Some(low) = contested.iter().filter_map(Value::as_str).find(|c| c.starts_with("low")) else { return vec![] };
    ["Drums", "Kick", "Snare", "Bass", "Bus", "Source"]
        .iter()
        .filter(|n| low.contains(*n))
        .map(|n| n.to_string())
        .collect()
}

#[tokio::test]
async fn masking_follows_groups_routing_and_solo() {
    let Some(h) = Harness::start().await else { return };
    // Source (the MIDI track), Drums [Kick, Snare], Bass, and Bus, an ordinary track that
    // Source can be routed into.
    for _ in 0..3 {
        h.ok("call", json!({"target": "live_set", "method": "create_audio_track", "args": []})).await;
    }
    h.ok("call", json!({"target": "live_set", "method": "fake_group_tracks", "args": [1, 2]})).await;
    for (k, name) in ["Source", "Drums", "Kick", "Snare", "Bass", "Bus"].iter().enumerate() {
        h.ok("set", json!({"target": format!("live_set tracks {k}"), "prop": "name", "value": name})).await;
    }
    // Every Ear hears the same low tone: a tie in the low band whenever two of them count.
    const LOW: &[(f64, f64)] = &[(60.0, -12.0)];
    let ear = json!(["plugins", "live-mcp", "LiveMCP Ear"]);
    let mut plugins = Vec::new();
    for (k, track) in [1usize, 2, 4, 5].into_iter().enumerate() {
        h.ok("load_device", json!({"track": track, "path": ear})).await;
        let param: &'static str = Box::leak(format!("live_set tracks {track} devices 0 parameters 1 value").into());
        plugins.push(tokio::spawn(fake_plugin(h.state_dir(), h.fake.connect().await, param, 10 + k as u64, LOW)));
    }
    // Each listen starts from a new bar: the fake plugin reports when playback starts or jumps.
    let bar = std::cell::Cell::new(0);
    let listen = |tracks: Value| {
        let (h, start_bar) = (&h, bar.replace(bar.get() + 1) + 1);
        async move { h.ok("listen", json!({"tracks": tracks, "start_bar": start_bar, "bars": 1})).await }
    };
    let excluded = |heard: &Value, name: &str| -> Option<String> {
        heard["masking"]["excluded"].as_array()?.iter().find(|e| e["name"] == name)?["why"].as_str().map(str::to_owned)
    };

    // A muted Kick must not take the place of Drums, which still carries the Snare.
    let heard = listen(json!(["Drums", "Bass"])).await;
    assert_eq!(contested_low(&heard), ["Drums", "Bass"], "{}", heard["masking"]);
    h.ok("set", json!({"target": "live_set tracks 2", "prop": "mute", "value": true})).await;
    let heard = listen(json!(["Drums", "Kick", "Bass"])).await;
    assert_eq!(contested_low(&heard), ["Drums", "Bass"], "{}", heard["masking"]);
    assert_eq!(excluded(&heard, "Kick").as_deref(), Some("muted"));
    assert_eq!(heard["tracks"][1]["group"], "Drums");
    // With Kick playing, its Ear covers only part of Drums (the Snare has none): Drums counts,
    // and Kick, already in it, does not.
    h.ok("set", json!({"target": "live_set tracks 2", "prop": "mute", "value": false})).await;
    let heard = listen(json!(["Drums", "Kick", "Bass"])).await;
    assert_eq!(contested_low(&heard), ["Drums", "Bass"], "{}", heard["masking"]);
    assert_eq!(
        excluded(&heard, "Kick").as_deref(),
        Some("its audio is counted in the Ear on Drums, which it plays through")
    );
    // With the Snare muted, Kick's Ear covers all of Drums and counts instead.
    h.ok("set", json!({"target": "live_set tracks 3", "prop": "mute", "value": true})).await;
    let heard = listen(json!(["Drums", "Kick", "Bass"])).await;
    assert_eq!(contested_low(&heard), ["Kick", "Bass"], "{}", heard["masking"]);
    assert!(excluded(&heard, "Drums").unwrap().contains("their own Ears"), "{}", heard["masking"]);
    h.ok("set", json!({"target": "live_set tracks 3", "prop": "mute", "value": false})).await;

    // The group fader counts only for audio that passes through the group.
    h.ok("set", json!({"target": "live_set tracks 1 mixer_device volume", "prop": "value", "value": 0.085})).await;
    let heard = listen(json!(["Kick", "Bass"])).await;
    assert_eq!(contested_low(&heard), Vec::<String>::new(), "Kick is 20 dB down through Drums: {}", heard["masking"]);
    route(&h, 2, "Main").await;
    let heard = listen(json!(["Kick", "Bass"])).await;
    assert_eq!(heard["tracks"][0]["output"], "Main", "{heard}");
    assert_eq!(contested_low(&heard), ["Kick", "Bass"], "{}", heard["masking"]);
    let shares: Vec<f64> = heard["masking"]["bands"][0]["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["share_db"].as_f64().unwrap())
        .collect();
    assert!(shares.iter().all(|s| (s + 3.0).abs() < 0.5), "{shares:?}");
    // Played straight to Main, Kick no longer overlaps Drums: both count.
    let heard = listen(json!(["Drums", "Kick"])).await;
    assert!(heard["masking"].get("excluded").is_none(), "{}", heard["masking"]);
    route(&h, 2, "Group").await;
    h.ok("set", json!({"target": "live_set tracks 1 mixer_device volume", "prop": "value", "value": 0.85})).await;

    // A bus that a soloed track plays into stays audible.
    route(&h, 0, "Bus").await;
    for track in [0, 4] {
        h.ok("set", json!({"target": format!("live_set tracks {track}"), "prop": "solo", "value": true})).await;
    }
    let heard = listen(json!(["Bus", "Bass", "Drums"])).await;
    assert_eq!(contested_low(&heard), ["Bass", "Bus"], "{}", heard["masking"]);
    assert_eq!(excluded(&heard, "Drums").as_deref(), Some("another track is soloed"));
    for track in [0, 4] {
        h.ok("set", json!({"target": format!("live_set tracks {track}"), "prop": "solo", "value": false})).await;
    }
    // Into a track that goes nowhere: left out, with where it goes.
    route(&h, 5, "Sends Only").await;
    let heard = listen(json!(["Bus", "Bass"])).await;
    assert_eq!(excluded(&heard, "Bus").as_deref(), Some("its output goes to Sends Only, not Main"));
    for plugin in plugins {
        plugin.abort();
    }
}
