//! End-to-end tests: the real Remote Script, hosted on a fake `Live` module, driven by the Rust client.
//!
//! Requires `python3` on PATH; the tests are skipped (with a message) when it is missing.

use std::time::Duration;

use live_mcp_bridge::testkit::FakeLive;
use live_mcp_bridge::{Bridge, BridgeError, GetOptions, Options, paths, script};
use live_mcp_protocol::{BatchOp, Target, codes, new_live_object, tuple};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

    // Relative path from an id (MIDI clips need a MIDI track, as in Live).
    let id = tracks[0]["$obj"].as_u64().unwrap();
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
    assert_eq!(described["observable"], json!(["mute", "name", "solo"]));
    assert_eq!(described["types"], json!(["View"]), "nested classes are not methods");
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
async fn set_retries_number_strings_only_where_live_wants_a_number() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;

    // Some MCP clients send an untyped 1.0 or 0.50 as a string; Live rejects a str for a float.
    assert_eq!(live.set(&path("live_set"), "tempo", json!("128.0")).await.unwrap(), 128.0);
    let volume = path("live_set tracks 0 mixer_device volume");
    assert_eq!(live.set(&volume, "value", json!("0.50")).await.unwrap(), 0.5);
    assert_eq!(live.set(&volume, "value", json!("1")).await.unwrap(), 1.0);

    // The same path inside a batch.
    let ops = vec![BatchOp {
        method: "set".into(),
        params: json!({"target": "live_set", "prop": "tempo", "value": "1.25e2"}),
    }];
    let result = live.batch(&ops, true, true).await.unwrap();
    assert_eq!(result["results"][0]["result"]["value"], 125.0);

    // A property that takes a string keeps the string.
    assert_eq!(live.set(&path("live_set tracks 0"), "name", json!("808")).await.unwrap(), "808");

    // Anything that is not a JSON number literal still fails, with Live's own error.
    for text in ["fast", "nan", "inf", " 120", "0x7f", "01"] {
        let err = live.set(&path("live_set"), "tempo", json!(text)).await.unwrap_err();
        assert_eq!(err.code(), Some(codes::LIVE), "{text:?}");
        assert!(err.to_string().contains("did not match C++ signature"), "{text:?}: {err}");
    }
    // A coerced number is still checked by Live.
    let err = live.set(&path("live_set"), "tempo", json!("5000.0")).await.unwrap_err();
    assert!(err.to_string().contains("tempo out of range"), "{err}");
    assert_eq!(live.get(&path("live_set tempo"), GetOptions::default()).await.unwrap(), 125.0);
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
async fn timed_out_requests_may_still_apply_and_the_connection_recovers() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;

    let slow = live.request_with_timeout(
        "call",
        json!({"target": "live_set", "method": "slow", "args": [1.0]}),
        Duration::from_millis(300),
    );
    assert!(matches!(slow.await, Err(BridgeError::Timeout { .. })));

    // The timed-out call still ran to completion: this is why timeouts are never retried.
    // Its late response must be discarded rather than answer the next request.
    let calls = live.get(&path("live_set slow_calls"), GetOptions::default()).await.unwrap();
    assert_eq!(calls, 1);
    assert_eq!(live.info().await.unwrap()["live_version"], "12.3.8");
}

#[tokio::test]
async fn sequential_requests_share_ticks_by_lingering() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    assert_eq!(live.info().await.unwrap()["linger"], true);
    let before = live.status().await.unwrap()["ticks"].as_u64().unwrap();
    for _ in 0..20 {
        live.get(&path("live_set tempo"), GetOptions::default()).await.unwrap();
    }
    let ticks = live.status().await.unwrap()["ticks"].as_u64().unwrap() - before;
    // Without lingering every request would wait for its own tick.
    assert!(ticks < 15, "20 sequential requests took {ticks} ticks");
}

#[tokio::test]
async fn pipelined_requests_share_ticks() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    let before = live.status().await.unwrap()["ticks"].as_u64().unwrap();

    let target = path("live_set tracks 0 name");
    let requests: Vec<_> = (0..50).map(|_| live.get(&target, GetOptions::default())).collect();
    for result in futures::future::join_all(requests).await {
        assert_eq!(result.unwrap(), "1-MIDI");
    }

    let status = live.status().await.unwrap();
    let ticks = status["ticks"].as_u64().unwrap() - before;
    assert!(ticks < 25, "50 pipelined requests took {ticks} ticks");
    assert_eq!(status["clients"], 1);
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

#[tokio::test]
async fn batch_ops_can_target_earlier_results() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    let created = Target::batch_result(0);
    let note = new_live_object("Clip.MidiNoteSpecification", json!({"pitch": 48, "start_time": 0.0, "duration": 1.0}));
    let ops = vec![
        BatchOp {
            method: "call".into(),
            params: json!({"target": "live_set tracks 0 clip_slots 1", "method": "create_clip", "args": [8.0]}),
        },
        BatchOp { method: "set".into(), params: json!({"target": created, "prop": "name", "value": "Bassline"}) },
        BatchOp {
            method: "call".into(),
            params: json!({"target": created, "method": "add_new_notes", "args": [tuple(vec![note])]}),
        },
        BatchOp { method: "get".into(), params: json!({"target": created.join("length")}) },
        BatchOp { method: "get".into(), params: json!({"target": Target::batch_result(1)}) },
    ];
    let result = live.batch(&ops, true, false).await.unwrap();
    let results = result["results"].as_array().unwrap();
    assert!(results[..4].iter().all(|r| r["ok"] == true), "{results:?}");
    assert_eq!(results[3]["result"], 8.0);
    // Op 1 returned a plain value, not a Live object.
    assert_eq!(results[4]["error"]["code"], codes::INVALID_PARAMS);

    let name = live.get(&path("live_set tracks 0 clip_slots 1 clip name"), GetOptions::default()).await.unwrap();
    assert_eq!(name, "Bassline");

    let err = live.get(&Target::batch_result(0), GetOptions::default()).await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS), "result targets only work inside a batch");
}

/// Polls `events` until at least `n` events arrived (they can fire on a later tick).
async fn wait_for_events(live: &Bridge, n: usize) -> Vec<live_mcp_protocol::Event> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while events.len() < n {
        assert!(std::time::Instant::now() < deadline, "only {} of {n} events arrived: {events:?}", events.len());
        events.extend(live.events().await.unwrap().events);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    events
}

#[tokio::test]
async fn observe_reports_changes_with_values() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;

    let tempo = live.observe(&path("live_set"), "tempo").await.unwrap();
    assert_eq!(tempo["value"], 120.0);
    assert_eq!(tempo["object"]["type"], "Song");
    let tempo_sub = tempo["subscription"].as_u64().unwrap();
    // Observing the same property of the same object again reuses the subscription.
    let again = live.observe(&path("song"), "tempo").await.unwrap();
    assert_eq!(again["subscription"].as_u64(), Some(tempo_sub));
    assert_eq!(again["existing"], true);

    let playing = live.observe(&path("live_set"), "is_playing").await.unwrap()["subscription"].as_u64().unwrap();
    let mute = live.observe(&path("live_set tracks 1"), "mute").await.unwrap()["subscription"].as_u64().unwrap();
    assert_eq!(live.status().await.unwrap()["subscriptions"], 3);

    // Nothing has changed yet.
    let initial = live.events().await.unwrap();
    assert!(initial.events.is_empty() && initial.dropped == 0, "{initial:?}");

    live.set(&path("live_set"), "tempo", json!(128.0)).await.unwrap();
    live.set(&path("live_set"), "tempo", json!(128.0)).await.unwrap(); // unchanged: no event
    live.set(&path("live_set tracks 1"), "mute", json!(true)).await.unwrap();
    let events = live.events().await.unwrap().events;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!((events[0].subscription, events[0].value.clone(), events[0].own), (tempo_sub, Some(json!(128.0)), true));
    assert_eq!((events[1].subscription, events[1].value.clone()), (mute, Some(json!(true))));
    assert!(events[0].seq < events[1].seq);
    assert!(live.events().await.unwrap().events.is_empty(), "events are taken once");

    // Transport changes apply on a later tick, outside the request that asked for them.
    live.call(&path("live_set"), "start_playing", vec![]).await.unwrap();
    let events = wait_for_events(&live, 1).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!((events[0].subscription, events[0].value.clone(), events[0].own), (playing, Some(json!(true)), false));

    // A listener follows the object, not the path it was found by.
    live.call(&path("live_set"), "create_midi_track", vec![json!(0)]).await.unwrap();
    live.set(&path("live_set tracks 2"), "mute", json!(false)).await.unwrap();
    let events = live.events().await.unwrap().events;
    assert_eq!(events.iter().map(|e| e.subscription).collect::<Vec<_>>(), [mute]);
}

#[tokio::test]
async fn observables_without_a_property_and_errors() {
    let Some(fake) = FakeLive::start() else { return };
    let live = fake.connect().await;
    live.call(&path("live_set tracks 0 clip_slots 0"), "create_clip", vec![json!(4.0)]).await.unwrap();
    let clip = path("live_set tracks 0 clip_slots 0 clip");

    // A clip's `notes` has a listener but no property: there is no value to report.
    let notes = live.observe(&clip, "notes").await.unwrap();
    assert!(notes.get("value").is_none(), "{notes}");
    let note = new_live_object("Clip.MidiNoteSpecification", json!({"pitch": 60, "start_time": 0.0, "duration": 1.0}));
    live.call(&clip, "add_new_notes", vec![tuple(vec![note])]).await.unwrap();
    let events = live.events().await.unwrap().events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].value, None);

    let err = live.observe(&path("live_set"), "signature_numerator").await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));
    assert!(err.to_string().contains("observable"), "{err}");
    let err = live.observe(&path("live_set tracks 0 name"), "upper").await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS), "only Live objects can be observed");
    let err = live.observe(&path("live_set"), "__class__").await.unwrap_err();
    assert_eq!(err.code(), Some(codes::INVALID_PARAMS));

    // Subscriptions belong to a connection, so they cannot be made inside a batch.
    let op = BatchOp { method: "observe".into(), params: json!({"target": "live_set", "prop": "tempo"}) };
    let result = live.batch(&[op], false, true).await.unwrap();
    assert_eq!(result["results"][0]["error"]["code"], codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn unobserve_and_connections_keep_subscriptions_apart() {
    let Some(fake) = FakeLive::start() else { return };
    let a = fake.connect().await;
    let b = fake.connect().await;

    let a_tempo = a.observe(&path("live_set"), "tempo").await.unwrap()["subscription"].as_u64().unwrap();
    let a_name = a.observe(&path("live_set tracks 0"), "name").await.unwrap()["subscription"].as_u64().unwrap();
    let b_tempo = b.observe(&path("live_set"), "tempo").await.unwrap()["subscription"].as_u64().unwrap();
    assert_ne!(a_tempo, b_tempo, "each connection gets its own subscription");

    b.set(&path("live_set"), "tempo", json!(100.0)).await.unwrap();
    let seen_by_a = a.events().await.unwrap().events;
    assert_eq!(seen_by_a.len(), 1);
    assert!(!seen_by_a[0].own, "b's request is not a's own");
    assert!(b.events().await.unwrap().events[0].own);

    // Unobserving discards unread events of that subscription.
    a.set(&path("live_set tracks 0"), "name", json!("Lead")).await.unwrap();
    let removed = a.unobserve(Some(&[a_name, 999])).await.unwrap();
    assert_eq!(removed, json!({"removed": [a_name], "unknown": [999]}));
    assert!(a.events().await.unwrap().events.is_empty());
    a.set(&path("live_set tracks 0"), "name", json!("Keys")).await.unwrap();
    assert!(a.events().await.unwrap().events.is_empty());

    // A closed connection's listeners are removed.
    assert_eq!(b.status().await.unwrap()["subscriptions"], 2);
    drop(a);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while b.status().await.unwrap()["subscriptions"] != 1 {
        assert!(std::time::Instant::now() < deadline, "a's subscriptions outlived its connection");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // An observed object that is deleted can still be unobserved.
    let name = b.observe(&path("live_set tracks 1"), "name").await.unwrap()["subscription"].as_u64().unwrap();
    b.call(&path("live_set"), "delete_track", vec![json!(1)]).await.unwrap();
    assert_eq!(b.unobserve(None).await.unwrap()["removed"], json!([b_tempo, name]));
    assert_eq!(b.status().await.unwrap()["subscriptions"], 0);
}
