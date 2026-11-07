//! Listening through LiveMCP Ear plugins: the hub that plugins connect to, matching plugin
//! instances to tracks, and the `listen` and `ears` tools.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use live_mcp_analysis::{BAND_NAMES, BarReport, BeatReport};
use live_mcp_bridge::paths;
use live_mcp_protocol::Target;
use live_mcp_protocol::ear::{
    EAR_DEFAULT_PORT, EAR_MIN_PROTOCOL_VERSION, EAR_PROTOCOL_VERSION, EarEndpoint, EarMessage, TAG_MAX,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::harmony::{self, BarSpan, Section, SetScale, Source, ensemble};
use crate::live::{
    Live, Track, TrackKind, TrackRef, call, default_true, field_f64, field_str, get_fields, get_props, items, set,
};
use crate::masking;
use crate::snapshots::{Kind, SnapshotMusic, SnapshotStore};

/// The plugin's name in Live's browser and device chain.
pub const EAR_NAME: &str = "LiveMCP Ear";
pub const EAR_BROWSER_PATH: [&str; 3] = ["plugins", "live-mcp", EAR_NAME];
/// Bars kept per instance.
const HISTORY: usize = 1024;
/// Beats kept per instance.
const BEAT_HISTORY: usize = 4096;
/// How long to wait for plugins to report a newly written tag.
const TAG_WAIT: Duration = Duration::from_millis(1500);
/// Longest `listen` call, so MCP clients do not time out.
const MAX_LISTEN: Duration = Duration::from_secs(60);
const MAX_LINE: usize = 1 << 20;
/// Tuning offsets smaller than this (cents) are not worth mentioning.
const TUNING_NOTE_CENTS: f32 = 10.0;
const OLD_EAR_NOTE: &str = "this LiveMCP Ear is an older build that reports levels only, not pitch or rhythm; \
     the user can run `live-mcp install`, then reload the plugin (or restart Live)";

#[derive(Debug, Clone)]
pub struct Instance {
    pub id: u64,
    pub tag: u32,
    pub playing: bool,
    pub connected: bool,
    pub tag_seen_at: Instant,
    pub sample_rate: f32,
    pub version: String,
    /// The plugin's protocol: 2 and up also report beats (pitch and rhythm).
    pub protocol: u32,
    bars: VecDeque<(Instant, BarReport)>,
    beats: VecDeque<(Instant, BeatReport)>,
}

impl Instance {
    /// Whether the plugin reports beats, which `listen` turns into key, chords and rhythm.
    pub fn hears_music(&self) -> bool {
        self.protocol >= 2
    }
}

/// Accepts connections from plugin instances and keeps what they report.
#[derive(Default)]
pub struct EarHub {
    instances: Mutex<HashMap<u64, Instance>>,
    changed: Notify,
    listening: OnceLock<Result<u16, String>>,
    state_dir: OnceLock<PathBuf>,
}

impl EarHub {
    /// Listens on 127.0.0.1 (port from `config.json` `ear_port`, default 19419) and advertises
    /// the port in `ear.json`. Failure is recorded, not fatal: the other tools still work.
    pub async fn start(self: &Arc<Self>, state_dir: &Path) {
        let _ = self.state_dir.set(state_dir.to_owned());
        let result = self.bind(state_dir).await;
        let _ = self.listening.set(result.map_err(|e| format!("{e:#}")));
    }

    async fn bind(self: &Arc<Self>, state_dir: &Path) -> Result<u16> {
        let token = paths::ensure_token(state_dir).context("reading the token")?;
        let port = std::fs::read_to_string(state_dir.join("config.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|config| config.get("ear_port").and_then(Value::as_u64))
            .map_or(EAR_DEFAULT_PORT, |p| p as u16);
        let listener = TcpListener::bind(("127.0.0.1", port)).await.with_context(|| {
            format!("listening for LiveMCP Ear plugins on 127.0.0.1:{port} (is another live-mcp server running?)")
        })?;
        let port = listener.local_addr()?.port();
        write_endpoint(state_dir, port)?;
        let hub = self.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let hub = hub.clone();
                let token = token.clone();
                tokio::spawn(async move { hub.serve_plugin(socket, &token).await });
            }
        });
        Ok(port)
    }

    /// Where `listen` records beats, per `config.json`'s `record_beats` (see [`recording_dir`]).
    /// The file is read on every call, so recording can be switched on and off while the server
    /// runs.
    fn recording_dir(&self) -> Option<PathBuf> {
        let state_dir = self.state_dir.get()?;
        let config: Value = serde_json::from_str(&std::fs::read_to_string(state_dir.join("config.json")).ok()?).ok()?;
        recording_dir(&config, state_dir)
    }

    pub fn listening(&self) -> Result<u16, String> {
        self.listening.get().cloned().unwrap_or_else(|| Err("not started".into()))
    }

    async fn serve_plugin(&self, socket: TcpStream, token: &str) {
        let _ = socket.set_nodelay(true);
        let mut lines = BufReader::new(socket).lines();
        let Ok(Some(first)) = lines.next_line().await else { return };
        let id = match serde_json::from_str::<EarMessage>(&first) {
            Ok(EarMessage::Hello { token: given, protocol, instance, plugin_version, sample_rate })
                if tokens_match(&given, token)
                    && (EAR_MIN_PROTOCOL_VERSION..=EAR_PROTOCOL_VERSION).contains(&protocol) =>
            {
                let mut instances = self.instances.lock().expect("hub lock");
                let entry = instances.entry(instance).or_insert_with(|| Instance {
                    id: instance,
                    tag: 0,
                    playing: false,
                    connected: true,
                    tag_seen_at: Instant::now(),
                    sample_rate,
                    version: plugin_version.clone(),
                    protocol,
                    bars: VecDeque::new(),
                    beats: VecDeque::new(),
                });
                entry.connected = true;
                entry.sample_rate = sample_rate;
                entry.version = plugin_version;
                entry.protocol = protocol;
                instance
            }
            _ => return, // wrong token, protocol or first message: drop the connection
        };
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() > MAX_LINE {
                break;
            }
            let Ok(message) = serde_json::from_str::<EarMessage>(&line) else { continue };
            {
                let mut instances = self.instances.lock().expect("hub lock");
                let Some(instance) = instances.get_mut(&id) else { break };
                match message {
                    EarMessage::Heartbeat { tag, playing, .. } => {
                        if tag != instance.tag {
                            instance.tag = tag;
                        }
                        instance.tag_seen_at = Instant::now();
                        instance.playing = playing;
                    }
                    EarMessage::Bar { tag, report } => {
                        instance.tag = tag;
                        if instance.bars.len() == HISTORY {
                            instance.bars.pop_front();
                        }
                        instance.bars.push_back((Instant::now(), report));
                    }
                    EarMessage::Beat { tag, report } => {
                        instance.tag = tag;
                        if instance.beats.len() == BEAT_HISTORY {
                            instance.beats.pop_front();
                        }
                        instance.beats.push_back((Instant::now(), report));
                    }
                    EarMessage::Hello { .. } => {}
                }
            }
            self.changed.notify_waiters();
        }
        if let Some(instance) = self.instances.lock().expect("hub lock").get_mut(&id) {
            instance.connected = false;
        }
        self.changed.notify_waiters();
    }

    fn snapshot(&self) -> Vec<Instance> {
        self.instances.lock().expect("hub lock").values().cloned().collect()
    }

    /// The connected instance that most recently reported `tag`, if it did so after `since`.
    fn by_tag(&self, tag: u32, since: Instant) -> Option<Instance> {
        self.snapshot()
            .into_iter()
            .filter(|i| i.connected && i.tag == tag && i.tag_seen_at >= since)
            .max_by_key(|i| i.tag_seen_at)
    }

    fn bars_since(&self, instance: u64, since: Instant) -> Vec<BarReport> {
        let instances = self.instances.lock().expect("hub lock");
        instances
            .get(&instance)
            .map(|i| i.bars.iter().filter(|(at, _)| *at >= since).map(|(_, r)| *r).collect())
            .unwrap_or_default()
    }

    fn beats_since(&self, instance: u64, since: Instant) -> Vec<BeatReport> {
        let instances = self.instances.lock().expect("hub lock");
        instances
            .get(&instance)
            .map(|i| i.beats.iter().filter(|(at, _)| *at >= since).map(|(_, r)| *r).collect())
            .unwrap_or_default()
    }

    async fn changed_or(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.changed.notified()).await;
    }
}

/// `record_beats`: `true` records into `recordings` in the state folder; a string names the folder
/// (relative to the state folder unless absolute); anything else, or no key, records nothing.
fn recording_dir(config: &Value, state_dir: &Path) -> Option<PathBuf> {
    let dir = match config.get("record_beats")? {
        Value::Bool(true) => PathBuf::from("recordings"),
        Value::String(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => return None,
    };
    Some(if dir.is_absolute() { dir } else { state_dir.join(dir) })
}

fn write_endpoint(state_dir: &Path, port: u16) -> Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let path: PathBuf = state_dir.join("ear.json");
    let tmp = state_dir.join("ear.json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&EarEndpoint { port, pid: std::process::id() })?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

// --------------------------------------------------------------- tag binding

/// A LiveMCP Ear device in the set, and the plugin instance behind it if it has reported in.
#[derive(Debug, Clone)]
pub struct Binding {
    pub track: Track,
    pub device: usize,
    pub tag: u32,
    pub instance: Option<Instance>,
}

/// Finds every LiveMCP Ear device in the set, gives each a unique tag through its `Tag`
/// parameter (keeping tags that are already unique), and matches devices to connected plugin
/// instances by the tags they report.
pub async fn bind(live: &Live, hub: &EarHub) -> Result<Vec<Binding>> {
    let mut tracks: Vec<Track> = Vec::new();
    let regular = live.count(&Target::path("live_set tracks")).await?;
    let returns = live.count(&Target::path("live_set return_tracks")).await?;
    tracks.extend((0..regular).map(Track::regular));
    tracks.extend((0..returns).map(Track::return_track));
    tracks.push(Track::master());
    let ops = tracks.iter().map(|t| get_fields(&t.target.join("devices"), &["name", "class_name"])).collect();
    let devices = live.batch_lenient(ops).await?;

    let mut ears: Vec<(Track, usize)> = Vec::new();
    for (track, list) in tracks.iter().zip(devices) {
        for (index, device) in items(&list.unwrap_or_default()).iter().enumerate() {
            if field_str(device, "class_name") == "PluginDevice" && field_str(device, "name").starts_with(EAR_NAME) {
                ears.push((track.clone(), index));
            }
        }
    }
    if ears.is_empty() {
        return Ok(Vec::new());
    }

    // Each Ear's Tag parameter: index and current value.
    let ops = ears
        .iter()
        .map(|(t, d)| get_fields(&t.target.join(&format!("devices {d} parameters")), &["name", "value", "min", "max"]))
        .collect();
    let params = live.batch_lenient(ops).await?;
    let mut tags: Vec<(Target, Option<u32>, f64, f64)> = Vec::new();
    for ((track, device), list) in ears.iter().zip(params) {
        let list = list.unwrap_or_default();
        let Some((index, param)) = items(&list).iter().enumerate().find(|(_, p)| field_str(p, "name") == "Tag") else {
            bail!(
                "the {EAR_NAME} on track {} has no Tag parameter in Live; update the plugin, or check that Live \
                 shows its parameters (device menu → Configure)",
                track.label()
            );
        };
        let (min, max) = (field_f64(param, "min").unwrap_or(0.0), field_f64(param, "max").unwrap_or(1.0));
        let value = field_f64(param, "value").unwrap_or(min);
        let tag = (((value - min) / (max - min)) * TAG_MAX as f64).round() as u32;
        let target = track.target.join(&format!("devices {device} parameters {index}"));
        tags.push((target, (tag > 0).then_some(tag), min, max));
    }

    // Keep unique non-zero tags; give the rest the smallest free ones.
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for (_, tag, _, _) in &tags {
        if let Some(t) = tag {
            *counts.entry(*t).or_default() += 1;
        }
    }
    let mut used: HashSet<u32> = counts.iter().filter(|(_, n)| **n == 1).map(|(t, _)| *t).collect();
    let mut next_free = 1u32;
    let mut writes = Vec::new();
    let mut assigned = Vec::new();
    for (target, tag, min, max) in &tags {
        let tag = match tag {
            Some(t) if counts[t] == 1 => *t,
            _ => {
                while used.contains(&next_free) {
                    next_free += 1;
                }
                if next_free > TAG_MAX {
                    bail!("more than {TAG_MAX} {EAR_NAME} instances");
                }
                used.insert(next_free);
                let value = min + (max - min) * next_free as f64 / TAG_MAX as f64;
                writes.push(set(target, "value", json!(value)));
                next_free
            }
        };
        assigned.push(tag);
    }
    let written_at = Instant::now();
    let rewrote = !writes.is_empty();
    if rewrote {
        live.batch(writes, true).await?;
    }

    // Wait until each tag has been reported (after the write, if there was one).
    let since = if rewrote { written_at } else { Instant::now() - Duration::from_secs(3) };
    let deadline = Instant::now() + TAG_WAIT;
    loop {
        let found: Vec<Option<Instance>> = assigned.iter().map(|t| hub.by_tag(*t, since)).collect();
        if found.iter().all(Option::is_some) || Instant::now() >= deadline {
            return Ok(ears
                .into_iter()
                .zip(assigned)
                .zip(found)
                .map(|(((track, device), tag), instance)| Binding { track, device, tag, instance })
                .collect());
        }
        hub.changed_or(Duration::from_millis(100)).await;
    }
}

// --------------------------------------------------------------- the tools

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct EarsParams {}

pub async fn ears(live: &Live, hub: &EarHub, _params: EarsParams) -> Result<Value> {
    let listening = hub.listening();
    let bindings = bind(live, hub).await?;
    let list: Vec<Value> = bindings
        .iter()
        .map(|b| {
            let mut entry = json!({"track": b.track.label(), "device": b.device, "tag": b.tag});
            match &b.instance {
                Some(i) => {
                    entry["connected"] = json!(true);
                    entry["sample_rate"] = json!(i.sample_rate);
                    entry["music"] = json!(i.hears_music());
                    if let Some((_, last)) = i.bars.back() {
                        entry["last_bar"] = compact_bar(last);
                    }
                }
                None => entry["connected"] = json!(false),
            }
            entry
        })
        .collect();
    let mut out = json!({"ears": list});
    match listening {
        Ok(port) => out["listening_on"] = json!(port),
        Err(e) => out["error"] = json!(e),
    }
    if bindings.is_empty() {
        out["hint"] = json!(format!(
            "no {EAR_NAME} in the set; add one with load_device and path {EAR_BROWSER_PATH:?}. If Live's \
                 browser doesn't have it, the user needs to run `live-mcp install` and restart Live"
        ));
    } else if bindings.iter().any(|b| b.instance.as_ref().is_some_and(|i| !i.hears_music())) {
        out["hint"] = json!(OLD_EAR_NOTE);
    } else if bindings.iter().any(|b| b.instance.is_none()) {
        out["hint"] = json!(
            "a device that is not connected may be from an older plugin build, or it may report to another \
             live-mcp server"
        );
    }
    Ok(out)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListenParams {
    /// Track whose LiveMCP Ear to listen through (default "master"). The plugin must be on it.
    pub track: Option<TrackRef>,
    /// Listen through several Ears in one playback: a list of tracks, or ["all"] for every
    /// connected Ear. Adds a masking analysis: which tracks share each frequency band.
    /// Replaces `track`.
    pub tracks: Option<Vec<TrackRef>>,
    /// First bar (1-based, as Live shows it). Default: the bar at the current song position.
    pub start_bar: Option<i64>,
    /// Number of bars, default 4. Listening happens in real time, so at most 60 seconds.
    pub bars: Option<u32>,
    /// Play from start_bar, then stop (default true). With false, only collect bars while
    /// playback is controlled elsewhere.
    #[serde(default = "default_true")]
    pub play: bool,
    /// "all" (default): mix measurements and music; "mix": levels, bands and stereo only;
    /// "music": key, chords, bass, pitch classes, rhythm and range only.
    #[serde(default)]
    pub focus: Focus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Focus {
    #[default]
    All,
    Mix,
    Music,
}

/// The best report per wanted beat that an instance sent since `mark`.
fn collect_beats(
    hub: &EarHub,
    instance: u64,
    mark: Instant,
    wanted: &std::ops::Range<i64>,
) -> BTreeMap<i64, BeatReport> {
    let mut heard: BTreeMap<i64, BeatReport> = BTreeMap::new();
    for report in hub.beats_since(instance, mark) {
        if wanted.contains(&report.beat) && heard.get(&report.beat).is_none_or(|old| report.coverage >= old.coverage) {
            heard.insert(report.beat, report);
        }
    }
    heard
}

/// What one Ear (or one file) held musically.
pub(crate) struct Music {
    pub summary: Map<String, Value>,
    pub bars: BTreeMap<i64, Map<String, Value>>,
    pub beats: Vec<harmony::Beat>,
}

impl Music {
    pub fn snapshot(&self) -> SnapshotMusic {
        SnapshotMusic { summary: self.summary.clone(), bars: self.bars.clone() }
    }
}

fn spans(reports: &[&BarReport]) -> Vec<BarSpan> {
    reports.iter().map(|r| BarSpan { bar: r.bar, start_beat: r.start_beat, beats: r.beats }).collect()
}

/// With `record_beats` set in `config.json`, saves what one Ear heard (bars and beat reports) as
/// `<snapshot>.json`, for tuning and regression tests. Failures are ignored: it is a debugging aid.
fn record(hub: &EarHub, snapshot: &str, label: &str, tempo: f64, bars: &[BarReport], beats: &[BeatReport]) {
    let Some(dir) = hub.recording_dir() else { return };
    let spans: Vec<Value> = bars
        .iter()
        .map(|r| json!({"bar": r.bar, "start_beat": r.start_beat, "beats": r.beats, "coverage": r.coverage}))
        .collect();
    let body = json!({"label": label, "tempo": tempo, "bars": spans, "beats": beats});
    let _ = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join(format!("{snapshot}.json")), body.to_string()));
}

/// Interprets the beats heard in the given bars.
pub(crate) fn hear_music(reports: &[&BarReport], beats: &[BeatReport], scale: Option<&SetScale>) -> Option<Music> {
    if reports.is_empty() || beats.is_empty() {
        return None;
    }
    let (beats, tuning) = harmony::audio::beats(beats);
    let bars = spans(reports);
    let analysis = harmony::analyze(&Section { bars: &bars, beats: &beats, source: Source::Audio, scale });
    let mut summary = analysis.summary;
    if let Some(cents) = tuning.filter(|c| c.abs() >= TUNING_NOTE_CENTS && summary.contains_key("key")) {
        summary.insert("tuning_cents".into(), json!(cents.round()));
    }
    Some(Music { summary, bars: analysis.bars.into_iter().collect(), beats })
}

/// Bars as `listen` and `analyze_file` report them: mix measurements, music, or both.
pub(crate) fn bar_entries(reports: &[&BarReport], focus: Focus, music: Option<&Music>) -> Vec<Value> {
    reports
        .iter()
        .map(|r| {
            let mut entry = if focus == Focus::Music {
                let mut e = json!({"bar": r.bar});
                if r.coverage < 0.99 {
                    e["coverage"] = json!(round2(r.coverage));
                }
                e
            } else {
                compact_bar(r)
            };
            if let (Some(fields), Value::Object(map)) = (music.and_then(|m| m.bars.get(&r.bar)), &mut entry) {
                map.extend(fields.clone());
            }
            entry
        })
        .collect()
}

/// The connected Ear on `track`, or an error that says what is wrong.
fn ear_on(bindings: &[Binding], track: &Track) -> Result<Instance> {
    let Some(binding) = bindings.iter().find(|b| b.track.target == track.target) else {
        bail!(
            "there is no {EAR_NAME} on track {}; add one with load_device (path {EAR_BROWSER_PATH:?}). Ears in the \
             set: {:?}",
            track.label(),
            bindings.iter().map(|b| b.track.label()).collect::<Vec<_>>()
        );
    };
    binding.instance.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "the {EAR_NAME} on track {} is not connected to this server. If Live loaded it before this server \
             started, it reconnects within a few seconds; otherwise rebuild/reinstall the plugin",
            track.label()
        )
    })
}

/// The best report per wanted bar that an instance sent since `mark`.
fn collect(hub: &EarHub, instance: u64, mark: Instant, wanted: &std::ops::Range<i64>) -> BTreeMap<i64, BarReport> {
    let mut heard: BTreeMap<i64, BarReport> = BTreeMap::new();
    for report in hub.bars_since(instance, mark) {
        if wanted.contains(&report.bar) && heard.get(&report.bar).is_none_or(|old| report.coverage >= old.coverage) {
            heard.insert(report.bar, report);
        }
    }
    heard
}

pub async fn listen(live: &Live, hub: &EarHub, store: &SnapshotStore, params: ListenParams) -> Result<Value> {
    if let Err(e) = hub.listening() {
        bail!("not listening for {EAR_NAME} plugins: {e}");
    }
    let bindings = bind(live, hub).await?;
    let multi = params.tracks.is_some();
    let mut targets: Vec<(Track, Instance)> = Vec::new();
    match &params.tracks {
        None => {
            let track = params.track.clone().unwrap_or_else(|| TrackRef::Name("master".into())).resolve(live).await?;
            let instance = ear_on(&bindings, &track)?;
            targets.push((track, instance));
        }
        Some(list) if list.iter().any(|t| matches!(t, TrackRef::Name(n) if n.trim().eq_ignore_ascii_case("all"))) => {
            targets.extend(bindings.iter().filter_map(|b| Some((b.track.clone(), b.instance.clone()?))));
            if targets.is_empty() {
                bail!("no connected {EAR_NAME} in the set; add them with load_device (path {EAR_BROWSER_PATH:?})");
            }
        }
        Some(list) => {
            if list.is_empty() {
                bail!("tracks is empty; give tracks, or [\"all\"]");
            }
            for track in list {
                let track = track.resolve(live).await?;
                let instance = ear_on(&bindings, &track)?;
                targets.push((track, instance));
            }
        }
    }

    let song = Target::path("live_set");
    let state = live
        .batch(
            vec![get_props(&song, &["tempo", "signature_numerator", "signature_denominator", "current_song_time"])],
            false,
        )
        .await?
        .remove(0);
    let tempo = state["tempo"].as_f64().unwrap_or(120.0);
    let beats_per_bar = state["signature_numerator"].as_f64().unwrap_or(4.0) * 4.0
        / state["signature_denominator"].as_f64().unwrap_or(4.0);
    let position = state["current_song_time"].as_f64().unwrap_or(0.0);
    let start_bar = params.start_bar.unwrap_or_else(|| (position / beats_per_bar).floor() as i64 + 1).max(1);
    let bars = params.bars.unwrap_or(4).max(1) as i64;
    let seconds = bars as f64 * beats_per_bar * 60.0 / tempo;
    if seconds > MAX_LISTEN.as_secs_f64() {
        bail!(
            "{bars} bars at {tempo} BPM take {seconds:.0} s; listen to at most {:.0} s at a time",
            MAX_LISTEN.as_secs_f64()
        );
    }

    let mark = Instant::now();
    if params.play {
        crate::transport::play_from(live, (start_bar - 1) as f64 * beats_per_bar).await?;
    }
    let wanted = start_bar..start_bar + bars;
    // Beats wholly inside the wanted bars; music needs them, and they arrive ~0.25 s after each
    // beat ends.
    let wanted_beats = ((start_bar - 1) as f64 * beats_per_bar).ceil() as i64
        ..((start_bar - 1 + bars) as f64 * beats_per_bar + 1e-9).floor() as i64;
    let focus = params.focus;
    let music_from = |i: &Instance| focus != Focus::Mix && i.hears_music();
    let deadline = Instant::now() + Duration::from_secs_f64(seconds + if params.play { 1.5 } else { seconds.max(2.0) });
    type Heard = (Vec<BTreeMap<i64, BarReport>>, Vec<BTreeMap<i64, BeatReport>>);
    let (heard, heard_beats): Heard = loop {
        let heard: Vec<_> = targets.iter().map(|(_, i)| collect(hub, i.id, mark, &wanted)).collect();
        let beats: Vec<_> = targets
            .iter()
            .map(|(_, i)| if music_from(i) { collect_beats(hub, i.id, mark, &wanted_beats) } else { BTreeMap::new() })
            .collect();
        let bars_done = heard.iter().all(|h| h.len() as i64 == bars && h.values().all(|r| r.coverage >= 0.95));
        let beats_done = targets.iter().zip(&beats).all(|((_, i), b)| {
            !music_from(i)
                || (b.len() as i64 == wanted_beats.end - wanted_beats.start && b.values().all(|r| r.coverage >= 0.95))
        });
        if (bars_done && beats_done) || Instant::now() >= deadline {
            break (heard, beats);
        }
        hub.changed_or(Duration::from_millis(100)).await;
    };
    if params.play {
        live.batch(vec![call(&song, "stop_playing", vec![])], false).await?;
    }
    let range = format!("{}–{}", start_bar, start_bar + bars - 1);
    let missing_hint = if params.play {
        "some bars were not heard; the plugin may be after a muted device, or playback did not reach them"
    } else {
        "some bars were not heard: was the song playing through them?"
    };

    let scale = if focus == Focus::Mix { None } else { crate::clips::set_scale(live).await };
    let music: Vec<Option<Music>> = heard
        .iter()
        .zip(&heard_beats)
        .map(|(bars, beats)| {
            let beats: Vec<BeatReport> = beats.values().copied().collect();
            hear_music(&bars.values().collect::<Vec<_>>(), &beats, scale.as_ref())
        })
        .collect();

    if !multi {
        let (track, instance) = &targets[0];
        let heard = &heard[0];
        let missing: Vec<i64> = wanted.clone().filter(|b| !heard.contains_key(b)).collect();
        let reports: Vec<&BarReport> = heard.values().collect();
        let mut out = json!({
            "track": track.label(),
            "tempo": tempo,
            "bars": bar_entries(&reports, focus, music[0].as_ref()),
        });
        if let Some(m) = &music[0] {
            out["music"] = Value::Object(m.summary.clone());
        } else if focus != Focus::Mix && !instance.hears_music() {
            out["music_note"] = json!(OLD_EAR_NOTE);
        }
        if !reports.is_empty() {
            if focus != Focus::Music {
                out["summary"] = summary(&reports);
            }
            let label = format!("listen on {} bars {range} at {tempo} BPM", track.label());
            let music = music[0].as_ref().map(Music::snapshot);
            let id = store.add(Kind::Listen, label.clone(), "bar", heard.values().copied().collect(), music);
            let beats: Vec<BeatReport> = heard_beats[0].values().copied().collect();
            record(hub, &id, &label, tempo, &heard.values().copied().collect::<Vec<_>>(), &beats);
            out["snapshot"] = json!(id);
        }
        if !missing.is_empty() {
            out["missing_bars"] = json!(missing);
            out["hint"] = json!(missing_hint);
        }
        return Ok(out);
    }

    // Several tracks. How each reaches the mix: mute and solo, where its audio goes, and the
    // faders after the Ear (its own and those of the tracks its audio passes through).
    const SWITCHES: &[&str] = &["name", "mute", "solo", "muted_via_solo", "group_track", "is_foldable"];
    let layout = live
        .batch_lenient(vec![
            get_fields(&Target::path("live_set tracks"), SWITCHES),
            get_fields(&Target::path("live_set return_tracks"), SWITCHES),
            get_fields(&Target::path("live_set master_track"), &["name"]),
        ])
        .await?;
    let listed = |k: usize| layout.get(k).and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
    let (tracks, returns) = (listed(0), listed(1));
    let regular = items(&tracks).len();
    let strip_target = |k: usize| {
        if k < regular { Track::regular(k).target } else { Track::return_track(k - regular).target }
    };
    let ops = (0..regular + items(&returns).len())
        .map(|k| {
            get_props(&strip_target(k).join("output_routing_type"), &["display_name", "category", "attached_object"])
        })
        .collect();
    let routes: Vec<Value> = live.batch_lenient(ops).await?.into_iter().map(|r| r.unwrap_or_default()).collect();
    let mixer = masking::Mixer::from_live(&tracks, &returns, &routes, &listed(2));
    let master_name = field_str(&listed(2), "name").to_owned();
    let strip_of = |track: &Track| match track.kind {
        TrackKind::Regular(i) => Some(i),
        TrackKind::Return(r) => Some(regular + r),
        TrackKind::Master => None,
    };
    // Faders by strip index; MASTER stands for the master track's.
    const MASTER: usize = usize::MAX;
    let strip_target = |k: usize| if k == MASTER { Track::master().target } else { strip_target(k) };
    let strips: Vec<Option<usize>> = targets.iter().map(|(t, _)| strip_of(t)).collect();
    // Every fader between a listened-to Ear and the mix.
    let mut faders: Vec<usize> = Vec::new();
    for k in &strips {
        let chain = match k {
            Some(k) => mixer.route(*k).0,
            None => vec![MASTER],
        };
        for f in chain {
            if !faders.contains(&f) {
                faders.push(f);
            }
        }
    }
    let ops = faders.iter().map(|&f| get_fields(&strip_target(f).join("mixer_device volume"), &["value"])).collect();
    let values = live.batch_lenient(ops).await?;
    let ops = faders
        .iter()
        .zip(&values)
        .map(|(&f, value)| {
            let value = value.as_ref().ok().and_then(|v| field_f64(v, "value")).unwrap_or(0.85);
            call(&strip_target(f).join("mixer_device volume"), "str_for_value", vec![json!(value)])
        })
        .collect();
    let displays: HashMap<usize, String> = faders
        .iter()
        .zip(live.batch_lenient(ops).await?)
        .map(|(&f, d)| (f, d.ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "0 dB".into())))
        .collect();

    let heard_bars: Vec<Vec<BarReport>> = heard.iter().map(|h| h.values().copied().collect()).collect();
    // Gain from a track's Ear to the mix: its fader and those of the tracks it plays through.
    let gain_of = |s: usize| -> Option<f64> {
        mixer.route(s).0.iter().map(|f| displays.get(f).map_or(Some(0.0), |d| parse_db(d))).sum()
    };
    // The tracks whose Ears heard something that reaches the mix.
    let voices: Vec<usize> = strips
        .iter()
        .enumerate()
        .filter_map(|(k, s)| s.map(|s| (k, s)))
        .filter(|&(k, s)| {
            mixer.silenced(s).is_none() && gain_of(s).is_some() && masking::heard_anything(&heard_bars[k])
        })
        .map(|(_, s)| s)
        .collect();

    let mut entries = Vec::new();
    let mut hearings = Vec::new();
    let mut missing_any = Vec::new();
    for (k, (track, _)) in targets.iter().enumerate() {
        let strip = strips[k].and_then(|s| mixer.strips.get(s).map(|strip| (s, strip)));
        let name = strip.map_or(master_name.clone(), |(_, strip)| strip.name.clone());
        let fader = strips[k].unwrap_or(MASTER);
        let reports: Vec<&BarReport> = heard[k].values().collect();
        let mut entry = json!({
            "track": track.label(),
            "name": name,
            "volume": displays.get(&fader).map_or("0 dB", String::as_str),
        });
        if let Some((_, strip)) = strip {
            for (flag, on) in [("muted", strip.mute), ("solo", strip.solo)] {
                if on {
                    entry[flag] = json!(true);
                }
            }
            if let Some(g) = strip.group {
                entry["group"] = json!(mixer.strips[g].name);
            }
            // Where the audio goes, unless it is the usual place (its group, or Main).
            match &strip.out {
                masking::Out::Strip(j) if Some(*j) != strip.group => entry["output"] = json!(mixer.strips[*j].name),
                masking::Out::Main if strip.group.is_some() => entry["output"] = json!(master_name),
                masking::Out::Elsewhere(name) => entry["output"] = json!(name),
                _ => {}
            }
        }
        if !reports.is_empty() {
            if focus != Focus::Music {
                entry["summary"] = summary(&reports);
            }
            if let Some(m) = &music[k] {
                entry["music"] = Value::Object(m.summary.clone());
            } else if focus != Focus::Mix && !targets[k].1.hears_music() {
                entry["music_note"] = json!(OLD_EAR_NOTE);
            }
            let label = format!("listen on {} ({name}) bars {range} at {tempo} BPM", track.label());
            let snapshot_music = music[k].as_ref().map(Music::snapshot);
            entry["snapshot"] =
                json!(store.add(Kind::Listen, label, "bar", heard[k].values().copied().collect(), snapshot_music));
            entry["bars"] = json!(bar_entries(&reports, focus, music[k].as_ref()));
        }
        let missing: Vec<i64> = wanted.clone().filter(|b| !heard[k].contains_key(b)).collect();
        if !missing.is_empty() {
            missing_any.push(json!({"track": track.label(), "bars": missing}));
        }
        entries.push(entry);
        // The master Ear hears the sum of everything; it is not one of the voices.
        let Some((s, _)) = strip else { continue };
        // Muted, silenced by solo, or not reaching Main; then −∞ and nothing heard (which
        // `analyze` reports); then a signal another Ear already counts.
        let left_out = mixer.silenced(s).or_else(|| voices.contains(&s).then(|| mixer.overlap(s, &voices)).flatten());
        hearings.push((track.label(), name, gain_of(s), left_out, k));
    }
    let voices: Vec<masking::Heard<'_>> = hearings
        .into_iter()
        .map(|(label, name, gain_db, left_out, k)| masking::Heard {
            label,
            name,
            gain_db,
            left_out,
            bars: &heard_bars[k],
        })
        .collect();

    let mut out = json!({"tempo": tempo, "bars": range, "tracks": entries});
    // The counted tracks together: the chords they make, where they rub, which octaves they crowd.
    let parts: Vec<ensemble::Voice> = voices
        .iter()
        .filter(|v| v.left_out.is_none())
        .filter_map(|v| {
            let k = targets.iter().position(|(t, _)| t.label() == v.label)?;
            let beats = &music[k].as_ref()?.beats;
            Some(ensemble::Voice { name: v.name.clone(), gain: 10f32.powf(v.gain_db? as f32 / 20.0), beats })
        })
        .collect();
    if parts.len() >= 2 {
        let bars: Vec<BarSpan> = wanted
            .clone()
            .map(|bar| BarSpan { bar, start_beat: (bar - 1) as f64 * beats_per_bar, beats: beats_per_bar })
            .collect();
        let combined = ensemble::combined(&parts);
        let together =
            harmony::analyze(&Section { bars: &bars, beats: &combined, source: Source::Audio, scale: scale.as_ref() });
        let mut music = together.summary;
        music.insert("tracks".into(), json!(parts.iter().map(|p| p.name.clone()).collect::<Vec<_>>()));
        music.insert("clashes".into(), ensemble::clashes(&parts, &together.chords, &bars, together.spelling));
        music.insert("crowded".into(), ensemble::crowded(&parts, &bars));
        out["music"] = Value::Object(music);
    }
    // Masking is about the mix, so focus "music" leaves it out.
    if focus != Focus::Music {
        if voices.len() >= 2 {
            out["masking"] = masking::analyze(&voices);
        } else {
            out["masking_note"] =
                json!("put Ears on two or more tracks (not just master) to see which tracks share each band");
        }
    }
    if !missing_any.is_empty() {
        out["missing"] = json!(missing_any);
        out["hint"] = json!(missing_hint);
    }
    Ok(out)
}

/// "-6.0 dB" → Some(-6.0); "-inf dB" → None (silent).
fn parse_db(display: &str) -> Option<f64> {
    let number = display.trim().trim_end_matches("dB").trim();
    if number.eq_ignore_ascii_case("-inf") {
        return None;
    }
    Some(number.parse::<f64>().unwrap_or(0.0))
}

pub(crate) fn round1(x: f32) -> f64 {
    (x as f64 * 10.0).round() / 10.0 + 0.0 // + 0.0 turns −0.0 into 0.0
}

pub(crate) fn round2(x: f32) -> f64 {
    (x as f64 * 100.0).round() / 100.0 + 0.0
}

/// A bar as tools report it: dB values to 0.1, correlation and width to 0.01.
pub(crate) fn compact_bar(r: &BarReport) -> Value {
    let s = &r.stats;
    let mut bands = Map::new();
    for (name, db) in BAND_NAMES.iter().zip(s.bands_dbfs) {
        bands.insert((*name).into(), json!(round1(db)));
    }
    let mut out = json!({
        "bar": r.bar,
        "lufs": round1(s.loudness_lufs),
        "peak": round1(s.peak_dbfs[0].max(s.peak_dbfs[1])),
        "rms": round1(s.rms_dbfs[0].max(s.rms_dbfs[1])),
        "crest": round1(s.crest_db),
        "bands": bands,
        "correlation": round2(s.correlation),
        "width": round2(s.width),
    });
    if s.clipped_samples > 0 {
        out["clipped"] = json!(s.clipped_samples);
    }
    if r.coverage < 0.99 {
        out["coverage"] = json!(round2(r.coverage));
    }
    out
}

/// Section-wide figures: energy averages for loudness and bands, maximum peak.
pub(crate) fn summary(reports: &[&BarReport]) -> Value {
    let energy_mean = |values: &mut dyn Iterator<Item = f32>| -> f64 {
        let (sum, n) = values.fold((0.0f64, 0usize), |(s, n), db| (s + 10f64.powf(db as f64 / 10.0), n + 1));
        if n == 0 || sum <= 0.0 { -120.0 } else { (10.0 * (sum / n as f64).log10() * 10.0).round() / 10.0 }
    };
    let lufs = energy_mean(&mut reports.iter().map(|r| r.stats.loudness_lufs));
    let peak = reports.iter().map(|r| r.stats.peak_dbfs[0].max(r.stats.peak_dbfs[1])).fold(f32::MIN, f32::max);
    let mut bands = Map::new();
    for (i, name) in BAND_NAMES.iter().enumerate() {
        bands.insert((*name).into(), json!(energy_mean(&mut reports.iter().map(|r| r.stats.bands_dbfs[i]))));
    }
    let by_loudness = |a: &&&BarReport, b: &&&BarReport| a.stats.loudness_lufs.total_cmp(&b.stats.loudness_lufs);
    let loudest = reports.iter().max_by(by_loudness).map(|r| r.bar);
    let quietest = reports.iter().min_by(by_loudness).map(|r| r.bar);
    json!({
        "lufs": lufs,
        "peak": round1(peak),
        "bands": bands,
        "loudest_bar": loudest,
        "quietest_bar": quietest,
        "min_correlation": reports.iter().map(|r| round2(r.stats.correlation)).fold(f64::MAX, f64::min),
        "clipped": reports.iter().map(|r| r.stats.clipped_samples as u64).sum::<u64>(),
    })
}

/// Compares tokens in time that doesn't depend on where they differ (the length is public).
fn tokens_match(given: &str, expected: &str) -> bool {
    let (given, expected) = (given.as_bytes(), expected.as_bytes());
    given.len() == expected.len() && given.iter().zip(expected).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_beats_takes_true_or_a_folder() {
        let state = Path::new("/state");
        let dir = |config: Value| recording_dir(&config, state);
        assert_eq!(dir(json!({"record_beats": true})), Some(PathBuf::from("/state/recordings")));
        assert_eq!(dir(json!({"record_beats": "mine"})), Some(PathBuf::from("/state/mine")));
        assert_eq!(dir(json!({"record_beats": "/tmp/x"})), Some(PathBuf::from("/tmp/x")));
        for off in [json!({}), json!({"record_beats": false}), json!({"record_beats": ""}), json!({"record_beats": 3})]
        {
            assert_eq!(dir(off), None);
        }
    }

    #[test]
    fn tokens_match_only_when_equal() {
        assert!(tokens_match("abc123", "abc123"));
        assert!(!tokens_match("abc124", "abc123"));
        assert!(!tokens_match("abc12", "abc123"));
        assert!(!tokens_match("", "abc123"));
    }

    #[test]
    fn parses_live_volume_displays() {
        assert_eq!(parse_db("0.0 dB"), Some(0.0));
        assert_eq!(parse_db("-6.0 dB"), Some(-6.0));
        assert_eq!(parse_db("6.0 dB"), Some(6.0));
        assert_eq!(parse_db("-inf dB"), None);
    }
}
