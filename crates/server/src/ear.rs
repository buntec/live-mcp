//! Listening through LiveMCP Ear plugins: the hub that plugins connect to, matching plugin
//! instances to tracks, and the `listen` and `ears` tools.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use live_mcp_analysis::{BAND_NAMES, BarReport};
use live_mcp_bridge::paths;
use live_mcp_protocol::Target;
use live_mcp_protocol::ear::{EAR_DEFAULT_PORT, EAR_PROTOCOL_VERSION, EarEndpoint, EarMessage, TAG_MAX};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::live::{
    Live, Track, TrackRef, call, default_true, field_bool, field_f64, field_str, get_fields, get_props, items, set,
};
use crate::masking;
use crate::snapshots::{Kind, SnapshotStore};

/// The plugin's name in Live's browser and device chain.
pub const EAR_NAME: &str = "LiveMCP Ear";
pub const EAR_BROWSER_PATH: [&str; 3] = ["plugins", "live-mcp", EAR_NAME];
/// Bars kept per instance.
const HISTORY: usize = 1024;
/// How long to wait for plugins to report a newly written tag.
const TAG_WAIT: Duration = Duration::from_millis(1500);
/// Longest `listen` call, so MCP clients do not time out.
const MAX_LISTEN: Duration = Duration::from_secs(60);
const MAX_LINE: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Instance {
    pub id: u64,
    pub tag: u32,
    pub playing: bool,
    pub connected: bool,
    pub tag_seen_at: Instant,
    pub sample_rate: f32,
    pub version: String,
    bars: VecDeque<(Instant, BarReport)>,
}

/// Accepts connections from plugin instances and keeps what they report.
#[derive(Default)]
pub struct EarHub {
    instances: Mutex<HashMap<u64, Instance>>,
    changed: Notify,
    listening: OnceLock<Result<u16, String>>,
}

impl EarHub {
    /// Listens on 127.0.0.1 (port from `config.json` `ear_port`, default 19419) and advertises
    /// the port in `ear.json`. Failure is recorded, not fatal: the other tools still work.
    pub async fn start(self: &Arc<Self>, state_dir: &Path) {
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

    pub fn listening(&self) -> Result<u16, String> {
        self.listening.get().cloned().unwrap_or_else(|| Err("not started".into()))
    }

    async fn serve_plugin(&self, socket: TcpStream, token: &str) {
        let _ = socket.set_nodelay(true);
        let mut lines = BufReader::new(socket).lines();
        let Ok(Some(first)) = lines.next_line().await else { return };
        let id = match serde_json::from_str::<EarMessage>(&first) {
            Ok(EarMessage::Hello { token: given, protocol, instance, plugin_version, sample_rate })
                if given == token && protocol == EAR_PROTOCOL_VERSION =>
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
                    bars: VecDeque::new(),
                });
                entry.connected = true;
                entry.sample_rate = sample_rate;
                entry.version = plugin_version;
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

    async fn changed_or(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.changed.notified()).await;
    }
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
        out["hint"] =
            json!(format!("no {EAR_NAME} in the set; add one with load_device and path {EAR_BROWSER_PATH:?}"));
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
    let deadline = Instant::now() + Duration::from_secs_f64(seconds + if params.play { 1.5 } else { seconds.max(2.0) });
    let heard: Vec<BTreeMap<i64, BarReport>> = loop {
        let heard: Vec<_> = targets.iter().map(|(_, i)| collect(hub, i.id, mark, &wanted)).collect();
        let complete = heard.iter().all(|h| h.len() as i64 == bars && h.values().all(|r| r.coverage >= 0.95));
        if complete || Instant::now() >= deadline {
            break heard;
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

    if !multi {
        let (track, _) = &targets[0];
        let heard = &heard[0];
        let missing: Vec<i64> = wanted.clone().filter(|b| !heard.contains_key(b)).collect();
        let reports: Vec<&BarReport> = heard.values().collect();
        let mut out = json!({
            "track": track.label(),
            "tempo": tempo,
            "bars": reports.iter().map(|r| compact_bar(r)).collect::<Vec<_>>(),
        });
        if !reports.is_empty() {
            out["summary"] = summary(&reports);
            let label = format!("listen on {} bars {range} at {tempo} BPM", track.label());
            out["snapshot"] = json!(store.add(Kind::Listen, label, "bar", heard.values().copied().collect()));
        }
        if !missing.is_empty() {
            out["missing_bars"] = json!(missing);
            out["hint"] = json!(missing_hint);
        }
        return Ok(out);
    }

    // Several tracks: names, mute state and fader gain (the Ear hears the signal before the fader).
    let mut ops = Vec::new();
    for (track, _) in &targets {
        ops.push(get_fields(&track.target, &["name", "mute"]));
        ops.push(get_fields(&track.target.join("mixer_device volume"), &["value"]));
    }
    let info = live.batch_lenient(ops).await?;
    let volumes: Vec<Option<f64>> =
        info.iter().skip(1).step_by(2).map(|v| v.as_ref().ok().and_then(|v| field_f64(v, "value"))).collect();
    let ops = targets
        .iter()
        .zip(&volumes)
        .map(|((track, _), value)| {
            call(&track.target.join("mixer_device volume"), "str_for_value", vec![json!(value.unwrap_or(0.85))])
        })
        .collect();
    let displays = live.batch_lenient(ops).await?;

    let mut entries = Vec::new();
    let mut hearings = Vec::new();
    let mut missing_any = Vec::new();
    for (k, (track, _)) in targets.iter().enumerate() {
        let props = info[2 * k].as_ref().ok().cloned().unwrap_or_default();
        let name = field_str(&props, "name").to_owned();
        let muted = field_bool(&props, "mute") == Some(true);
        let display = displays[k].as_ref().ok().and_then(|v| v.as_str()).unwrap_or("0 dB").to_owned();
        let gain_db = parse_db(&display);
        let reports: Vec<&BarReport> = heard[k].values().collect();
        let mut entry = json!({"track": track.label(), "name": name, "volume": display});
        if muted {
            entry["muted"] = json!(true);
        }
        if !reports.is_empty() {
            entry["summary"] = summary(&reports);
            let label = format!("listen on {} ({name}) bars {range} at {tempo} BPM", track.label());
            entry["snapshot"] = json!(store.add(Kind::Listen, label, "bar", heard[k].values().copied().collect()));
            entry["bars"] = json!(reports.iter().map(|r| compact_bar(r)).collect::<Vec<_>>());
        }
        let missing: Vec<i64> = wanted.clone().filter(|b| !heard[k].contains_key(b)).collect();
        if !missing.is_empty() {
            missing_any.push(json!({"track": track.label(), "bars": missing}));
        }
        entries.push(entry);
        // The master Ear hears the sum of everything; it is not one of the voices.
        if track.kind != crate::live::TrackKind::Master {
            hearings.push((track.label(), name, gain_db, muted, k));
        }
    }
    let heard_bars: Vec<Vec<BarReport>> = heard.iter().map(|h| h.values().copied().collect()).collect();
    let voices: Vec<masking::Heard<'_>> = hearings
        .into_iter()
        .map(|(label, name, gain_db, muted, k)| masking::Heard { label, name, gain_db, muted, bars: &heard_bars[k] })
        .collect();

    let mut out = json!({"tempo": tempo, "bars": range, "tracks": entries});
    if voices.len() >= 2 {
        out["masking"] = masking::analyze(&voices);
    } else {
        out["masking_note"] =
            json!("put Ears on two or more tracks (not just master) to see which tracks share each band");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_volume_displays() {
        assert_eq!(parse_db("0.0 dB"), Some(0.0));
        assert_eq!(parse_db("-6.0 dB"), Some(-6.0));
        assert_eq!(parse_db("6.0 dB"), Some(6.0));
        assert_eq!(parse_db("-inf dB"), None);
    }
}
