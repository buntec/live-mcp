//! `analyze_file` end to end, on a generated WAV file with known content.

mod common;

use common::{Harness, tone_sequence, write_wav};
use serde_json::{Value, json};

fn loudest_band(bar: &Value) -> String {
    let bands = bar["bands"].as_object().unwrap();
    bands.iter().max_by(|a, b| a.1.as_f64().unwrap().total_cmp(&b.1.as_f64().unwrap())).unwrap().0.clone()
}

#[tokio::test]
async fn analyses_a_wav_file_in_bars_and_sections() {
    let Some(h) = Harness::start().await else { return };
    let dir = h.state_dir();
    let path = dir.join("reference.wav");
    // 0.5 s of silence, then four bars at 120 BPM (2 s each).
    let tone =
        tone_sequence(48_000, &[(0.5, 0.0, -300.0), (2.0, 100.0, -20.0), (2.0, 3000.0, -20.0), (4.0, 1000.0, -30.0)]);
    write_wav(&path, 48_000, &[tone.clone(), tone]);

    let a = h.ok("analyze_file", json!({"path": path.display().to_string(), "bpm": 120, "first_downbeat": 0.5})).await;
    assert_eq!((a["sample_rate"].clone(), a["channels"].clone()), (json!(48_000), json!(2)));
    assert_eq!(a["duration_seconds"], 8.5);
    assert_eq!(a["tempo"], 120.0);
    let bars = a["bars"].as_array().unwrap();
    let numbers: Vec<i64> = bars.iter().map(|b| b["bar"].as_i64().unwrap()).collect();
    assert_eq!(numbers, [1, 2, 3, 4], "{a}");
    assert_eq!(bars[0]["start_seconds"], 0.5);
    assert_eq!(bars[1]["start_seconds"], 2.5);
    assert_eq!(loudest_band(&bars[0]), "low");
    assert_eq!(loudest_band(&bars[1]), "high_mid");
    assert_eq!(loudest_band(&bars[2]), "mid");
    // A stereo 1 kHz sine at −30 dBFS measures −30 LUFS (EBU Tech 3341 calibration).
    assert!((bars[2]["lufs"].as_f64().unwrap() + 30.0).abs() <= 0.15, "{}", bars[2]);
    assert_eq!(bars[0]["peak"], -20.0);

    let s = &a["summary"];
    assert_eq!(s["peak"], -20.0);
    assert_eq!(s["correlation"], 1.0);
    let integrated = s["integrated_lufs"].as_f64().unwrap();
    assert!((-25.0..-17.0).contains(&integrated), "{s}");
    assert!(s["loudness_range_lu"].as_f64().unwrap() > 3.0, "the −30 dB bars widen the range: {s}");
    assert!(s["loudest_bar"].as_i64().unwrap() <= 2);

    // Without a tempo: 2-second sections from the start of the file; the last one is partial.
    let a = h.ok("analyze_file", json!({"path": path.display().to_string()})).await;
    let sections = a["sections"].as_array().unwrap();
    assert_eq!(sections.len(), 5);
    assert_eq!(sections[0]["section"], 1);
    assert!(sections[0].get("bar").is_none());
    assert_eq!(sections[4]["coverage"], 0.25);
    assert!(a.get("tempo").is_none());

    let a = h.ok("analyze_file", json!({"path": path.display().to_string(), "bars": false})).await;
    assert!(a.get("sections").is_none() && a.get("summary").is_some());

    let e = h.err("analyze_file", json!({"path": "relative.wav"})).await;
    assert!(e.contains("absolute"), "{e}");
    let e = h.err("analyze_file", json!({"path": dir.join("nope.wav").display().to_string()})).await;
    assert!(e.contains("opening"), "{e}");
    let e = h.err("analyze_file", json!({"path": path.display().to_string(), "bpm": 5})).await;
    assert!(e.contains("bpm"), "{e}");
}
