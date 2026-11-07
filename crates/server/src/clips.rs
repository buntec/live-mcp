//! `write_clip` and `read_clip`: MIDI clips in Session slots or on the Arrangement timeline.

use anyhow::{Result, bail};
use live_mcp_protocol::{Target, new_live_object, tuple};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::harmony::midi::{self, MidiNote};
use crate::harmony::{self, BarSpan, Section, SetScale, Source};
use crate::live::{
    Live, TrackRef, call, call_fields, default_true, field, field_bool, field_f64, get_fields, get_props, items, set,
};
use crate::session::beats_per_bar;

/// Notes are cleared over this span, which covers any realistic clip.
const ALL_TIME: f64 = 1_000_000.0;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Note {
    /// MIDI pitch 0–127 (60 = middle C).
    pub pitch: u8,
    /// Start in beats from the clip start.
    pub start: f64,
    /// Length in beats.
    pub duration: f64,
    /// 1–127. Default 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    /// Chance that the note plays, 0–1. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
    /// Random velocity range, −127–127. Default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_deviation: Option<f64>,
    /// Note-off velocity, 0–127. Default 64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_velocity: Option<f64>,
}

impl Note {
    fn validate(&self, index: usize) -> Result<()> {
        let fail = |what: &str| bail!("note {index}: {what}");
        if self.pitch > 127 {
            return fail("pitch must be 0–127");
        }
        if !(self.start.is_finite() && self.start >= 0.0) {
            return fail("start must be ≥ 0");
        }
        if !(self.duration.is_finite() && self.duration > 0.0) {
            return fail("duration must be > 0");
        }
        if let Some(v) = self.velocity
            && !(1.0..=127.0).contains(&v)
        {
            return fail("velocity must be 1–127");
        }
        if let Some(p) = self.probability
            && !(0.0..=1.0).contains(&p)
        {
            return fail("probability must be 0–1");
        }
        Ok(())
    }

    fn to_spec(&self) -> Value {
        let mut kwargs = Map::new();
        kwargs.insert("pitch".into(), json!(self.pitch));
        kwargs.insert("start_time".into(), json!(self.start));
        kwargs.insert("duration".into(), json!(self.duration));
        kwargs.insert("velocity".into(), json!(self.velocity.unwrap_or(100.0)));
        if let Some(mute) = self.mute {
            kwargs.insert("mute".into(), json!(mute));
        }
        for (key, value) in [
            ("probability", self.probability),
            ("velocity_deviation", self.velocity_deviation),
            ("release_velocity", self.release_velocity),
        ] {
            if let Some(v) = value {
                kwargs.insert(key.into(), json!(v));
            }
        }
        new_live_object("Clip.MidiNoteSpecification", Value::Object(kwargs))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteClipParams {
    /// Must be a MIDI track.
    pub track: TrackRef,
    /// Session View clip slot (scene row), 0-based. Give either this or `arrangement_start`.
    pub slot: Option<usize>,
    /// Arrangement View position in beats. A new clip is created there.
    pub arrangement_start: Option<f64>,
    /// Clip length in beats, used when a clip is created. Default: the notes' extent rounded up to
    /// whole bars, at least one bar.
    pub length: Option<f64>,
    pub name: Option<String>,
    pub notes: Vec<Note>,
    /// If the slot already holds a clip: true (default) replaces its notes, false adds to them.
    #[serde(default = "default_true")]
    pub replace: bool,
}

pub async fn write_clip(live: &Live, params: WriteClipParams) -> Result<Value> {
    for (i, note) in params.notes.iter().enumerate() {
        note.validate(i)?;
    }
    let track = params.track.resolve(live).await?;
    let length = match params.length {
        Some(l) if l.is_finite() && l > 0.0 => l,
        Some(_) => bail!("length must be > 0"),
        None => {
            let bar = beats_per_bar(live).await?;
            let end = params.notes.iter().map(|n| n.start + n.duration).fold(0.0_f64, f64::max);
            (end / bar).ceil().max(1.0) * bar
        }
    };
    let specs: Vec<Value> = params.notes.iter().map(Note::to_spec).collect();

    let (clip, mut ops, location) = match (params.slot, params.arrangement_start) {
        (Some(_), Some(_)) => bail!("give either slot or arrangement_start, not both"),
        (None, None) => bail!("give slot (Session View) or arrangement_start (Arrangement View)"),
        (Some(slot), None) => {
            let slot_target = track.target.join(&format!("clip_slots {slot}"));
            let has_clip = live.get(&slot_target.join("has_clip")).await?;
            if has_clip == true {
                let clip = slot_target.join("clip");
                let ops = if params.replace {
                    vec![call(&clip, "remove_notes_extended", vec![json!(0), json!(128), json!(0.0), json!(ALL_TIME)])]
                } else {
                    vec![]
                };
                (clip, ops, json!({"slot": slot}))
            } else {
                let ops = vec![call(&slot_target, "create_clip", vec![json!(length)])];
                (Target::batch_result(0), ops, json!({"slot": slot}))
            }
        }
        (None, Some(start)) => {
            if !(start.is_finite() && start >= 0.0) {
                bail!("arrangement_start must be ≥ 0");
            }
            let ops = vec![call(&track.target, "create_midi_clip", vec![json!(start), json!(length)])];
            (Target::batch_result(0), ops, json!({"arrangement_start": start}))
        }
    };
    if let Some(name) = &params.name {
        ops.push(set(&clip, "name", json!(name)));
    }
    if !specs.is_empty() {
        ops.push(call(&clip, "add_new_notes", vec![tuple(specs)]));
    }
    ops.push(get_props(&clip, &["name", "length"]));
    let results = live.batch(ops, true).await?;
    let summary = results.last().cloned().unwrap_or_default();

    let mut out = Map::new();
    out.insert("track".into(), track.label());
    if let Value::Object(loc) = location {
        out.extend(loc);
    }
    out.insert("name".into(), summary["name"].clone());
    out.insert("length".into(), summary["length"].clone());
    out.insert("notes_written".into(), json!(params.notes.len()));
    Ok(Value::Object(out))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadClipParams {
    pub track: TrackRef,
    /// Session View clip slot, 0-based. Give either this or `arrangement_index`.
    pub slot: Option<usize>,
    /// Index into the track's Arrangement clips, in timeline order (see `session` with arrangement).
    pub arrangement_index: Option<usize>,
    /// Include the notes (default true).
    #[serde(default = "default_true")]
    pub notes: bool,
    /// For MIDI clips, add a musical reading of the notes (default true): key, chords per bar,
    /// bass, pitch classes, a 16th-note rhythm grid, range and fit with the set's scale.
    #[serde(default = "default_true")]
    pub music: bool,
}

const CLIP_FIELDS: &[&str] = &[
    "name",
    "length",
    "is_midi_clip",
    "is_playing",
    "looping",
    "loop_start",
    "loop_end",
    "start_marker",
    "end_marker",
    "start_time",
    "signature_numerator",
    "signature_denominator",
];

const NOTE_FIELDS: &[&str] =
    &["pitch", "start_time", "duration", "velocity", "mute", "probability", "velocity_deviation", "release_velocity"];

pub async fn read_clip(live: &Live, params: ReadClipParams) -> Result<Value> {
    let track = params.track.resolve(live).await?;
    let (clip, mut out) = match (params.slot, params.arrangement_index) {
        (Some(slot), None) => (track.target.join(&format!("clip_slots {slot} clip")), json!({"slot": slot})),
        (None, Some(index)) => {
            (track.target.join(&format!("arrangement_clips {index}")), json!({"arrangement_index": index}))
        }
        _ => bail!("give exactly one of slot or arrangement_index"),
    };
    out["track"] = track.label();
    let info = live.get_fields(&clip, CLIP_FIELDS).await?;
    if info.is_null() {
        bail!("there is no clip there");
    }
    for (key, name) in [
        ("name", "name"),
        ("length", "length"),
        ("looping", "looping"),
        ("loop_start", "loop_start"),
        ("loop_end", "loop_end"),
        ("start_marker", "start_marker"),
        ("end_marker", "end_marker"),
    ] {
        if let Some(v) = field(&info, name) {
            out[key] = v.clone();
        }
    }
    if params.arrangement_index.is_some() {
        out["start"] = json!(field_f64(&info, "start_time"));
    }
    if field_bool(&info, "is_playing") == Some(true) {
        out["playing"] = json!(true);
    }
    let midi = field_bool(&info, "is_midi_clip") != Some(false);
    out["kind"] = json!(if midi { "midi" } else { "audio" });
    if midi && (params.notes || params.music) {
        let span = [field_f64(&info, "length"), field_f64(&info, "loop_end"), field_f64(&info, "end_marker")]
            .into_iter()
            .flatten()
            .fold(0.0_f64, f64::max)
            + 1.0;
        let results = live
            .batch(
                vec![call_fields(
                    &clip,
                    "get_notes_extended",
                    vec![json!(0), json!(128), json!(0.0), json!(span)],
                    NOTE_FIELDS,
                )],
                false,
            )
            .await?;
        let raw = items(&results[0]);
        if params.notes {
            out["notes"] = json!(raw.iter().map(note_out).collect::<Vec<_>>());
        }
        if params.music {
            let (scale, drums) = music_context(live, &track.target).await;
            out["music"] = clip_music(&info, raw, scale.as_ref(), drums);
        }
    }
    Ok(out)
}

/// The set's scale (Live 12), or `None` if Live has none.
pub(crate) async fn set_scale(live: &Live) -> Option<SetScale> {
    let song = Target::path("live_set");
    let results = live.batch_lenient(vec![get_props(&song, &["root_note", "scale_name", "scale_intervals"])]).await;
    let props = results.ok()?.into_iter().next()?.ok()?;
    SetScale::from_song(props.get("root_note"), props.get("scale_name"), props.get("scale_intervals"))
}

/// The set's scale (Live 12; `None` if Live has none), and whether the track plays a Drum Rack,
/// whose pitches are pads rather than notes.
async fn music_context(live: &Live, track: &Target) -> (Option<SetScale>, bool) {
    let song = Target::path("live_set");
    let ops = vec![
        get_props(&song, &["root_note", "scale_name", "scale_intervals"]),
        get_fields(&track.join("devices"), &["can_have_drum_pads"]),
    ];
    let Ok(results) = live.batch_lenient(ops).await else { return (None, false) };
    let mut results = results.into_iter();
    let scale = results.next().and_then(Result::ok).and_then(|props| {
        SetScale::from_song(props.get("root_note"), props.get("scale_name"), props.get("scale_intervals"))
    });
    let drums = results
        .next()
        .and_then(Result::ok)
        .is_some_and(|devices| items(&devices).iter().any(|d| field_bool(d, "can_have_drum_pads") == Some(true)));
    (scale, drums)
}

/// Interprets the notes that play: the loop if the clip loops, else start to end marker. Bars
/// count from the clip's beat 0 (bar 1), in the clip's time signature. For a Drum Rack only the
/// rhythm means anything.
fn clip_music(info: &Value, raw_notes: &[Value], scale: Option<&SetScale>, drums: bool) -> Value {
    let get = |name| field_f64(info, name);
    let (from, to) = if field_bool(info, "looping") == Some(true) {
        (get("loop_start").unwrap_or(0.0), get("loop_end").or(get("length")).unwrap_or(0.0))
    } else {
        (get("start_marker").unwrap_or(0.0), get("end_marker").or(get("length")).unwrap_or(0.0))
    };
    let numerator = get("signature_numerator").filter(|n| *n > 0.0).unwrap_or(4.0);
    let denominator = get("signature_denominator").filter(|d| *d > 0.0).unwrap_or(4.0);
    let beats_per_bar = numerator * 4.0 / denominator;
    let notes: Vec<MidiNote> = raw_notes
        .iter()
        .filter(|n| field_bool(n, "mute") != Some(true))
        .filter_map(|n| {
            Some(MidiNote {
                pitch: field_f64(n, "pitch")?.clamp(0.0, 127.0) as u8,
                start: field_f64(n, "start_time")?,
                duration: field_f64(n, "duration")?,
                velocity: field_f64(n, "velocity").unwrap_or(100.0),
            })
        })
        .collect();
    let mut beats = midi::beats(&notes, from, to);
    if drums {
        for beat in &mut beats {
            *beat =
                harmony::Beat { index: beat.index, coverage: beat.coverage, onsets: beat.onsets, ..Default::default() };
        }
    }
    let first_bar = (from / beats_per_bar + 1e-9).floor() as i64;
    let last_bar = ((to / beats_per_bar - 1e-9).ceil() as i64).max(first_bar + 1);
    let bars: Vec<BarSpan> = (first_bar..last_bar)
        .map(|k| BarSpan { bar: k + 1, start_beat: k as f64 * beats_per_bar, beats: beats_per_bar })
        .collect();
    let analysis = harmony::analyze(&Section { bars: &bars, beats: &beats, source: Source::Midi, scale });
    let mut music = analysis.summary;
    music.insert("region".into(), json!([from, to]));
    if drums {
        music.insert("drum_rack".into(), json!(true));
    }
    let bars: Vec<Value> = analysis
        .bars
        .into_iter()
        .map(|(bar, fields)| {
            let mut entry = Map::new();
            entry.insert("bar".into(), json!(bar));
            entry.extend(fields);
            Value::Object(entry)
        })
        .collect();
    music.insert("bars".into(), json!(bars));
    Value::Object(music)
}

/// A note as tools report it: defaults left out to keep responses small.
fn note_out(note: &Value) -> Value {
    let mut out = Map::new();
    out.insert("pitch".into(), json!(field(note, "pitch")));
    out.insert("start".into(), json!(field(note, "start_time")));
    out.insert("duration".into(), json!(field(note, "duration")));
    out.insert("velocity".into(), json!(field(note, "velocity")));
    if field_bool(note, "mute") == Some(true) {
        out.insert("mute".into(), json!(true));
    }
    for (name, default) in [("probability", 1.0), ("velocity_deviation", 0.0), ("release_velocity", 64.0)] {
        if let Some(v) = field_f64(note, name)
            && (v - default).abs() > 1e-9
        {
            out.insert(name.into(), json!(v));
        }
    }
    Value::Object(out)
}
