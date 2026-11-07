//! Replays beats recorded by `listen` (with `"record_beats": true` in `~/.live-mcp/config.json`,
//! which records into `~/.live-mcp/recordings`) through the interpreter, to tune it on real audio
//! without playing anything:
//!
//! ```text
//! HEARD=~/.live-mcp/recordings cargo test -p live-mcp-server replay -- --ignored --nocapture
//! ```
//!
//! Each `<snapshot>.json` holds the bars and beat reports one Ear heard. An optional
//! `labels.json` in the same folder maps snapshot ids to {"name", "key", "chords"} (chords as
//! "Am C G F", one per bar) to score against.

use std::collections::BTreeMap;
use std::path::Path;

use live_mcp_analysis::BeatReport;
use serde_json::Value;

use super::{BarSpan, Section, Source, analyze, audio};

pub struct Recording {
    pub id: String,
    pub bars: Vec<BarSpan>,
    pub beats: Vec<BeatReport>,
}

pub fn load(dir: &Path) -> Vec<Recording> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned();
        if path.extension().is_none_or(|e| e != "json") || id == "labels" {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path).map(|t| serde_json::from_str::<Value>(&t)) else { continue };
        let Ok(body) = body else { continue };
        let bars = body["bars"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["coverage"].as_f64().unwrap_or(0.0) >= 0.5)
            .map(|b| BarSpan {
                bar: b["bar"].as_i64().unwrap_or(0),
                start_beat: b["start_beat"].as_f64().unwrap_or(0.0),
                beats: b["beats"].as_f64().unwrap_or(4.0),
            })
            .collect();
        let beats = serde_json::from_value(body["beats"].clone()).unwrap_or_default();
        out.push(Recording { id, bars, beats });
    }
    out.sort_by_key(|r| r.id.trim_start_matches('s').parse::<u64>().unwrap_or(u64::MAX));
    out
}

fn tonic(name: &str) -> Option<(u8, Option<bool>)> {
    let (note, rest) = name.split_at(name.find([' ', 'm']).unwrap_or(name.len()));
    let pc =
        ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"].iter().position(|n| *n == note).or_else(
            || ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"].iter().position(|n| *n == note),
        )?;
    let rest = rest.trim();
    let minor = if rest.starts_with('m') {
        Some(true)
    } else if rest == "major" || rest == "maj" {
        Some(false)
    } else {
        None
    };
    Some((pc as u8, minor))
}

#[test]
#[ignore = "needs recordings: HEARD=<dir>"]
fn replay() {
    let Ok(dir) = std::env::var("HEARD") else { return };
    let dir = Path::new(&dir);
    let labels: BTreeMap<String, Value> = std::fs::read_to_string(dir.join("labels.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let (mut keys, mut keys_right, mut chords, mut chords_right) = (0, 0, 0, 0);
    for rec in load(dir) {
        let (beats, tuning) = audio::beats(&rec.beats);
        let analysis = analyze(&Section { bars: &rec.bars, beats: &beats, source: Source::Audio, scale: None });
        let s = &analysis.summary;
        let label = labels.get(&rec.id);
        let name = label.and_then(|l| l["name"].as_str()).unwrap_or("");
        let got_key = s.get("key").and_then(|k| k.get("name")).and_then(Value::as_str).unwrap_or("-");
        let mut verdict = String::new();
        if let Some(want) = label.and_then(|l| l["key"].as_str()) {
            keys += 1;
            let ok = match (
                tonic(want),
                tonic(got_key.trim_end_matches(" major").trim_end_matches(" minor")),
                got_key.ends_with("minor"),
            ) {
                (Some((w, Some(wm))), Some((g, _)), gm) => w == g && wm == gm,
                (Some((w, None)), Some((g, _)), _) => w == g,
                _ => false,
            };
            keys_right += ok as usize;
            verdict.push_str(if ok { "key ok " } else { "KEY?? " });
        }
        let progression = s.get("progression").and_then(Value::as_str).unwrap_or("");
        if let Some(want) = label.and_then(|l| l["chords"].as_str()) {
            for (w, got) in want.split_whitespace().zip(progression.split(" | ")) {
                chords += 1;
                // Right if the bar's chords include the labelled root and quality (any extension or
                // inversion).
                let base = |c: &str| c.split('/').next().unwrap_or("").to_owned();
                let quality = |c: &str| {
                    let rest = c.trim_start_matches(|ch: char| ch.is_ascii_uppercase() || ch == '#' || ch == 'b');
                    rest.starts_with('m') && !rest.starts_with("maj")
                };
                let root = |c: &str| {
                    c[..c
                        .find(|ch: char| !(ch.is_ascii_uppercase() || ch == '#' || (ch == 'b' && c.len() > 1)))
                        .unwrap_or(c.len())]
                        .to_owned()
                };
                let ok = got.split(' ').any(|g| {
                    let g = base(g);
                    tonic(&root(&g)).map(|t| t.0) == tonic(&root(w)).map(|t| t.0) && quality(&g) == quality(w)
                });
                chords_right += ok as usize;
            }
        }
        println!(
            "{:5} {verdict:7} {name:36} key={got_key:9} conf={} tuning={tuning:?}\n      {progression}",
            rec.id,
            s.get("key").and_then(|k| k.get("confidence")).map_or("-".into(), |c| c.to_string()),
        );
        if std::env::var("BASS").is_ok() {
            for (bar, fields) in &analysis.bars {
                if let Some(line) = fields.get("bass_line").and_then(Value::as_str) {
                    println!("      bar {bar}: {line}");
                }
            }
        }
    }
    println!("\nkeys right {keys_right}/{keys}, chords right {chords_right}/{chords}");
}
