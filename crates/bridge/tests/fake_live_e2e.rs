//! End-to-end tests: the real Remote Script, hosted on a fake `Live` module, driven by the Rust client.
//!
//! Requires `python3` on PATH; the tests are skipped (with a message) when it is missing.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use live_mcp_bridge::{Bridge, BridgeError, GetOptions, Options, paths, script};
use live_mcp_protocol::{BatchOp, Target, codes, new_live_object, tuple};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct FakeLive {
    child: Child,
    state: tempfile::TempDir,
}

impl FakeLive {
    fn start() -> Option<Self> {
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("skipping: python3 not found");
            return None;
        }
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("config.json"), r#"{"port": 0}"#).unwrap();
        let runner = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_live/run.py");
        let child = Command::new("python3")
            .arg("-I")
            .arg("-B")
            .arg(runner)
            .env(paths::STATE_DIR_ENV, state.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn python3");
        let fake = Self { child, state };
        let deadline = Instant::now() + Duration::from_secs(10);
        while paths::read_endpoint(fake.state.path()).is_none() {
            assert!(Instant::now() < deadline, "fake Live did not start listening");
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(fake)
    }

    fn options(&self) -> Options {
        Options { state_dir: self.state.path().to_path_buf(), ..Options::default() }
    }

    fn state_dir(&self) -> PathBuf {
        self.state.path().to_path_buf()
    }

    async fn connect(&self) -> Bridge {
        Bridge::connect(&self.options()).await.expect("connect")
    }
}

impl Drop for FakeLive {
    fn drop(&mut self) {
        drop(self.child.stdin.take()); // closing stdin stops the runner cleanly
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
    }
}

fn path(p: &str) -> Target {
    Target::path(p)
}

#[tokio::test]
async fn handshake_reports_current_script_and_info() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    assert_eq!(live.hello().code_hash, script::source_hash());
    assert!(!live.script_is_stale());

    let info = live.info().await.unwrap();
    assert_eq!(info["live_version"], "12.3.8");
    assert_eq!(info["protocol"], 1);

    let endpoint = paths::read_endpoint(&fake.state_dir()).unwrap();
    assert_eq!(endpoint.code_hash, script::source_hash());
}

#[tokio::test]
async fn get_set_call_and_describe() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;

    let tracks = live.get(&path("live_set tracks"), GetOptions::fields(["name", "mute"])).await.unwrap();
    let names: Vec<&str> = tracks.as_array().unwrap().iter().map(|t| t["fields"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["1-MIDI", "2-Audio"]);
    assert_eq!(tracks[0]["type"], "Track");

    // Same Live object, same id: ids come from _live_ptr.
    let again = live.get(&path("live_set tracks 0"), GetOptions::default()).await.unwrap();
    assert_eq!(again["$obj"], tracks[0]["$obj"]);

    let id = tracks[1]["$obj"].as_u64().unwrap();
    assert_eq!(live.set(&Target::id(id), "name", json!("Drums")).await.unwrap(), "Drums");
    let props = live.get(&path("live_set tracks 1"), GetOptions::props(["name", "mute"])).await.unwrap();
    assert_eq!(props, json!({"name": "Drums", "mute": false}));

    // Relative path from an id.
    let slot_target = Target::Ref { id, path: Some("clip_slots 0".into()) };
    live.call(&slot_target, "create_clip", vec![json!(4.0)]).await.unwrap();
    let clip = live
        .get(&Target::Ref { id, path: Some("clip_slots 0 clip".into()) }, GetOptions::fields(["length"]))
        .await
        .unwrap();
    assert_eq!(clip["fields"]["length"], 4.0);

    let described = live.describe(&path("live_set tracks 0"), true).await.unwrap();
    assert_eq!(described["type"], "Track");
    let props: Vec<&str> = described["properties"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(props.contains(&"name") && props.contains(&"clip_slots"));
    assert_eq!(described["observable"], json!(["name"]));
    assert!(described["methods"].as_array().unwrap().iter().all(|m| !m.as_str().unwrap().contains("listener")));
    assert_eq!(described["values"]["name"], "1-MIDI");

    // Paging a sequence.
    let page = live
        .get(
            &path("live_set tracks 0 clip_slots"),
            GetOptions { offset: Some(2), limit: Some(3), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(page["total"], 8);
    assert_eq!(page["items"].as_array().unwrap().len(), 3);

    // Non-finite floats are encoded as null rather than breaking the JSON.
    assert_eq!(live.get(&path("live_set nan_value"), GetOptions::default()).await.unwrap(), Value::Null);
}

#[tokio::test]
async fn notes_via_constructed_live_objects() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    live.call(&path("live_set tracks 0 clip_slots 0"), "create_clip", vec![json!(4.0)]).await.unwrap();

    let notes: Vec<Value> = [(60, 0.0), (64, 1.0), (67, 2.0)]
        .into_iter()
        .map(|(pitch, start)| {
            new_live_object(
                "Clip.MidiNoteSpecification",
                json!({"pitch": pitch, "start_time": start, "duration": 0.5, "velocity": 100.0}),
            )
        })
        .collect();
    let clip = path("live_set tracks 0 clip_slots 0 clip");
    live.call(&clip, "add_new_notes", vec![tuple(notes)]).await.unwrap();

    let read = live
        .request(
            "call",
            json!({
                "target": clip, "method": "get_notes_extended",
                "args": [0, 128, 0.0, 4.0], "fields": ["pitch", "start_time", "duration"]
            }),
        )
        .await
        .unwrap();
    let pitches: Vec<i64> = read.as_array().unwrap().iter().map(|n| n["fields"]["pitch"].as_i64().unwrap()).collect();
    assert_eq!(pitches, [60, 64, 67]);

    // $new is confined to the Live module.
    let err = live
        .call(&clip, "add_new_notes", vec![tuple(vec![new_live_object("__builtins__.open", json!({}))])])
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));
}

#[tokio::test]
async fn batch_runs_in_one_undo_step_and_stops_on_error() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    let ops = vec![
        BatchOp {
            method: "call".into(),
            params: json!({"target": "live_set", "method": "create_midi_track", "args": [-1]}),
        },
        BatchOp {
            method: "set".into(),
            params: json!({"target": "live_set tracks 2", "prop": "name", "value": "Lead"}),
        },
        BatchOp { method: "set".into(), params: json!({"target": "live_set tempo", "prop": "x", "value": 1}) },
        BatchOp { method: "set".into(), params: json!({"target": "live_set", "prop": "tempo", "value": 99.0}) },
    ];
    let result = live.batch(&ops, true, true).await.unwrap();
    let results = result["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "stopped after the failing op");
    assert!(results[0]["ok"].as_bool().unwrap() && results[1]["ok"].as_bool().unwrap());
    assert_eq!(results[2]["ok"], false);
    assert_eq!(results[2]["error"]["code"], codes::INVALID_PARAMS);

    let song = live
        .get(&path("live_set"), GetOptions::props(["tempo", "undo_steps_begun", "undo_steps_ended"]))
        .await
        .unwrap();
    assert_eq!(song, json!({"tempo": 120.0, "undo_steps_begun": 1, "undo_steps_ended": 1}));
    let name = live.get(&path("live_set tracks 2 name"), GetOptions::default()).await.unwrap();
    assert_eq!(name, "Lead");

    let ops = vec![
        BatchOp { method: "get".into(), params: json!({"target": "live_set nope"}) },
        BatchOp { method: "set".into(), params: json!({"target": "live_set", "prop": "tempo", "value": 99.0}) },
    ];
    let result = live.batch(&ops, false, false).await.unwrap();
    assert_eq!(result["results"][1]["ok"], true, "continue_on_error ran the second op");
}

#[tokio::test]
async fn errors_are_typed() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;

    let err = live.get(&path("live_set tracks 9"), GetOptions::default()).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));
    let err = live.get(&path("nowhere"), GetOptions::default()).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));
    let err = live.get(&Target::id(12345), GetOptions::default()).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::STALE_REF));
    let err = live.set(&path("live_set"), "tempo", json!(5000.0)).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::LIVE));
    assert!(err.to_string().contains("tempo out of range"));

    // No reflection into Python internals, and set/call only on Live objects.
    let err = live.get(&path("live_set __class__"), GetOptions::default()).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));
    let err = live.call(&path("live_set tracks 0 name"), "upper", vec![]).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));
    let err = live.request("exec", json!({})).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::METHOD_NOT_FOUND));

    assert!(!live.is_closed(), "errors do not close the connection");
}

#[tokio::test]
async fn bad_token_is_rejected_and_unauthenticated_requests_close() {
    let Some(fake) = FakeLive::start() else { return };
    // A client whose state dir points at the right port but holds the wrong token.
    let client_state = tempfile::tempdir().unwrap();
    std::fs::write(paths::token_path(client_state.path()), "wrong\n").unwrap();
    std::fs::copy(paths::endpoint_path(&fake.state_dir()), paths::endpoint_path(client_state.path())).unwrap();
    let options = Options { state_dir: client_state.path().to_path_buf(), ..fake.options() };
    match Bridge::connect(&options).await {
        Err(BridgeError::Unauthorized(_)) => {}
        Err(other) => panic!("expected Unauthorized, got {other}"),
        Ok(_) => panic!("expected Unauthorized, got a connection"),
    }

    // A raw client that skips hello gets an error and the connection is closed.
    let port = paths::read_endpoint(&fake.state_dir()).unwrap().port;
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (read, mut write) = stream.into_split();
    write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"info\",\"params\":{}}\n").await.unwrap();
    let mut lines = BufReader::new(read).lines();
    let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["error"]["code"], codes::UNAUTHORIZED);
    assert!(lines.next_line().await.unwrap().is_none(), "connection closed after unauthorised request");
}

#[tokio::test]
async fn main_thread_timeout_fails_closed_and_status_still_answers() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;

    let slow = live.request_with_timeout(
        "call",
        json!({"target": "live_set", "method": "slow", "args": [1.5]}),
        Duration::from_millis(300),
    );
    match slow.await {
        Err(BridgeError::Timeout { started: Some(true), .. }) => {}
        other => panic!("expected a started timeout, got {other:?}"),
    }

    // The main thread is still blocked in `slow`, but the socket thread reports it.
    let status = live.status().await.unwrap();
    assert_eq!(status["busy_method"], "call");

    // A request queued behind the blocked job times out without ever starting.
    let queued = live.request_with_timeout("info", json!({}), Duration::from_millis(200)).await;
    match queued {
        Err(BridgeError::Timeout { started: Some(false), .. }) => {}
        other => panic!("expected a not-started timeout, got {other:?}"),
    }

    // Once the main thread recovers, the connection keeps working.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(live.info().await.unwrap()["live_version"], "12.3.8");
}

#[tokio::test]
async fn concurrent_connections_are_serialised_on_the_main_thread() {
    let Some(fake) = FakeLive::start() else { return };
    let a = fake.connect().await;
    let b = fake.connect().await;
    let song = path("live_set");
    let (ra, rb) = tokio::join!(
        a.call(&song, "create_midi_track", vec![json!(-1)]),
        b.call(&song, "create_midi_track", vec![json!(-1)]),
    );
    ra.unwrap();
    rb.unwrap();
    let tracks = a.get(&path("live_set tracks"), GetOptions::default()).await.unwrap();
    assert_eq!(tracks.as_array().unwrap().len(), 4);
}
