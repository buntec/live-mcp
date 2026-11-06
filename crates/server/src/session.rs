//! `session`: an overview of the whole Live set in three batched requests.

use anyhow::Result;
use live_mcp_protocol::Target;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::live::{Live, field_bool, field_f64, field_str, get_fields, get_props, items};

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SessionParams {
    /// Also list each track's Arrangement View clips.
    #[serde(default)]
    pub arrangement: bool,
}

pub async fn session(live: &Live, params: SessionParams) -> Result<Value> {
    let song = Target::path("live_set");
    let first = live
        .batch_lenient(vec![
            get_props(
                &song,
                &[
                    "tempo",
                    "signature_numerator",
                    "signature_denominator",
                    "is_playing",
                    "current_song_time",
                    "loop",
                    "loop_start",
                    "loop_length",
                ],
            ),
            get_fields(
                &Target::path("live_set tracks"),
                &["name", "has_midi_input", "is_foldable", "mute", "solo", "arm"],
            ),
            get_fields(&Target::path("live_set return_tracks"), &["name", "mute", "solo"]),
            get_fields(&Target::path("live_set scenes"), &["name"]),
            get_fields(&Target::path("live_set master_track devices"), &["name"]),
        ])
        .await?;
    let [song_props, tracks, returns, scenes, master_devices] =
        <[_; 5]>::try_from(first).map_err(|_| anyhow::anyhow!("malformed session batch"))?;
    let song_props = song_props?;
    let tracks = tracks?;
    let returns = returns.unwrap_or_default();
    let tracks = items(&tracks);
    let returns = items(&returns);

    // Second pass: devices and clip slots of every track.
    let mut ops = Vec::new();
    for i in 0..tracks.len() {
        ops.push(get_fields(&Target::path(format!("live_set tracks {i} devices")), &["name"]));
        ops.push(get_fields(&Target::path(format!("live_set tracks {i} clip_slots")), &["has_clip"]));
        if params.arrangement {
            ops.push(get_fields(
                &Target::path(format!("live_set tracks {i} arrangement_clips")),
                &["name", "start_time", "length"],
            ));
        }
    }
    for r in 0..returns.len() {
        ops.push(get_fields(&Target::path(format!("live_set return_tracks {r} devices")), &["name"]));
    }
    let mut second = live.batch_lenient(ops).await?.into_iter();
    let per_track = if params.arrangement { 3 } else { 2 };
    let mut track_parts = Vec::new();
    for _ in 0..tracks.len() {
        let parts: Vec<Value> =
            (0..per_track).map(|_| second.next().and_then(Result::ok).unwrap_or_default()).collect();
        track_parts.push(parts);
    }
    let return_devices: Vec<Value> =
        (0..returns.len()).map(|_| second.next().and_then(Result::ok).unwrap_or_default()).collect();

    // Third pass: details of every clip in a Session slot.
    let mut clip_ops = Vec::new();
    let mut clip_keys = Vec::new();
    for (i, parts) in track_parts.iter().enumerate() {
        for (j, slot) in items(&parts[1]).iter().enumerate() {
            if field_bool(slot, "has_clip") == Some(true) {
                clip_ops.push(get_fields(
                    &Target::path(format!("live_set tracks {i} clip_slots {j} clip")),
                    &["name", "length", "is_playing", "is_midi_clip"],
                ));
                clip_keys.push((i, j));
            }
        }
    }
    let clips = live.batch_lenient(clip_ops).await?;
    let mut clips_by_track: Vec<Vec<Value>> = vec![Vec::new(); tracks.len()];
    for ((i, j), clip) in clip_keys.into_iter().zip(clips) {
        let clip = clip.unwrap_or_default();
        let mut entry = Map::new();
        entry.insert("slot".into(), json!(j));
        entry.insert("name".into(), json!(field_str(&clip, "name")));
        entry.insert("length".into(), json!(field_f64(&clip, "length")));
        if field_bool(&clip, "is_midi_clip") == Some(false) {
            entry.insert("audio".into(), json!(true));
        }
        if field_bool(&clip, "is_playing") == Some(true) {
            entry.insert("playing".into(), json!(true));
        }
        clips_by_track[i].push(Value::Object(entry));
    }

    let names = |list: &Value| -> Vec<String> { items(list).iter().map(|d| field_str(d, "name").to_owned()).collect() };
    let tracks_out: Vec<Value> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let kind = if field_bool(t, "is_foldable") == Some(true) {
                "group"
            } else if field_bool(t, "has_midi_input") == Some(true) {
                "midi"
            } else {
                "audio"
            };
            let mut entry = Map::new();
            entry.insert("index".into(), json!(i));
            entry.insert("name".into(), json!(field_str(t, "name")));
            entry.insert("kind".into(), json!(kind));
            for flag in ["mute", "solo", "arm"] {
                if field_bool(t, flag) == Some(true) {
                    entry.insert(flag.into(), json!(true));
                }
            }
            entry.insert("devices".into(), json!(names(&track_parts[i][0])));
            entry.insert("clips".into(), Value::Array(std::mem::take(&mut clips_by_track[i])));
            if params.arrangement {
                let arrangement: Vec<Value> = items(&track_parts[i][2])
                    .iter()
                    .map(|c| {
                        json!({
                            "name": field_str(c, "name"),
                            "start": field_f64(c, "start_time"),
                            "length": field_f64(c, "length"),
                        })
                    })
                    .collect();
                entry.insert("arrangement_clips".into(), json!(arrangement));
            }
            Value::Object(entry)
        })
        .collect();
    let returns_out: Vec<Value> = returns
        .iter()
        .enumerate()
        .map(|(r, t)| json!({"index": r, "name": field_str(t, "name"), "devices": names(&return_devices[r])}))
        .collect();
    let scenes_out: Vec<String> = scenes.map(|s| names(&s)).unwrap_or_default();

    let num = song_props["signature_numerator"].as_i64().unwrap_or(4);
    let den = song_props["signature_denominator"].as_i64().unwrap_or(4);
    Ok(json!({
        "tempo": song_props["tempo"],
        "time_signature": format!("{num}/{den}"),
        "playing": song_props["is_playing"],
        "position": song_props["current_song_time"],
        "loop": {"enabled": song_props["loop"], "start": song_props["loop_start"], "length": song_props["loop_length"]},
        "tracks": tracks_out,
        "returns": returns_out,
        "master": {"devices": master_devices.map(|d| names(&d)).unwrap_or_default()},
        "scenes": scenes_out,
    }))
}

/// Beats per bar for the set's time signature.
pub async fn beats_per_bar(live: &Live) -> Result<f64> {
    let song = live
        .request("get", json!({"target": "live_set", "props": ["signature_numerator", "signature_denominator"]}))
        .await?;
    let num = song["signature_numerator"].as_f64().unwrap_or(4.0);
    let den = song["signature_denominator"].as_f64().unwrap_or(4.0);
    Ok(num * 4.0 / den)
}
