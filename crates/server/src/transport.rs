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
    /// Start playback: from `position` if given, else from the song start.
    Play,
    /// Resume where playback last stopped, or play from `position` if given.
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
    /// Song position in beats. With play/continue, playback starts there. While stopped and without
    /// an action, Live shows the position but resumes from where it last stopped.
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

/// Starts playback at `beat`. Measured on Live 12.4:
/// - `start_playing` always starts from the song start;
/// - `continue_playing` resumes where playback last stopped, ignoring a position set while stopped;
/// - setting `current_song_time` while playing jumps there;
/// - all of these take effect on a later tick, and `is_playing` reads stale until then.
///
/// So: start playback, let a tick pass, jump, let a tick pass, and check. Up to a tick of audio
/// from the song start can be heard before the jump.
pub async fn play_from(live: &Live, beat: f64) -> Result<()> {
    check_beat(beat)?;
    let song = Target::path("live_set");
    live.batch(vec![call(&song, "start_playing", vec![])], false).await?;
    live.wait_tick().await?;
    for _attempt in 0..3 {
        live.batch(vec![set(&song, "current_song_time", json!(beat))], false).await?;
        live.wait_tick().await?;
        let state =
            live.request("get", json!({"target": "live_set", "props": ["is_playing", "current_song_time"]})).await?;
        let now = state["current_song_time"].as_f64().unwrap_or(f64::NAN);
        // Playing moves the position on from the target straight away.
        if state["is_playing"] == true && (beat - 0.01..beat + 2.0).contains(&now) {
            return Ok(());
        }
    }
    bail!("Live did not start playing at beat {beat}")
}

/// Sets the song position and waits until Live has applied it. While playing this jumps;
/// while stopped Live shows the new position, but continue_playing still resumes where
/// playback last stopped (use [`play_from`] to play from a position).
pub async fn set_position(live: &Live, beat: f64) -> Result<()> {
    check_beat(beat)?;
    let song = Target::path("live_set");
    live.batch(vec![set(&song, "current_song_time", json!(beat))], false).await?;
    live.wait_tick().await
}

fn check_beat(beat: f64) -> Result<()> {
    if !(beat.is_finite() && beat >= 0.0) {
        bail!("position must be ≥ 0 beats");
    }
    Ok(())
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
    let starts_playback = matches!(params.action, Action::Play | Action::Continue);
    if let Some(position) = params.position {
        live.batch(std::mem::take(&mut ops), false).await?;
        if starts_playback {
            play_from(live, position).await?;
        } else {
            set_position(live, position).await?;
        }
    }
    match params.action {
        Action::Status => {}
        _ if starts_playback && params.position.is_some() => {} // already playing from `position`
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
    let changed = !ops.is_empty() || params.position.is_some();
    live.batch(ops, false).await?;
    // Live applies transport changes on a later tick; read the state after one has passed.
    if changed {
        live.wait_tick().await?;
    }
    let state = live
        .batch(
            vec![get_props(&song, &["is_playing", "tempo", "current_song_time", "loop", "loop_start", "loop_length"])],
            false,
        )
        .await?
        .remove(0);
    let out = json!({
        "playing": state["is_playing"],
        "tempo": state["tempo"],
        "position": state["current_song_time"],
        "loop": {"enabled": state["loop"], "start": state["loop_start"], "length": state["loop_length"]},
    });
    Ok(out)
}
