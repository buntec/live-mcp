//! Snapshots of what was heard (`listen`) or analysed (`analyze_file`), and `compare`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use anyhow::{Result, bail};
use live_mcp_analysis::{BAND_EDGES_HZ, BAND_NAMES, BarReport, BeatReport};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::harmony::BarSpan;

/// Snapshots kept per server session; the oldest are dropped first.
const KEEP: usize = 100;
/// Bars heard for less than this fraction are left out of comparisons.
const MIN_COVERAGE: f32 = 0.5;
/// Bar-by-bar differences are listed for selections up to this long.
const MAX_PAIRED_BARS: usize = 64;
/// Rhythm density (onsets per beat) and syncopation differences worth a note.
const DENSITY_NOTE: f64 = 0.5;
const SYNCOPATION_NOTE: f64 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Listen,
    File,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    /// "bar" or "section".
    pub unit: &'static str,
    pub bars: Vec<BarReport>,
    pub music: Option<SnapshotMusic>,
}

/// The musical reading that came with a snapshot: section summary and per-bar fields, and the beat
/// reports it was heard from (to hear a selection of the bars again).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnapshotMusic {
    pub summary: Map<String, Value>,
    pub bars: BTreeMap<i64, Map<String, Value>>,
    pub beats: Vec<BeatReport>,
}

#[derive(Default)]
pub struct SnapshotStore {
    inner: Mutex<(u64, VecDeque<Snapshot>)>,
}

impl SnapshotStore {
    /// Stores the bars (and their musical reading, if any) and returns the new snapshot's id.
    pub fn add(
        &self,
        kind: Kind,
        label: String,
        unit: &'static str,
        bars: Vec<BarReport>,
        music: Option<SnapshotMusic>,
    ) -> String {
        let mut inner = self.inner.lock().expect("snapshot lock");
        inner.0 += 1;
        let id = format!("s{}", inner.0);
        if inner.1.len() == KEEP {
            inner.1.pop_front();
        }
        inner.1.push_back(Snapshot { id: id.clone(), kind, label, unit, bars, music });
        id
    }

    pub fn get(&self, id: &str) -> Result<Snapshot> {
        let inner = self.inner.lock().expect("snapshot lock");
        let wanted = id.trim();
        match inner.1.iter().find(|s| s.id == wanted) {
            Some(s) => Ok(s.clone()),
            None => {
                let known: Vec<&str> = inner.1.iter().map(|s| s.id.as_str()).collect();
                bail!(
                    "no snapshot {wanted:?}; snapshots this session: {known:?} (they are kept until the server stops)"
                )
            }
        }
    }

    pub fn list(&self) -> Vec<Snapshot> {
        self.inner.lock().expect("snapshot lock").1.iter().cloned().collect()
    }
}

/// A selection of bars, summarised the same way whatever its source.
#[derive(Debug, Clone, PartialEq)]
struct Profile {
    bars: usize,
    first: i64,
    last: i64,
    lufs: f64,
    peak: f64,
    rms: f64,
    crest: f64,
    /// Band levels (energy mean over the bars), dBFS.
    bands: [f64; 5],
    correlation: f64,
    width: f64,
    clipped: u64,
    /// Loudest minus quietest bar, in LU.
    spread: f64,
}

fn energy_mean(levels: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = levels.fold((0.0, 0usize), |(s, n), db| (s + 10f64.powf(db / 10.0), n + 1));
    if n == 0 || sum <= 0.0 { -120.0 } else { 10.0 * (sum / n as f64).log10() }
}

impl Profile {
    fn of(bars: &[&BarReport]) -> Option<Self> {
        let n = bars.len();
        if n == 0 {
            return None;
        }
        let louder = |r: &BarReport| r.stats.rms_dbfs[0].max(r.stats.rms_dbfs[1]) as f64;
        let lufs: Vec<f64> = bars.iter().map(|r| r.stats.loudness_lufs as f64).collect();
        Some(Self {
            bars: n,
            first: bars.iter().map(|r| r.bar).min().unwrap_or(0),
            last: bars.iter().map(|r| r.bar).max().unwrap_or(0),
            lufs: energy_mean(lufs.iter().copied()),
            peak: bars.iter().map(|r| r.stats.peak_dbfs[0].max(r.stats.peak_dbfs[1]) as f64).fold(f64::MIN, f64::max),
            rms: energy_mean(bars.iter().map(|r| louder(r))),
            crest: bars.iter().map(|r| r.stats.crest_db as f64).sum::<f64>() / n as f64,
            bands: std::array::from_fn(|i| energy_mean(bars.iter().map(|r| r.stats.bands_dbfs[i] as f64))),
            correlation: bars.iter().map(|r| r.stats.correlation as f64).sum::<f64>() / n as f64,
            width: bars.iter().map(|r| r.stats.width as f64).sum::<f64>() / n as f64,
            clipped: bars.iter().map(|r| r.stats.clipped_samples as u64).sum(),
            spread: lufs.iter().copied().fold(f64::MIN, f64::max) - lufs.iter().copied().fold(f64::MAX, f64::min),
        })
    }
}

// Adding 0.0 turns −0.0 into 0.0, so results never print as "-0.0".
fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0 + 0.0
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0 + 0.0
}

fn band_label(i: usize) -> String {
    match i {
        0 => format!("low (<{} Hz)", BAND_EDGES_HZ[0]),
        4 => format!("high (>{} kHz)", BAND_EDGES_HZ[3] / 1000.0),
        _ => {
            let fmt = |hz: f64| if hz >= 1000.0 { format!("{}k", hz / 1000.0) } else { format!("{hz}") };
            format!("{} ({}–{} Hz)", BAND_NAMES[i], fmt(BAND_EDGES_HZ[i - 1]), fmt(BAND_EDGES_HZ[i]))
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompareParams {
    /// A snapshot id from listen or analyze_file, e.g. "s2". Differences are reported as a − b.
    pub a: String,
    /// Only these bars (or sections) of a, inclusive: [first, last].
    pub a_bars: Option<[i64; 2]>,
    pub b: String,
    pub b_bars: Option<[i64; 2]>,
}

fn select(snapshot: &Snapshot, range: Option<[i64; 2]>) -> Result<Vec<&BarReport>> {
    if let Some([first, last]) = range
        && first > last
    {
        bail!("{}: bar range [{first}, {last}] is empty", snapshot.id);
    }
    let selected: Vec<&BarReport> = snapshot
        .bars
        .iter()
        .filter(|r| r.coverage >= MIN_COVERAGE)
        .filter(|r| range.is_none_or(|[first, last]| (first..=last).contains(&r.bar)))
        .collect();
    if selected.is_empty() {
        let have: Vec<i64> = snapshot.bars.iter().map(|r| r.bar).collect();
        bail!("{} has no complete {}s in that range; it has {:?}", snapshot.id, snapshot.unit, have);
    }
    Ok(selected)
}

pub fn compare(store: &SnapshotStore, params: CompareParams) -> Result<Value> {
    let a = store.get(&params.a)?;
    let b = store.get(&params.b)?;
    let a_bars = select(&a, params.a_bars)?;
    let b_bars = select(&b, params.b_bars)?;
    let (pa, pb) = (Profile::of(&a_bars).expect("non-empty"), Profile::of(&b_bars).expect("non-empty"));

    let side = |s: &Snapshot, p: &Profile| json!({"id": s.id, "label": s.label, s.unit.to_owned() + "s": format!("{}–{}", p.first, p.last), "count": p.bars});
    let metric = |x: f64, y: f64, round: fn(f64) -> f64| json!({"a": round(x), "b": round(y), "delta": round(x - y)});
    // Tonal balance: per-band differences after matching levels on the median band. A change in
    // one band then shows only in that band, instead of shifting every band's share of the total.
    let raw: [f64; 5] = std::array::from_fn(|i| pa.bands[i] - pb.bands[i]);
    let mut sorted = raw;
    sorted.sort_by(f64::total_cmp);
    let level_offset = sorted[2];
    let tone: [f64; 5] = raw.map(|d| d - level_offset);
    let balance: Vec<Value> = (0..5)
        .map(|i| json!({"band": BAND_NAMES[i], "a_dbfs": r1(pa.bands[i]), "b_dbfs": r1(pb.bands[i]), "delta": r1(tone[i])}))
        .collect();

    // Plain-language notes on the differences large enough to matter.
    let mut notes = Vec::new();
    let (an, bn) = (&a.id, &b.id);
    let loud = pa.lufs - pb.lufs;
    if loud.abs() >= 1.0 {
        let word = if loud > 0.0 { "louder" } else { "quieter" };
        notes.push(format!("{an} is {:.1} LU {word} than {bn}", loud.abs()));
    }
    let mut tonal: Vec<(usize, f64)> = (0..5).map(|i| (i, tone[i])).filter(|(_, d)| d.abs() >= 2.0).collect();
    tonal.sort_by(|x, y| y.1.abs().total_cmp(&x.1.abs()));
    for (i, d) in tonal {
        let word = if d > 0.0 { "more" } else { "less" };
        notes.push(format!("{an} has {:.1} dB {word} {} than {bn}, with levels matched", d.abs(), band_label(i)));
    }
    let crest = pa.crest - pb.crest;
    if crest.abs() >= 2.0 {
        let word =
            if crest > 0.0 { "more dynamic (higher crest factor)" } else { "more compressed (lower crest factor)" };
        notes.push(format!("{an} is {word}: {:.1} vs {:.1} dB", pa.crest, pb.crest));
    }
    if (pa.width - pb.width).abs() >= 0.1 {
        let word = if pa.width > pb.width { "wider" } else { "narrower" };
        notes.push(format!("{an} is {word} in stereo: width {:.2} vs {:.2}", pa.width, pb.width));
    }
    if pa.correlation < 0.0 && pb.correlation >= 0.0 {
        notes.push(format!("{an} has negative stereo correlation ({:.2}): check for phase problems", pa.correlation));
    }
    if pa.clipped > 0 && pb.clipped == 0 {
        notes.push(format!("{an} clips ({} samples); {bn} does not", pa.clipped));
    }
    if notes.is_empty() {
        notes.push(
            "no large differences: loudness within 1 LU, tonal balance within 2 dB per band (levels matched)".into(),
        );
    }

    let mut out = json!({
        "a": side(&a, &pa),
        "b": side(&b, &pb),
        "notes": notes,
        "loudness_lufs": metric(pa.lufs, pb.lufs, r1),
        "peak_dbfs": metric(pa.peak, pb.peak, r1),
        "rms_dbfs": metric(pa.rms, pb.rms, r1),
        "crest_db": metric(pa.crest, pb.crest, r1),
        "loudness_spread_lu": metric(pa.spread, pb.spread, r1),
        "tonal_balance_db": balance,
        "tonal_balance_note": "delta: a − b per band after matching levels on the median band",
        "width": metric(pa.width, pb.width, r2),
        "correlation": metric(pa.correlation, pb.correlation, r2),
    });
    // Same-length selections line up bar by bar, as a before/after of one section does.
    if a_bars.len() == b_bars.len() && a_bars.len() <= MAX_PAIRED_BARS && a_bars.len() > 1 {
        let paired: Vec<Value> = a_bars
            .iter()
            .zip(&b_bars)
            .map(|(x, y)| {
                let bands: Vec<f64> =
                    (0..5).map(|i| r1((x.stats.bands_dbfs[i] - y.stats.bands_dbfs[i]) as f64)).collect();
                json!({
                    "a": x.bar,
                    "b": y.bar,
                    "lufs_delta": r1((x.stats.loudness_lufs - y.stats.loudness_lufs) as f64),
                    "band_deltas": bands,
                })
            })
            .collect();
        out["per_bar"] = json!(paired);
        out["per_bar_bands"] = json!(BAND_NAMES);
    }
    if let (Some(ma), Some(mb)) = (&a.music, &b.music) {
        let (sa, sb) = (selected_summary(&a, ma, &a_bars), selected_summary(&b, mb, &b_bars));
        let (music, music_notes) = compare_music(an, ma, &sa, &a_bars, bn, mb, &sb, &b_bars);
        out["music"] = music;
        if let Value::Array(list) = &mut out["notes"] {
            if !music_notes.is_empty() && list.len() == 1 && list[0].as_str().is_some_and(|n| n.starts_with("no large"))
            {
                list[0] = json!("no large mix differences: loudness within 1 LU, tonal balance within 2 dB per band");
            }
            list.extend(music_notes.into_iter().map(Value::from));
        }
    }
    Ok(out)
}

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// A bar's chords as one string: "Am" or "C G".
fn bar_chords(fields: &Map<String, Value>) -> Option<String> {
    if let Some(chord) = text(fields, "chord") {
        return Some(chord.to_owned());
    }
    let list = fields.get("chords")?.as_array()?;
    Some(list.iter().filter_map(|c| c.get("chord").and_then(Value::as_str)).collect::<Vec<_>>().join(" "))
}

/// A bar's chords with the beat each starts on, so that moving a change shows: "Am", or "C@1 G@3"
/// when the bar has several.
fn bar_chords_timed(fields: &Map<String, Value>) -> Option<String> {
    if let Some(chord) = text(fields, "chord") {
        return Some(chord.to_owned());
    }
    let list = fields.get("chords")?.as_array()?;
    let timed = list.iter().filter_map(|c| {
        let chord = c.get("chord").and_then(Value::as_str)?;
        Some(format!("{chord}@{}", c.get("beat").and_then(Value::as_f64).unwrap_or(1.0)))
    });
    Some(timed.collect::<Vec<_>>().join(" "))
}

/// The musical summary of the selected bars. A snapshot's own summary covers all of its bars, so
/// for part of them the bars are heard again from the snapshot's beat reports.
fn selected_summary(snapshot: &Snapshot, music: &SnapshotMusic, bars: &[&BarReport]) -> Map<String, Value> {
    if bars.len() == snapshot.bars.iter().filter(|r| r.coverage >= MIN_COVERAGE).count() {
        return music.summary.clone();
    }
    let spans: Vec<BarSpan> =
        bars.iter().map(|r| BarSpan { bar: r.bar, start_beat: r.start_beat, beats: r.beats }).collect();
    let beats: Vec<BeatReport> =
        music.beats.iter().filter(|b| spans.iter().any(|s| s.holds(b.beat))).copied().collect();
    let Some(heard) = crate::ear::hear_music(bars, &beats, None) else { return Map::new() };
    let mut summary = heard.summary;
    if snapshot.unit == "section" {
        // As analyze_file does: without the tempo the 16th grid means nothing.
        summary.remove("rhythm");
    }
    summary
}

/// Key, progression and rhythm of two snapshots side by side, with notes on what changed. The
/// summaries are those of the selected bars.
#[allow(clippy::too_many_arguments)]
fn compare_music(
    an: &str,
    ma: &SnapshotMusic,
    sa: &Map<String, Value>,
    a_bars: &[&BarReport],
    bn: &str,
    mb: &SnapshotMusic,
    sb: &Map<String, Value>,
    b_bars: &[&BarReport],
) -> (Value, Vec<String>) {
    let key =
        |m: &Map<String, Value>| m.get("key").and_then(|k| k.get("name")).and_then(Value::as_str).map(str::to_owned);
    let progression = |m: &SnapshotMusic, bars: &[&BarReport]| {
        bars.iter()
            .map(|r| m.bars.get(&r.bar).and_then(bar_chords).unwrap_or_else(|| "-".into()))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let rhythm =
        |m: &Map<String, Value>, metric: &str| m.get("rhythm").and_then(|r| r.get(metric)).and_then(Value::as_f64);
    let mut notes = Vec::new();
    let (ka, kb) = (key(sa), key(sb));
    if ka != kb {
        let name = |k: &Option<String>| k.clone().unwrap_or_else(|| "no clear key".into());
        notes.push(format!("{an} is in {}, {bn} in {}", name(&ka), name(&kb)));
    }
    let tuning = |m: &Map<String, Value>| m.get("tuning_cents").and_then(Value::as_f64).unwrap_or(0.0);
    if (tuning(sa) - tuning(sb)).abs() >= 10.0 {
        notes.push(format!("{an} is tuned {:+.0} cents, {bn} {:+.0}", tuning(sa), tuning(sb)));
    }
    let mut changed = Vec::new();
    if a_bars.len() == b_bars.len() {
        for (x, y) in a_bars.iter().zip(b_bars) {
            let (fa, fb) = (ma.bars.get(&x.bar), mb.bars.get(&y.bar));
            let (ca, cb) = (fa.and_then(bar_chords_timed), fb.and_then(bar_chords_timed));
            let (ra, rb) = (fa.and_then(|f| text(f, "rhythm")), fb.and_then(|f| text(f, "rhythm")));
            let mut entry = json!({"a": x.bar, "b": y.bar});
            if ca != cb {
                entry["chords"] = json!([ca, cb]);
            }
            if ra != rb {
                entry["rhythm"] = json!([ra, rb]);
            }
            if entry.as_object().is_some_and(|e| e.len() > 2) {
                changed.push(entry);
            }
        }
        let chord_changes = changed.iter().filter(|c| c.get("chords").is_some()).count();
        if chord_changes > 0 {
            notes.push(format!("the chords differ in {chord_changes} of {} paired bars", a_bars.len()));
        }
        let rhythm_changes = changed.iter().filter(|c| c.get("rhythm").is_some()).count();
        if rhythm_changes > 0 {
            notes.push(format!("the onset grid differs in {rhythm_changes} of {} paired bars", a_bars.len()));
        }
    }
    if let (Some(da), Some(db)) = (rhythm(sa, "density"), rhythm(sb, "density"))
        && (da - db).abs() >= DENSITY_NOTE
    {
        let word = if da > db { "busier" } else { "sparser" };
        notes.push(format!("{an} is rhythmically {word}: {da:.1} vs {db:.1} onsets per beat"));
    }
    if let (Some(ya), Some(yb)) = (rhythm(sa, "syncopation"), rhythm(sb, "syncopation"))
        && (ya - yb).abs() >= SYNCOPATION_NOTE
    {
        let word = if ya > yb { "more" } else { "less" };
        notes.push(format!("{an} is {word} syncopated: {ya:.2} vs {yb:.2}"));
    }
    let mut music = json!({
        "key": {"a": ka, "b": kb},
        "progression": {"a": progression(ma, a_bars), "b": progression(mb, b_bars)},
    });
    if !changed.is_empty() && changed.len() <= MAX_PAIRED_BARS {
        music["per_bar"] = json!(changed);
    }
    (music, notes)
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SnapshotsParams {}

pub fn list(store: &SnapshotStore) -> Value {
    let list: Vec<Value> = store
        .list()
        .iter()
        .map(|s| {
            let all: Vec<&BarReport> = s.bars.iter().filter(|r| r.coverage >= MIN_COVERAGE).collect();
            let mut entry = json!({
                "id": s.id,
                "kind": if s.kind == Kind::Listen { "listen" } else { "file" },
                "label": s.label,
                "unit": s.unit,
                "count": s.bars.len(),
            });
            if let Some(p) = Profile::of(&all) {
                entry["range"] = json!([p.first, p.last]);
                entry["lufs"] = json!(r1(p.lufs));
            }
            entry
        })
        .collect();
    json!({"snapshots": list})
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use live_mcp_analysis::{BarAnalyzer, Transport};

    use super::*;

    /// One bar (2 s at 120 BPM) of the sum of sines given as (Hz, amplitude).
    fn bar(number: i64, partials: &[(f64, f64)]) -> BarReport {
        let sr = 48_000.0;
        let mut analyzer = BarAnalyzer::new(sr as f32);
        let samples: Vec<f32> = (0..2 * 96_000)
            .map(|i| partials.iter().map(|(f, a)| a * (2.0 * PI * f * i as f64 / sr).sin()).sum::<f64>() as f32)
            .collect();
        let mut reports = Vec::new();
        let t = Transport {
            playing: true,
            beat: Some((number - 2) as f64 * 4.0),
            tempo: Some(120.0),
            time_signature: None,
        };
        analyzer.process(&samples, &samples, &t, &mut |r| reports.push(r));
        *reports.last().unwrap() // the second bar, after the filters have settled
    }

    fn store_with(bars_a: Vec<BarReport>, bars_b: Vec<BarReport>) -> SnapshotStore {
        let store = SnapshotStore::default();
        assert_eq!(store.add(Kind::Listen, "mine".into(), "bar", bars_a, None), "s1");
        assert_eq!(store.add(Kind::File, "reference".into(), "bar", bars_b, None), "s2");
        store
    }

    fn params(a_bars: Option<[i64; 2]>, b_bars: Option<[i64; 2]>) -> CompareParams {
        CompareParams { a: "s1".into(), a_bars, b: "s2".into(), b_bars }
    }

    #[test]
    fn tonal_balance_ignores_overall_level() {
        let mix = [(80.0, 0.3), (300.0, 0.3), (1000.0, 0.3)];
        let quieter: Vec<(f64, f64)> = mix.iter().map(|(f, a)| (*f, a * 0.5)).collect();
        let store = store_with(vec![bar(1, &quieter), bar(2, &quieter)], vec![bar(1, &mix), bar(2, &mix)]);
        let out = compare(&store, params(None, None)).unwrap();
        assert!((out["loudness_lufs"]["delta"].as_f64().unwrap() + 6.0).abs() < 0.2, "{out}");
        for band in out["tonal_balance_db"].as_array().unwrap() {
            assert!(band["delta"].as_f64().unwrap().abs() < 0.2, "same mix, only quieter: {band}");
        }
        let notes = out["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].as_str().unwrap().contains("s1 is 6.0 LU quieter than s2"), "{notes:?}");
        assert_eq!(out["per_bar"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn spots_a_muddy_low_mid() {
        let reference = [(80.0, 0.3), (300.0, 0.1), (1000.0, 0.3)];
        let muddy = [(80.0, 0.3), (300.0, 0.4), (1000.0, 0.3)];
        let store = store_with(vec![bar(5, &muddy)], vec![bar(17, &reference)]);
        let out = compare(&store, params(None, None)).unwrap();
        let notes: Vec<&str> = out["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
        assert!(notes.iter().any(|n| n.contains("more low_mid (120–500 Hz)")), "{notes:?}");
        let low_mid = &out["tonal_balance_db"][1];
        assert!(low_mid["delta"].as_f64().unwrap() > 8.0, "{low_mid}");
        assert!(out.get("per_bar").is_none(), "single bars are not paired");
        assert_eq!(out["a"]["bars"], "5–5");
    }

    #[test]
    fn bar_ranges_partial_bars_and_errors() {
        let tone = [(440.0, 0.3)];
        let mut partial = bar(3, &tone);
        partial.coverage = 0.2;
        let store = store_with(vec![bar(1, &tone), bar(2, &tone), partial], vec![bar(1, &tone), bar(2, &tone)]);
        let out = compare(&store, params(Some([2, 3]), Some([2, 2]))).unwrap();
        assert_eq!(out["a"]["count"], 1, "the partial bar 3 is left out");
        assert!(compare(&store, params(Some([7, 9]), None)).unwrap_err().to_string().contains("no complete bars"));
        assert!(compare(&store, params(Some([3, 2]), None)).is_err());
        let bad = CompareParams { a: "s9".into(), a_bars: None, b: "s1".into(), b_bars: None };
        assert!(compare(&store, bad).unwrap_err().to_string().contains("\"s1\", \"s2\""));
        let listed = list(&store);
        assert_eq!(listed["snapshots"][0]["range"], json!([1, 2]));
        assert_eq!(listed["snapshots"][1]["kind"], "file");
    }

    /// Steady beat reports of the sum of sines (Hz, amplitude) for beats `from..to`.
    fn beats(from: i64, to: i64, partials: &[(f64, f64)]) -> Vec<BeatReport> {
        let sr = 48_000.0;
        let mut analyzer = live_mcp_analysis::BeatAnalyzer::new(sr as f32);
        let samples: Vec<f32> = (0..72_000)
            .map(|i| partials.iter().map(|(f, a)| a * (2.0 * PI * f * i as f64 / sr).sin()).sum::<f64>() as f32)
            .collect();
        let mut reports = Vec::new();
        let t = Transport { playing: true, beat: Some(0.0), tempo: Some(120.0), time_signature: None };
        analyzer.process(&samples, &samples, &t, &mut |r| reports.push(r));
        let one = reports.into_iter().find(|r| r.beat == 1).unwrap();
        // Not `BeatReport { beat, ..one }` in a closure: that crashes rustc 1.88 (our MSRV).
        (from..to)
            .map(|beat| {
                let mut report = one;
                report.beat = beat;
                report
            })
            .collect()
    }

    fn fields(pairs: Value) -> Map<String, Value> {
        pairs.as_object().unwrap().clone()
    }

    #[test]
    fn music_of_part_of_a_snapshot_is_heard_again() {
        let tone = [(440.0, 0.3)];
        let c_major = [(261.63, 0.2), (329.63, 0.2), (392.0, 0.2)];
        let a_minor = [(220.0, 0.2), (261.63, 0.2), (329.63, 0.2)];
        let store = SnapshotStore::default();
        // The same C major first bar; the stored summaries (of both bars) name different keys.
        for (key, second) in [("C major", c_major), ("A minor", a_minor)] {
            let mut heard = beats(0, 4, &c_major);
            heard.extend(beats(4, 8, &second));
            let music = SnapshotMusic {
                summary: fields(json!({"key": {"name": key}})),
                bars: BTreeMap::from([(1, fields(json!({"chord": "C"}))), (2, fields(json!({"chord": "?"})))]),
                beats: heard,
            };
            store.add(Kind::Listen, key.into(), "bar", vec![bar(1, &tone), bar(2, &tone)], Some(music));
        }
        let whole = compare(&store, params(None, None)).unwrap();
        assert_eq!(whole["music"]["key"], json!({"a": "C major", "b": "A minor"}), "{whole}");
        let first = compare(&store, params(Some([1, 1]), Some([1, 1]))).unwrap();
        let key = &first["music"]["key"];
        assert!(key["a"].is_string() && key["a"] == key["b"], "the first bars are heard again, alike: {first}");
        assert!(!first["notes"].to_string().contains(" is in "), "{first}");
    }

    #[test]
    fn moving_a_chord_change_changes_the_bar() {
        let tone = [(440.0, 0.3)];
        let store = SnapshotStore::default();
        for change in [2.0, 3.0] {
            let bar_fields = fields(json!({"chords": [{"beat": 1.0, "chord": "C"}, {"beat": change, "chord": "F"}]}));
            let music = SnapshotMusic {
                bars: BTreeMap::from([(1, bar_fields.clone()), (2, bar_fields)]),
                ..SnapshotMusic::default()
            };
            store.add(Kind::Listen, String::new(), "bar", vec![bar(1, &tone), bar(2, &tone)], Some(music));
        }
        let out = compare(&store, params(None, None)).unwrap();
        assert_eq!(out["music"]["progression"]["a"], out["music"]["progression"]["b"], "{out}");
        assert_eq!(out["music"]["per_bar"][0]["chords"], json!(["C@1 F@2", "C@1 F@3"]), "{out}");
        assert!(out["notes"].to_string().contains("the chords differ in 2 of 2 paired bars"), "{out}");
    }

    #[test]
    fn rounding_never_gives_negative_zero() {
        assert!(r1(-0.04).is_sign_positive() && r2(-0.004).is_sign_positive());
        assert_eq!(r1(-0.06), -0.1);
    }

    #[test]
    fn keeps_the_most_recent_snapshots() {
        let store = SnapshotStore::default();
        for _ in 0..KEEP + 5 {
            store.add(Kind::Listen, String::new(), "bar", Vec::new(), None);
        }
        let ids: Vec<String> = store.list().into_iter().map(|s| s.id).collect();
        assert_eq!((ids.len(), ids[0].as_str()), (KEEP, "s6"));
    }
}
