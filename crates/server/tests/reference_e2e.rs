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

#[tokio::test]
async fn compares_a_mix_against_a_reference() {
    let Some(h) = Harness::start().await else { return };
    let dir = h.state_dir();
    let mix = |low_mid_db: f64, overall_db: f64| -> Vec<f32> {
        let parts = [(80.0, -18.0), (300.0, low_mid_db), (1500.0, -20.0), (5000.0, -26.0)];
        let gain = 10f64.powf(overall_db / 20.0);
        let n = 48_000 * 8; // four bars at 120 BPM
        (0..n)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                let sum: f64 = parts
                    .iter()
                    .map(|(f, db)| 10f64.powf(db / 20.0) * (2.0 * std::f64::consts::PI * f * t).sin())
                    .sum();
                (gain * sum) as f32
            })
            .collect()
    };
    let reference = dir.join("reference.wav");
    let muddy = dir.join("muddy.wav");
    write_wav(&reference, 48_000, &[mix(-24.0, 0.0), mix(-24.0, 0.0)]);
    // 9 dB more 300 Hz, and 4 dB quieter overall.
    write_wav(&muddy, 48_000, &[mix(-15.0, -4.0), mix(-15.0, -4.0)]);

    let r = h.ok("analyze_file", json!({"path": reference.display().to_string(), "bpm": 120})).await;
    let m = h.ok("analyze_file", json!({"path": muddy.display().to_string(), "bpm": 120})).await;
    let (rid, mid) = (r["snapshot"].as_str().unwrap().to_owned(), m["snapshot"].as_str().unwrap().to_owned());
    assert_ne!(rid, mid);

    let c = h.ok("compare", json!({"a": mid, "b": rid, "a_bars": [2, 3], "b_bars": [2, 3]})).await;
    let notes: Vec<&str> = c["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert!(notes.iter().any(|n| n.contains("more low_mid (120–500 Hz)")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("quieter")), "{notes:?}");
    // After matching levels, the 9 dB boost shows in low_mid alone.
    let tone = c["tonal_balance_db"].as_array().unwrap();
    assert!((tone[1]["delta"].as_f64().unwrap() - 9.0).abs() < 1.0, "{tone:?}");
    for i in [0, 2, 3, 4] {
        assert!(tone[i]["delta"].as_f64().unwrap().abs() < 1.0, "band {i}: {tone:?}");
    }
    let tonal_notes = notes.iter().filter(|n| n.contains("with levels matched")).count();
    assert_eq!(tonal_notes, 1, "{notes:?}");
    assert_eq!(c["a"]["bars"], "2–3");
    assert_eq!(c["per_bar"].as_array().unwrap().len(), 2, "equal-length selections pair up");

    let listed = h.ok("snapshots", json!({})).await;
    let labels: Vec<&str> =
        listed["snapshots"].as_array().unwrap().iter().map(|s| s["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["reference.wav at 120 BPM", "muddy.wav at 120 BPM"]);

    let e = h.err("compare", json!({"a": "s99", "b": rid})).await;
    assert!(e.contains("no snapshot"), "{e}");
}

/// Four bars at 120 BPM, one chord (MIDI notes) per bar, each note with a few harmonics and a
/// pluck on every beat.
fn chords(sample_rate: u32, bars: &[&[f64]]) -> Vec<f32> {
    let sr = sample_rate as f64;
    (0..(bars.len() as f64 * 2.0 * sr) as usize)
        .map(|n| {
            let t = n as f64 / sr;
            let notes = bars[(t / 2.0) as usize];
            let pluck = (-(t % 0.5) * 5.0).exp();
            let sum: f64 = notes
                .iter()
                .map(|&m| {
                    let f = 440.0 * 2f64.powf((m - 69.0) / 12.0);
                    (1..=4).map(|h| (2.0 * std::f64::consts::PI * f * h as f64 * t).sin() / h as f64).sum::<f64>()
                })
                .sum();
            (sum * 0.08 * pluck) as f32
        })
        .collect()
}

#[tokio::test]
async fn reads_and_compares_the_music_of_files() {
    let Some(h) = Harness::start().await else { return };
    let dir = tempfile::tempdir().unwrap();
    let (c, f, g, dm) = (
        &[48.0, 55.0, 60.0, 64.0][..],
        &[53.0, 57.0, 60.0, 65.0][..],
        &[55.0, 59.0, 62.0, 67.0][..],
        &[50.0, 57.0, 62.0, 65.0][..],
    );
    let before = dir.path().join("before.wav");
    let after = dir.path().join("after.wav");
    let a = chords(48_000, &[c, f, g, c]);
    let b = chords(48_000, &[c, dm, g, c]);
    write_wav(&before, 48_000, &[a.clone(), a]);
    write_wav(&after, 48_000, &[b.clone(), b]);

    let x = h.ok("analyze_file", json!({"path": before.display().to_string(), "bpm": 120})).await;
    assert_eq!(x["music"]["progression"], "C | F | G | C", "{}", x["music"]);
    assert_eq!(x["music"]["key"]["name"], "C major", "{}", x["music"]);
    assert_eq!(x["bars"][1]["chord"], "F", "{}", x["bars"][1]);
    // An onset on every beat (strong or weaker) and nothing in between.
    let grid: String = x["bars"][1]["rhythm"].as_str().unwrap().replace('X', "x");
    assert_eq!(grid, "x... x... x... x...", "{}", x["bars"][1]);
    let y = h.ok("analyze_file", json!({"path": after.display().to_string(), "bpm": 120})).await;
    assert_eq!(y["music"]["progression"], "C | Dm | G | C", "{}", y["music"]);

    let compared = h.ok("compare", json!({"a": y["snapshot"], "b": x["snapshot"]})).await;
    let music = &compared["music"];
    assert_eq!(music["per_bar"], json!([{"a": 2, "b": 2, "chords": ["Dm", "F"]}]), "{music}");
    let notes: Vec<&str> = compared["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert!(notes.contains(&"the chords differ in 1 of 4 paired bars"), "{notes:?}");

    // Without a bpm there is no rhythm grid, and music: false leaves the music out.
    let sections = h.ok("analyze_file", json!({"path": before.display().to_string()})).await;
    assert!(sections["music"].get("rhythm").is_none() && sections["sections"][0].get("rhythm").is_none());
    assert!(sections["sections"][0]["pcs"].is_string(), "{}", sections["sections"][0]);
    let plain = h.ok("analyze_file", json!({"path": before.display().to_string(), "bpm": 120, "music": false})).await;
    assert!(plain.get("music").is_none() && plain["bars"][0].get("chord").is_none());
}
