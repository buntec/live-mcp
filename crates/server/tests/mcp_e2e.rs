//! End-to-end: an MCP client drives `live-mcp serve`, which talks to the real Remote Script on a fake Live.
//!
//! Requires `python3` on PATH; the tests are skipped (with a message) when it is missing.

use live_mcp_bridge::paths::STATE_DIR_ENV;
use live_mcp_bridge::testkit::FakeLive;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

struct Harness {
    client: RunningService<RoleClient, ()>,
    _fake: FakeLive,
}

impl Harness {
    async fn start() -> Option<Self> {
        let fake = FakeLive::start()?;
        let state = fake.state_dir();
        let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_live-mcp")).configure(|cmd| {
            cmd.arg("serve").env(STATE_DIR_ENV, &state);
        });
        let client = ().serve(TokioChildProcess::new(command).expect("spawn live-mcp")).await.expect("initialize");
        Some(Self { client, _fake: fake })
    }

    /// Calls a tool; Ok with the parsed JSON result, or Err with the tool's error text.
    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let arguments = args.as_object().cloned().expect("arguments must be an object");
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
            .await
            .expect("tools/call");
        let text = result.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
        if result.is_error == Some(true) {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} returned non-JSON ({e}): {text}")))
        }
    }

    async fn ok(&self, name: &str, args: Value) -> Value {
        self.call(name, args).await.unwrap_or_else(|e| panic!("{name} failed: {e}"))
    }

    async fn err(&self, name: &str, args: Value) -> String {
        match self.call(name, args).await {
            Err(e) => e,
            Ok(v) => panic!("{name} unexpectedly succeeded: {v}"),
        }
    }
}

#[tokio::test]
async fn lists_all_tools_with_object_schemas() {
    let Some(h) = Harness::start().await else { return };
    let tools = h.client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "batch",
            "browser",
            "call",
            "create_track",
            "describe",
            "device_parameters",
            "get",
            "load_device",
            "read_clip",
            "session",
            "set",
            "set_parameter",
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
async fn transport_controls() {
    let Some(h) = Harness::start().await else { return };
    let state =
        h.ok("transport", json!({"action": "play", "tempo": 128, "loop_enabled": true, "loop_length": 8})).await;
    assert_eq!(state["playing"], true);
    assert_eq!(state["tempo"], 128.0);
    assert_eq!(state["loop"], json!({"enabled": true, "start": 0.0, "length": 8.0}));
    let state = h.ok("transport", json!({"action": "stop"})).await;
    assert_eq!(state["playing"], false);

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
