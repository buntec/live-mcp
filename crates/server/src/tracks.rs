//! `create_track`.

use anyhow::{Result, bail};
use live_mcp_protocol::Target;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::live::{Live, call, set};

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NewTrackKind {
    Midi,
    Audio,
    Return,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTrackParams {
    /// "midi", "audio" or "return".
    pub kind: NewTrackKind,
    /// Position among the regular tracks (0-based). Omit to append. Ignored for return tracks.
    pub index: Option<usize>,
    pub name: Option<String>,
}

pub async fn create_track(live: &Live, params: CreateTrackParams) -> Result<Value> {
    let song = Target::path("live_set");
    let (list, method, args) = match params.kind {
        NewTrackKind::Midi => ("live_set tracks", "create_midi_track", true),
        NewTrackKind::Audio => ("live_set tracks", "create_audio_track", true),
        NewTrackKind::Return => ("live_set return_tracks", "create_return_track", false),
    };
    let count = live.count(&Target::path(list)).await?;
    let index = match params.kind {
        NewTrackKind::Return => count,
        _ => params.index.unwrap_or(count),
    };
    if index > count {
        bail!("index {index} is past the end: the set has {count} tracks");
    }
    let mut ops =
        vec![call(&song, method, if args { vec![json!(params.index.map_or(-1, |i| i as i64))] } else { vec![] })];
    let track = Target::path(format!("{list} {index}"));
    if let Some(name) = &params.name {
        ops.push(set(&track, "name", json!(name)));
    }
    live.batch(ops, true).await?;
    let name = live.get(&track.join("name")).await?;
    let label = match params.kind {
        NewTrackKind::Return => json!(format!("return {index}")),
        _ => json!(index),
    };
    Ok(json!({"track": label, "name": name}))
}
