//! End-to-end: an MCP client drives `live-mcp serve`, which talks to the real Remote Script on a fake Live.
//!
//! Requires `python3` on PATH; the tests are skipped (with a message) when it is missing.

mod common;

use common::Harness;
use serde_json::{Value, json};

#[tokio::test]
async fn lists_all_tools_with_object_schemas() {
    let Some(h) = Harness::start().await else { return };
    let tools = h.client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "analyze_file",
            "batch",
            "browser",
            "call",
            "compare",
            "create_track",
            "describe",
            "device_parameters",
            "ears",
            "get",
            "listen",
            "load_device",
            "read_clip",
            "session",
            "set",
            "set_parameter",
            "snapshots",
            "status",
            "transport",
            "write_clip"
        ]
    );
    for tool in &tools {
        assert_eq!(tool.input_schema.get("type"), Some(&json!("object")), "{} schema", tool.name);
        assert!(tool.description.as_deref().is_some_and(|d| !d.is_empty()), "{} has no description", tool.name);
    }
    let info = h.client.peer_info().expect("server info");
    assert!(info.instructions.as_deref().unwrap_or_default().contains("live_set"));
}

#[tokio::test]
async fn session_overview() {
    let Some(h) = Harness::start().await else { return };
    let s = h.ok("session", json!({})).await;
    assert_eq!(s["tempo"], 120.0);
    assert_eq!(s["time_signature"], "4/4");
    assert_eq!(
        s["tracks"][0],
        json!({"index": 0, "name": "1-MIDI", "kind": "midi", "devices": ["Drift"], "clips": []})
    );
    assert_eq!(s["tracks"][1]["kind"], "audio");
    assert_eq!(s["returns"], json!([{"index": 0, "name": "A-Reverb", "devices": ["Reverb"]}]));
    assert_eq!(s["scenes"], json!(["", "Drop"]));
    assert_eq!(s["master"]["devices"], json!([]));
}

#[tokio::test]
async fn tracks_and_clips_round_trip() {
    let Some(h) = Harness::start().await else { return };
    let created = h.ok("create_track", json!({"kind": "midi", "name": "Bass"})).await;
    assert_eq!(created, json!({"track": 2, "name": "Bass"}));

    let notes = json!([
        {"pitch": 36, "start": 0.0, "duration": 0.5},
        {"pitch": 43, "start": 1.0, "duration": 0.5, "velocity": 90},
        {"pitch": 48, "start": 2.0, "duration": 0.5, "probability": 0.5},
    ]);
    let written = h.ok("write_clip", json!({"track": "Bass", "slot": 0, "name": "Groove", "notes": notes})).await;
    assert_eq!(written, json!({"track": 2, "slot": 0, "name": "Groove", "length": 4.0, "notes_written": 3}));

    let read = h.ok("read_clip", json!({"track": 2, "slot": 0})).await;
    assert_eq!(read["name"], "Groove");
    assert_eq!(read["kind"], "midi");
    assert_eq!(
        read["notes"],
        json!([
            {"pitch": 36, "start": 0.0, "duration": 0.5, "velocity": 100.0},
            {"pitch": 43, "start": 1.0, "duration": 0.5, "velocity": 90.0},
            {"pitch": 48, "start": 2.0, "duration": 0.5, "velocity": 100.0, "probability": 0.5},
        ])
    );

    // Replacing (the default) clears the old notes; replace=false adds to them.
    h.ok("write_clip", json!({"track": 2, "slot": 0, "notes": [{"pitch": 60, "start": 0, "duration": 1}]})).await;
    let read = h.ok("read_clip", json!({"track": 2, "slot": 0})).await;
    assert_eq!(read["notes"].as_array().unwrap().len(), 1);
    h.ok(
        "write_clip",
        json!({"track": 2, "slot": 0, "replace": false, "notes": [{"pitch": 64, "start": 1, "duration": 1}]}),
    )
    .await;
    let read = h.ok("read_clip", json!({"track": 2, "slot": 0, "notes": true})).await;
    assert_eq!(read["notes"].as_array().unwrap().len(), 2);

    // Arrangement clips, with the default length rounded up to whole bars.
    let arranged = h
        .ok(
            "write_clip",
            json!({"track": 0, "arrangement_start": 8, "notes": [{"pitch": 60, "start": 0, "duration": 5}]}),
        )
        .await;
    assert_eq!(arranged["length"], 8.0);
    let read = h.ok("read_clip", json!({"track": 0, "arrangement_index": 0})).await;
    assert_eq!(read["start"], 8.0);
    let s = h.ok("session", json!({"arrangement": true})).await;
    assert_eq!(s["tracks"][0]["arrangement_clips"], json!([{"name": "", "start": 8.0, "length": 8.0}]));
    assert_eq!(s["tracks"][2]["clips"], json!([{"slot": 0, "name": "Groove", "length": 4.0}]));

    // Errors come back as tool errors with useful messages.
    let e = h.err("write_clip", json!({"track": 1, "slot": 0, "notes": []})).await;
    assert!(e.contains("MIDI"), "{e}");
    let e =
        h.err("write_clip", json!({"track": 0, "slot": 0, "notes": [{"pitch": 200, "start": 0, "duration": 1}]})).await;
    assert!(e.contains("pitch"), "{e}");
    let e = h.err("read_clip", json!({"track": "Nope", "slot": 0})).await;
    assert!(e.contains("no track named") && e.contains("Bass"), "{e}");
    let e = h.err("read_clip", json!({"track": 0, "slot": 5})).await;
    assert!(e.contains("no clip"), "{e}");
}

#[tokio::test]
async fn browser_listing_search_and_loading() {
    let Some(h) = Harness::start().await else { return };
    let roots = h.ok("browser", json!({})).await;
    assert!(roots["roots"].as_array().unwrap().contains(&json!("instruments")));
    assert!(!roots["roots"].as_array().unwrap().contains(&json!("clips")), "absent roots are left out");

    let folder = h.ok("browser", json!({"path": ["Instruments", "Operator"]})).await;
    let names: Vec<&str> = folder["items"].as_array().unwrap().iter().map(|i| i["path"][2].as_str().unwrap()).collect();
    assert_eq!(names, ["Operator", "Bass"]);

    let found = h.ok("browser", json!({"query": "sub bass"})).await;
    let paths: Vec<Value> = found["items"].as_array().unwrap().iter().map(|i| i["path"].clone()).collect();
    assert!(paths.contains(&json!(["instruments", "Operator", "Bass", "Sub Bass.adg"])), "{paths:?}");
    assert!(paths.contains(&json!(["sounds", "Bass", "Warm Sub Bass.adv"])), "{paths:?}");

    let loaded =
        h.ok("load_device", json!({"track": "return A", "path": ["audio_effects", "Delay & Loop", "Echo"]})).await;
    assert_eq!(loaded["devices"], json!(["Reverb", "Echo"]));
    assert_eq!(loaded["track"], "return 0");
    let e = h.err("load_device", json!({"track": 0, "path": ["audio_effects", "Delay & Loop"]})).await;
    assert!(e.contains("not loadable"), "{e}");
    let e = h.err("load_device", json!({"track": 0, "path": ["audio_effects", "Nope"]})).await;
    assert!(e.contains("Reverb"), "lists what the folder contains: {e}");
}

#[tokio::test]
async fn device_parameters_and_setting_them() {
    let Some(h) = Harness::start().await else { return };
    let params = h.ok("device_parameters", json!({"track": 0, "device": "Drift"})).await;
    assert_eq!(params["device_name"], "Drift");
    let list = params["parameters"].as_array().unwrap();
    assert_eq!(
        list[1],
        json!({"index": 1, "name": "Filter Freq", "value": 0.5, "display": "0.50 kHz", "min": 0.0, "max": 1.0})
    );
    assert_eq!(list[2]["options"], json!(["Sine", "Saw", "Square"]));

    let filtered = h.ok("device_parameters", json!({"track": 0, "device": 0, "query": "osc"})).await;
    assert_eq!(filtered["parameters"].as_array().unwrap().len(), 1);
    assert!(filtered.get("all_names").is_none());
    let none = h.ok("device_parameters", json!({"track": 0, "device": 0, "query": "cutoff"})).await;
    assert_eq!(none["parameters"], json!([]));
    assert_eq!(none["all_names"], json!(["Device On", "Filter Freq", "Osc 1 Shape"]));

    let set = h.ok("set_parameter", json!({"track": 0, "device": 0, "parameter": "Filter Freq", "value": 0.8})).await;
    assert_eq!(set["display"], "0.80 kHz");
    let set = h.ok("set_parameter", json!({"track": 0, "device": "Drift", "parameter": 2, "value": "saw"})).await;
    assert_eq!((set["value"].clone(), set["display"].clone()), (json!(1.0), json!("Saw")));
    let e = h.err("set_parameter", json!({"track": 0, "device": 0, "parameter": 1, "value": 3})).await;
    assert!(e.contains("between 0 and 1"), "{e}");
    let e = h.err("set_parameter", json!({"track": 0, "device": 0, "parameter": 2, "value": "Triangle"})).await;
    assert!(e.contains("Square"), "lists the options: {e}");
}

#[tokio::test]
async fn parameters_of_devices_inside_racks() {
    let Some(h) = Harness::start().await else { return };
    h.ok("load_device", json!({"track": 1, "path": ["instruments", "Instrument Rack"]})).await;
    let rack = h.ok("device_parameters", json!({"track": 1, "device": "Instrument Rack"})).await;
    assert_eq!(rack["parameters"][1]["name"], "Macro 1");
    assert_eq!(
        rack["chains"],
        json!([
            {"index": 0, "name": "Sub", "devices": ["Operator"]},
            {"index": 1, "name": "Pad", "devices": ["Wavetable", "Reverb"]}
        ])
    );
    let plain = h.ok("device_parameters", json!({"track": 0, "device": 0})).await;
    assert!(plain.get("chains").is_none(), "only Racks list chains");

    let reverb = h.ok("device_parameters", json!({"track": 1, "device": ["Instrument Rack", "pad", 1]})).await;
    assert_eq!((reverb["device"].clone(), reverb["device_name"].clone()), (json!([0, 1, 1]), json!("Reverb")));
    let set = h
        .ok("set_parameter", json!({"track": 1, "device": [0, "Pad", "Reverb"], "parameter": "Dry/Wet", "value": 0.6}))
        .await;
    assert_eq!((set["device"].clone(), set["display"].clone()), (json!([0, 1, 1]), json!("0.60")));
    let now = h.ok("get", json!({"target": "live_set tracks 1 devices 0 chains 1 devices 1 parameters 1 value"})).await;
    assert_eq!(now, 0.6);

    let e = h.err("device_parameters", json!({"track": 1, "device": [0, "Pad"]})).await;
    assert!(e.contains("ends at a device"), "{e}");
    let e = h.err("device_parameters", json!({"track": 0, "device": ["Drift", 0, 0]})).await;
    assert!(e.contains("Drift is not a Rack"), "{e}");
    // A chain named like a number is found by its name, as Live's own "808" Rack chains are.
    h.ok("set", json!({"target": "live_set tracks 1 devices 0 chains 1", "prop": "name", "value": "808"})).await;
    let by_name = h.ok("device_parameters", json!({"track": 1, "device": [0, "808", 0]})).await;
    assert_eq!(by_name["device_name"], "Wavetable");
    let by_index = h.ok("device_parameters", json!({"track": 1, "device": [0, "0", 0]})).await;
    assert_eq!(by_index["device_name"], "Operator", "a string that names nothing is an index");
    let e = h.err("device_parameters", json!({"track": 1, "device": [0, "Lead", 0]})).await;
    assert!(e.contains("no chain named") && e.contains("Sub"), "lists the chains: {e}");
}

#[tokio::test]
async fn disabled_parameters_are_marked_and_refused() {
    let Some(h) = Harness::start().await else { return };
    h.ok("load_device", json!({"track": 1, "path": ["instruments", "Instrument Rack"]})).await;
    let operator = h.ok("device_parameters", json!({"track": 1, "device": [0, "Sub", "Operator"]})).await;
    assert_eq!(operator["parameters"][1]["enabled"], false, "a macro controls the filter");
    assert!(operator["parameters"][2].get("enabled").is_none(), "enabled parameters are not marked");
    let e = h
        .err("set_parameter", json!({"track": 1, "device": [0, 0, 0], "parameter": "Filter Freq", "value": 0.2}))
        .await;
    assert!(e.contains("Filter Freq on Operator is disabled") && e.contains("macro"), "{e}");
    h.ok("set_parameter", json!({"track": 1, "device": [0, 0, 0], "parameter": "Osc 1 Shape", "value": "Saw"})).await;
}

#[tokio::test]
async fn transport_controls() {
    let Some(h) = Harness::start().await else { return };
    let state =
        h.ok("transport", json!({"action": "play", "tempo": 128, "loop_enabled": true, "loop_length": 8})).await;
    assert_eq!(state["playing"], true);
    assert_eq!(state["tempo"], 128.0);
    assert_eq!(state["loop"], json!({"enabled": true, "start": 0.0, "length": 8.0}));
    let state = h.ok("transport", json!({"action": "stop"})).await;
    assert_eq!(state["playing"], false);
    // Live applies a new position only on the next tick and drops it if playback starts in the
    // same tick, so the tool must wait for the position before continuing.
    let state = h.ok("transport", json!({"position": 24, "action": "continue"})).await;
    assert_eq!((state["playing"].clone(), state["position"].clone()), (json!(true), json!(24.0)));
    let now = h.ok("get", json!({"target": "live_set current_song_time"})).await;
    assert_eq!(now, 24.0, "the fake does not advance time, so the position stays where it was set");
    h.ok("transport", json!({"action": "stop"})).await;

    h.ok("write_clip", json!({"track": 0, "slot": 1, "notes": [{"pitch": 60, "start": 0, "duration": 1}]})).await;
    h.ok("transport", json!({"action": "fire_clip", "track": 0, "slot": 1})).await;
    let s = h.ok("session", json!({})).await;
    assert_eq!(s["tracks"][0]["clips"][0]["playing"], true);

    let e = h.err("transport", json!({"action": "fire_clip", "track": 0})).await;
    assert!(e.contains("slot"), "{e}");
    let e = h.err("transport", json!({"tempo": 5000})).await;
    assert!(e.contains("tempo out of range"), "{e}");
}

#[tokio::test]
async fn generic_tools_reach_the_object_model() {
    let Some(h) = Harness::start().await else { return };
    let status = h.ok("status", json!({})).await;
    assert_eq!(status["script_current"], true);
    assert_eq!(status["live"]["live_version"], "12.3.8");

    let tracks = h.ok("get", json!({"target": "live_set tracks", "fields": ["name"]})).await;
    let id = tracks[0]["$obj"].as_u64().unwrap();
    assert_eq!(h.ok("get", json!({"target": format!("#{id} name")})).await, "1-MIDI");
    assert_eq!(h.ok("set", json!({"target": format!("#{id}"), "prop": "name", "value": "Keys"})).await, "Keys");

    let described = h.ok("describe", json!({"target": "live_set tracks 0"})).await;
    assert!(described["methods"].as_array().unwrap().contains(&json!("create_midi_clip")));

    let batch = h
        .ok(
            "batch",
            json!({"ops": [
                {"method": "call", "params": {"target": "live_set tracks 0 clip_slots 3", "method": "create_clip", "args": [2.0]}},
                {"method": "set", "params": {"target": "@0", "prop": "name", "value": "From batch"}},
                {"method": "get", "params": {"target": "@0 length"}},
            ]}),
        )
        .await;
    assert_eq!(batch["results"][2]["result"], 2.0);
    let read = h.ok("read_clip", json!({"track": "Keys", "slot": 3, "notes": false})).await;
    assert_eq!(read["name"], "From batch");
    assert!(read.get("notes").is_none());

    let e = h.err("get", json!({"target": "live_set nowhere"})).await;
    assert!(e.contains("no attribute"), "{e}");
    let e = h.err("call", json!({"target": "live_set", "method": "__class__"})).await;
    assert!(e.contains("invalid attribute name"), "{e}");
}
