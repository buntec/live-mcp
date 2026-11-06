//! `transport`: play/stop, tempo, position, loop, and firing clips and scenes.

use anyhow::{Result, bail};
use live_mcp_protocol::Target;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::live::{Live, TrackRef, call, get_props, set};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Only apply the settings given (tempo, position, loop) and report the transport state.
    #[default]
    Status,
    /// Start playback.
    Play,
    /// Resume from the current position.
    Continue,
    Stop,
    /// Launch the clip in `track`/`slot`.
    FireClip,
    /// Stop the clip in `track`/`slot`.
    StopClip,
    /// Launch scene `scene`.
    FireScene,
    StopAllClips,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TransportParams {
    #[serde(default)]
    pub action: Action,
    /// Tempo in BPM (20–999).
    pub tempo: Option<f64>,
    /// Song position in beats. Applied before the action.
    pub position: Option<f64>,
    /// Turn the Arrangement loop on or off.
    pub loop_enabled: Option<bool>,
    /// Loop start in beats.
    pub loop_start: Option<f64>,
    /// Loop length in beats.
    pub loop_length: Option<f64>,
    /// For fire_clip / stop_clip.
    pub track: Option<TrackRef>,
    /// For fire_clip / stop_clip.
    pub slot: Option<usize>,
    /// For fire_scene (0-based).
    pub scene: Option<usize>,
}

pub async fn transport(live: &Live, params: TransportParams) -> Result<Value> {
    let song = Target::path("live_set");
    let mut ops = Vec::new();
    if let Some(tempo) = params.tempo {
        ops.push(set(&song, "tempo", json!(tempo)));
    }
    if let Some(start) = params.loop_start {
        ops.push(set(&song, "loop_start", json!(start)));
    }
    if let Some(length) = params.loop_length {
        ops.push(set(&song, "loop_length", json!(length)));
    }
    if let Some(enabled) = params.loop_enabled {
        ops.push(set(&song, "loop", json!(enabled)));
    }
    if let Some(position) = params.position {
        ops.push(set(&song, "current_song_time", json!(position)));
    }
    match params.action {
        Action::Status => {}
        Action::Play => ops.push(call(&song, "start_playing", vec![])),
        Action::Continue => ops.push(call(&song, "continue_playing", vec![])),
        Action::Stop => ops.push(call(&song, "stop_playing", vec![])),
        Action::StopAllClips => ops.push(call(&song, "stop_all_clips", vec![])),
        Action::FireScene => {
            let Some(scene) = params.scene else { bail!("fire_scene needs `scene`") };
            ops.push(call(&Target::path(format!("live_set scenes {scene}")), "fire", vec![]));
        }
        Action::FireClip | Action::StopClip => {
            let (Some(track), Some(slot)) = (&params.track, params.slot) else {
                bail!("fire_clip and stop_clip need `track` and `slot`")
            };
            let track = track.resolve(live).await?;
            let method = if params.action == Action::FireClip { "fire" } else { "stop" };
            ops.push(call(&track.target.join(&format!("clip_slots {slot}")), method, vec![]));
        }
    }
    ops.push(get_props(&song, &["is_playing", "tempo", "current_song_time", "loop", "loop_start", "loop_length"]));
    let results = live.batch(ops, false).await?;
    let mut state = results.last().cloned().unwrap_or_default();
    // Live applies some of these (e.g. current_song_time) on its next tick, so a read in the
    // same batch can still see the old value. The sets succeeded, so report what was set.
    for (key, value) in [
        ("tempo", params.tempo.map(|v| json!(v))),
        ("current_song_time", params.position.map(|v| json!(v))),
        ("loop", params.loop_enabled.map(|v| json!(v))),
        ("loop_start", params.loop_start.map(|v| json!(v))),
        ("loop_length", params.loop_length.map(|v| json!(v))),
    ] {
        if let Some(value) = value {
            state[key] = value;
        }
    }
    let mut out = json!({
        "playing": state["is_playing"],
        "tempo": state["tempo"],
        "position": state["current_song_time"],
        "loop": {"enabled": state["loop"], "start": state["loop_start"], "length": state["loop_length"]},
    });
    if matches!(params.action, Action::Play | Action::Continue | Action::Stop) {
        out["note"] = json!("`playing` can lag the action by a tick; call transport again to confirm");
    }
    Ok(out)
}
