//! The hearing test set: labelled loops, from Live's packs or rendered (MIDI written here, played
//! through Live instruments), heard through the Ear with `record_beats` on and scored offline.
//!
//! `testdata/hearing-set.json` describes each loop: where it comes from, its family, its split
//! ("tune" for loops the interpreter may be tuned on, "holdout" for reporting only) and its labels.
//! A run folder holds the recordings (`<session>-<snapshot>.json`, as `listen` writes them) and
//! `takes.json`, which maps recording ids to loop ids. To score runs (folders separated by `:`):
//!
//! ```text
//! HEARD=~/.live-mcp/recordings/run4 cargo test -p live-mcp-server --bins score_recordings -- --ignored --nocapture
//! ```
//!
//! `BASS=1` also prints the heard bass lines. To get the notes of the rendered loops, to write
//! into Live with `write_clip`:
//!
//! ```text
//! RENDER=notes.json cargo test -p live-mcp-server --bins write_rendered_notes -- --ignored
//! ```

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use live_mcp_analysis::BeatReport;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::chords::{self, Chord, QUALITIES};
use super::key::Key;
use super::midi::{self, MidiNote};
use super::{BarSpan, Section, Source, analyze, audio};

const SET: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/hearing-set.json");

#[derive(Debug, Deserialize)]
struct TestSet {
    loops: Vec<Loop>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Loop {
    pub id: String,
    /// Instrument family, for scores per family.
    pub category: String,
    pub split: Split,
    pub tempo: f64,
    /// Time signature; default 4/4.
    #[serde(default = "four_four")]
    pub meter: [u32; 2],
    /// Bars to listen to.
    pub bars: u32,
    /// A pack sample, relative to Live's Factory Packs folder.
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub render: Option<Render>,
    /// "A minor", a tonic alone ("F#"), or "none" for no key (drums, one repeated note).
    #[serde(default)]
    pub key: Option<String>,
    /// Where the key label comes from: the loop itself, or the construction kit it is from (a
    /// file name's key: the loop may sit on another degree of it).
    #[serde(default)]
    pub key_source: Option<KeySource>,
    /// Chords, bar by bar (see [`progression`]). Rendered loops take them from their progression.
    #[serde(default)]
    pub chords: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn four_four() -> [u32; 2] {
    [4, 4]
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Tune,
    Holdout,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    Loop,
    Kit,
}

/// A loop written as MIDI and played through an instrument.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Render {
    /// Browser path of the instrument (preset) to play it with.
    pub instrument: Vec<String>,
    /// The chords (see [`progression`]); a slash chord puts that note in the bass.
    pub progression: String,
    #[serde(default)]
    pub style: Style,
    /// Whether a bass note plays under the chords (default true).
    #[serde(default = "yes")]
    pub bass: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// Chords held, bass held.
    #[default]
    Sustain,
    /// Short chords on a 3+3+2 eighth pattern, bass on every eighth.
    Stabs,
    /// The chord's notes one after another in 16ths, bass held.
    Arp,
}

impl Loop {
    fn beats_per_bar(&self) -> f64 {
        self.meter[0] as f64 * 4.0 / self.meter[1] as f64
    }

    fn chord_text(&self) -> Option<&str> {
        self.render.as_ref().map(|r| r.progression.as_str()).or(self.chords.as_deref())
    }

    fn changes(&self) -> Option<Result<Vec<Change>, String>> {
        self.chord_text().map(|text| progression(text, self.beats_per_bar()))
    }
}

fn load_set() -> Vec<Loop> {
    let text = std::fs::read_to_string(SET).expect("testdata/hearing-set.json");
    serde_json::from_str::<TestSet>(&text).unwrap_or_else(|e| panic!("hearing-set.json: {e}")).loops
}

// ------------------------------------------------------------------ labels

/// A chord and the beat (quarter notes from the loop start) where it starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    pub at: f64,
    pub chord: Option<Chord>,
}

/// Chords bar by bar: bars separated by `|`, chords in a bar by spaces. Chords in a bar share it
/// equally ("Dm7 G7" in 4/4: two beats each), or say where they start with `@` and the beat,
/// counted from 1 ("C F@4"; the first chord of a bar starts on it). "N" is no chord.
pub fn progression(text: &str, beats_per_bar: f64) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();
    for (bar, cell) in text.split('|').enumerate() {
        let tokens: Vec<&str> = cell.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(format!("bar {} of {text:?} is empty", bar + 1));
        }
        let start = bar as f64 * beats_per_bar;
        let timed = tokens.iter().skip(1).any(|t| t.contains('@'));
        for (k, token) in tokens.iter().enumerate() {
            let (name, at) = match token.split_once('@') {
                Some((name, beat)) => {
                    let beat: f64 = beat.parse().map_err(|_| format!("bad beat in {token:?}"))?;
                    if !(1.0..beats_per_bar + 1.0).contains(&beat) {
                        return Err(format!("{token:?} starts outside its bar"));
                    }
                    (name, beat - 1.0)
                }
                None if !timed => (*token, k as f64 * beats_per_bar / tokens.len() as f64),
                None if k == 0 => (*token, 0.0),
                None => return Err(format!("{token:?}: after a timed chord, time every chord in the bar")),
            };
            let chord = chords::parse(name).ok_or_else(|| format!("unknown chord {name:?}"))?;
            if out.last().is_some_and(|c: &Change| start + at <= c.at) {
                return Err(format!("{token:?} does not start after the chord before it"));
            }
            out.push(Change { at: start + at, chord });
        }
    }
    Ok(out)
}

/// The chord sounding at time `t` (beats), the progression repeating every `length` beats.
fn chord_at(changes: &[Change], length: f64, t: f64) -> Option<Chord> {
    let t = t.rem_euclid(length);
    changes.iter().rev().find(|c| c.at <= t + 1e-9).and_then(|c| c.chord)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    NoKey,
    Tonic(u8),
    Key(Key),
}

/// "C major", "C maj", "Cmaj", "Cm", "C minor", "Cmin", a tonic alone ("Bb"), or "none".
fn expected_key(label: &str) -> Option<Expect> {
    let label = label.trim();
    if label == "none" {
        return Some(Expect::NoKey);
    }
    let split = label.find([' ', 'm']).unwrap_or(label.len());
    let (note, rest) = label.split_at(split);
    let tonic = chords::pitch_class(note)?;
    match rest.trim() {
        "" => Some(Expect::Tonic(tonic)),
        "major" | "maj" => Some(Expect::Key(Key { tonic, minor: false })),
        "minor" | "min" | "m" => Some(Expect::Key(Key { tonic, minor: true })),
        _ => None,
    }
}

/// A heard key name ("Db major") as a key.
fn heard_key(name: &str) -> Option<Key> {
    match expected_key(name)? {
        Expect::Key(key) => Some(key),
        _ => None,
    }
}

// ------------------------------------------------------------------ rendering

/// Chord tones sit in this range (G2–F#3 in Live's names), the bass note below it (C1–B1).
const UPPER_LOW: u8 = 55;
const BASS_LOW: u8 = 36;

fn place(pc: u8, low: u8) -> u8 {
    low + (pc + 12 - low % 12) % 12
}

/// The notes of a rendered loop, `bars` long (the progression repeats as needed).
pub fn notes(l: &Loop) -> Vec<MidiNote> {
    let render = l.render.as_ref().expect("a rendered loop");
    let changes = progression(&render.progression, l.beats_per_bar()).expect("a valid progression");
    let length = l.bars as f64 * l.beats_per_bar();
    let cycle = (render.progression.split('|').count() as f64 * l.beats_per_bar()).min(length);
    let mut out = Vec::new();
    let note = |pitch, start, duration, velocity| MidiNote { pitch, start, duration, velocity };
    let mut offset = 0.0;
    while offset < length - 1e-9 {
        for (k, change) in changes.iter().enumerate() {
            let start = offset + change.at;
            let end = (offset + changes.get(k + 1).map_or(cycle, |c| c.at)).min(length);
            let Some(chord) = change.chord.filter(|_| start < end - 1e-9) else { continue };
            let mut upper: Vec<u8> = chord.tones().map(|pc| place(pc, UPPER_LOW)).collect();
            upper.sort_unstable();
            let bass = place(chord.bass.unwrap_or(chord.root), BASS_LOW);
            let span = end - start;
            // The bass a little softer than the chord, as in a mix, so that the chord's notes
            // outweigh it.
            match render.style {
                Style::Sustain => {
                    out.extend(upper.iter().map(|&p| note(p, start, span - 0.02, 90.0)));
                    if render.bass {
                        out.push(note(bass, start, span - 0.02, 80.0));
                    }
                }
                Style::Stabs => {
                    // Chords on eighths 1, 4 and 7 of every eight, each ringing for most of the
                    // time to the next; the bass on every beat.
                    let hits: Vec<f64> = (0..)
                        .map(|k: usize| (k / 3) as f64 * 4.0 + [0.0, 1.5, 3.0][k % 3])
                        .take_while(|t| *t < span - 1e-9)
                        .collect();
                    for (k, &t) in hits.iter().enumerate() {
                        let next = hits.get(k + 1).copied().unwrap_or(span).min(span);
                        out.extend(upper.iter().map(|&p| note(p, start + t, 0.8 * (next - t), 95.0)));
                    }
                    let mut t = 0.0;
                    while render.bass && t < span - 1e-9 {
                        out.push(note(bass, start + t, 0.45_f64.min(span - t), 80.0));
                        t += 1.0;
                    }
                }
                Style::Arp => {
                    // The chord's notes in turn, each ringing until it comes round again, as with
                    // the sustain pedal down.
                    let ring = upper.len() as f64 * 0.25 - 0.02;
                    let mut t = 0.0;
                    let mut k = 0;
                    while t < span - 1e-9 {
                        out.push(note(upper[k % upper.len()], start + t, ring.min(span - t - 0.02), 90.0));
                        t += 0.25;
                        k += 1;
                    }
                    if render.bass {
                        out.push(note(bass, start, span - 0.02, 80.0));
                    }
                }
            }
        }
        offset += cycle;
    }
    out
}

// ------------------------------------------------------------------ recordings

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
        if path.extension().is_none_or(|e| e != "json") || ["labels", "takes"].contains(&id.as_str()) {
            continue;
        }
        let Ok(Ok(body)) = std::fs::read_to_string(&path).map(|t| serde_json::from_str::<Value>(&t)) else { continue };
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
    out.sort_by_key(|r| order(&r.id));
    out
}

/// Recordings in the order they were made: by session (older plain `s<N>` names first), then by
/// snapshot number.
fn order(id: &str) -> (u64, u64, String) {
    let (session, snapshot) = match id.split_once('-') {
        Some((session, snapshot)) => (session.parse().unwrap_or(u64::MAX), snapshot),
        None => (0, id),
    };
    (session, snapshot.trim_start_matches('s').parse().unwrap_or(u64::MAX), id.to_owned())
}

// ------------------------------------------------------------------ scoring

/// Counts of right answers out of tries.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    right: usize,
    of: usize,
}

impl Tally {
    fn add(&mut self, right: bool) {
        self.right += right as usize;
        self.of += 1;
    }

    fn merge(&mut self, other: Tally) {
        self.right += other.right;
        self.of += other.of;
    }

    fn text(self) -> String {
        if self.of == 0 { "-".into() } else { format!("{}/{}", self.right, self.of) }
    }

    fn percent(self) -> String {
        if self.of == 0 { "-".into() } else { format!("{:.0}%", 100.0 * self.right as f64 / self.of as f64) }
    }
}

/// The scores of one recording, or of a group of them.
#[derive(Debug, Default, Clone, Copy)]
struct Score {
    loops: usize,
    /// Key exactly right (for a tonic-only label, its tonic), by where the label comes from.
    key_loop: Tally,
    key_kit: Tally,
    /// Tonic right, whatever the mode.
    tonic: Tally,
    /// Per beat: root right; root and third (major or minor) right; quality right, and the
    /// inversion where the label gives it.
    root: Tally,
    majmin: Tally,
    exact: Tally,
    /// Per bar, as earlier runs were scored: each labelled chord heard somewhere in its bar (root and
    /// third, any extension or inversion).
    bars: Tally,
    /// Per 16th of a rendered loop's bass: pitch class right, and the exact note.
    bass_pc: Tally,
    bass_note: Tally,
}

impl Score {
    fn merge(&mut self, o: &Score) {
        self.loops += o.loops;
        for (a, b) in [
            (&mut self.key_loop, o.key_loop),
            (&mut self.key_kit, o.key_kit),
            (&mut self.tonic, o.tonic),
            (&mut self.root, o.root),
            (&mut self.majmin, o.majmin),
            (&mut self.exact, o.exact),
            (&mut self.bars, o.bars),
            (&mut self.bass_pc, o.bass_pc),
            (&mut self.bass_note, o.bass_note),
        ] {
            a.merge(b);
        }
    }

    fn row(&self, name: &str) -> String {
        format!(
            "{name:24} {:>5} {:>7} {:>7} {:>7} | {:>5} {:>6} {:>6} {:>7} | {:>5} {:>5}",
            self.loops,
            self.key_loop.text(),
            self.key_kit.text(),
            self.tonic.text(),
            self.root.percent(),
            self.majmin.percent(),
            self.exact.percent(),
            self.bars.text(),
            self.bass_pc.percent(),
            self.bass_note.percent(),
        )
    }
}

const HEADER: &str =
    "                         loops  key(l)  key(k)   tonic |  root majmin  exact    bars | bass  note";

/// Major, minor or neither (sus, power chords): the third, for the "majmin" score.
fn third(chord: &Chord) -> Option<bool> {
    let intervals = QUALITIES[chord.quality].intervals;
    if intervals.contains(&4) {
        Some(true)
    } else if intervals.contains(&3) {
        Some(false)
    } else {
        None
    }
}

/// What the interpreter said about one bar: its chords from where each starts (beats from the
/// bar's start), parsed back from the names it printed.
fn heard_segments(fields: &Map<String, Value>) -> Vec<(f64, Option<Chord>)> {
    let parse = |v: &Value| v.as_str().and_then(chords::parse).flatten();
    if let Some(name) = fields.get("chord") {
        return vec![(0.0, parse(name))];
    }
    let Some(list) = fields.get("chords").and_then(Value::as_array) else { return Vec::new() };
    list.iter().map(|c| (c["beat"].as_f64().unwrap_or(1.0) - 1.0, parse(&c["chord"]))).collect()
}

fn score(l: &Loop, rec: &Recording, verbose: bool) -> Score {
    let (beats, tuning) = audio::beats(&rec.beats);
    score_beats(l, &rec.bars, &beats, Source::Audio, tuning, verbose)
}

/// A rendered loop read from its own MIDI, as `read_clip` reads it: the interpreter on exact input.
fn score_midi(l: &Loop) -> Score {
    let bars: Vec<BarSpan> = (0..l.bars as i64)
        .map(|k| BarSpan { bar: k + 1, start_beat: k as f64 * l.beats_per_bar(), beats: l.beats_per_bar() })
        .collect();
    let beats = midi::beats(&notes(l), 0.0, l.bars as f64 * l.beats_per_bar());
    score_beats(l, &bars, &beats, Source::Midi, None, false)
}

fn score_beats(
    l: &Loop,
    bars: &[BarSpan],
    beats: &[super::Beat],
    source: Source,
    tuning: Option<f32>,
    verbose: bool,
) -> Score {
    let analysis = analyze(&Section { bars, beats, source, scale: None });
    let summary = &analysis.summary;
    let mut s = Score { loops: 1, ..Score::default() };

    let got_key = summary.get("key").and_then(|k| k.get("name")).and_then(Value::as_str);
    let mut key_verdict = String::new();
    if let Some(want) = l.key.as_deref() {
        let want = expected_key(want).unwrap_or_else(|| panic!("{}: key {want:?}", l.id));
        let heard = got_key.and_then(heard_key);
        let right = match want {
            Expect::NoKey => heard.is_none(),
            Expect::Tonic(t) => heard.is_some_and(|k| k.tonic == t),
            Expect::Key(k) => heard == Some(k),
        };
        match l.key_source.unwrap_or(KeySource::Loop) {
            KeySource::Loop => s.key_loop.add(right),
            KeySource::Kit => s.key_kit.add(right),
        }
        if let Expect::Key(k) = want {
            s.tonic.add(heard.is_some_and(|h| h.tonic == k.tonic));
        }
        key_verdict = if right { "key ok".into() } else { "KEY??".into() };
    }

    let heard_bars: BTreeMap<i64, &Map<String, Value>> = analysis.bars.iter().map(|(b, f)| (*b, f)).collect();
    // Only a rendered loop's bass part says which inversion is played; labels from file names,
    // and loops without a bass part, leave it to the voicing.
    let bass_known = l.render.as_ref().is_some_and(|r| r.bass);
    if let Some(changes) = l.changes() {
        let changes = changes.unwrap_or_else(|e| panic!("{}: {e}", l.id));
        let length = l.chord_text().unwrap_or("").split('|').count() as f64 * l.beats_per_bar();
        for bar in bars {
            let Some(fields) = heard_bars.get(&bar.bar) else { continue };
            let segments = heard_segments(fields);
            // The song starts with the loop, so song beats are the loop's.
            let first = bar.start_beat.ceil() as i64;
            let last = (bar.start_beat + bar.beats - 1e-9).ceil() as i64;
            for index in first..last {
                let want = chord_at(&changes, length, index as f64 + 0.5);
                let offset = index as f64 + 0.5 - bar.start_beat;
                let got = segments.iter().rev().find(|(at, _)| *at <= offset).and_then(|(_, c)| *c);
                let (root, majmin, exact) = match (want, got) {
                    (None, None) => (true, true, true),
                    (Some(w), Some(g)) => {
                        let root = w.root == g.root;
                        let quality = root && w.quality == g.quality;
                        (root, root && third(&w) == third(&g), quality && (!bass_known || w.bass == g.bass))
                    }
                    _ => (false, false, false),
                };
                s.root.add(root);
                s.majmin.add(majmin);
                s.exact.add(exact);
            }
            // Per bar: each labelled chord starting in it heard somewhere in it.
            let start = bar.start_beat.rem_euclid(length);
            for change in changes.iter().filter(|c| c.at >= start - 1e-9 && c.at < start + bar.beats - 1e-9) {
                let hit = segments.iter().any(|(_, g)| match (change.chord, g) {
                    (None, None) => true,
                    (Some(w), Some(g)) => w.root == g.root && third(&w) == third(g),
                    _ => false,
                });
                s.bars.add(hit);
            }
        }
    }

    // The bass line against the rendered bass, 16th by 16th.
    if l.render.as_ref().is_some_and(|r| r.bass) {
        let notes = notes(l);
        let length = l.bars as f64 * l.beats_per_bar();
        let truth: BTreeMap<i64, _> = midi::beats(&notes, 0.0, length).into_iter().map(|b| (b.index, b)).collect();
        for beat in beats {
            let Some(want) = truth.get(&beat.index) else { continue };
            for (w, g) in want.bass_line.iter().zip(&beat.bass_line) {
                if let Some(w) = w {
                    s.bass_pc.add(g.is_some_and(|g| g.pitch % 12 == w.pitch % 12));
                    s.bass_note.add(g.is_some_and(|g| g.pitch == w.pitch));
                }
            }
        }
    }

    let progression = summary.get("progression").and_then(Value::as_str).unwrap_or("");
    println!(
        "{:28} {:9} {key_verdict:6} key={:12} conf={:4} want={:10} chords {}/{} bass {}  tuning={}\n      {progression}",
        l.id,
        l.category,
        got_key.unwrap_or("-"),
        summary.get("key").and_then(|k| k.get("confidence")).map_or("-".into(), |c| c.to_string()),
        l.key.as_deref().unwrap_or("-"),
        s.majmin.right,
        s.majmin.of,
        s.bass_pc.percent(),
        tuning.map_or("-".into(), |c| format!("{c:+.0}")),
    );
    if let Some(note) = &l.note {
        println!("      ({note})");
    }
    if verbose {
        for (bar, fields) in &analysis.bars {
            if let Some(line) = fields.get("bass_line").and_then(Value::as_str) {
                println!("      bar {bar}: {line}");
            }
        }
    }
    s
}

#[test]
#[ignore = "needs recordings: HEARD=<run folder>[:<run folder>…]"]
fn score_recordings() {
    let Ok(dirs) = std::env::var("HEARD") else { return };
    let set = load_set();
    let by_id: BTreeMap<&str, &Loop> = set.iter().map(|l| (l.id.as_str(), l)).collect();
    let verbose = std::env::var("BASS").is_ok();
    // Each loop once, from its latest take: the last folder that has one, its last recording.
    let mut latest: BTreeMap<&str, (String, Recording)> = BTreeMap::new();
    for dir in dirs.split(':') {
        let dir = Path::new(dir);
        let takes: BTreeMap<String, String> = std::fs::read_to_string(dir.join("takes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for rec in load(dir) {
            match takes.get(&rec.id).and_then(|id| by_id.get_key_value(id.as_str())) {
                Some((id, _)) => {
                    latest.insert(id, (dir.display().to_string(), rec));
                }
                None if !takes.is_empty() => println!("(no loop for {} in {})", rec.id, dir.display()),
                None => {}
            }
        }
    }
    report(&set, &latest, verbose);
}

/// Scores each loop that has a recording, rendered loops also from their MIDI, and prints the
/// scores per family, split and group.
fn report(set: &[Loop], latest: &BTreeMap<&str, (String, Recording)>, verbose: bool) {
    let mut by_category: BTreeMap<(String, Split), Score> = BTreeMap::new();
    for l in set {
        let Some((dir, rec)) = latest.get(l.id.as_str()) else { continue };
        println!("[{}/{}]", Path::new(dir).file_name().and_then(|n| n.to_str()).unwrap_or(dir), rec.id);
        let s = score(l, rec, verbose);
        by_category.entry((l.category.clone(), l.split)).or_default().merge(&s);
    }
    // The rendered loops from their MIDI, as a ceiling for what the Ear can hear of them.
    println!("\n== rendered loops read from their MIDI");
    for l in set.iter().filter(|l| l.render.is_some()) {
        let category = format!("midi:{}", l.category.trim_start_matches("r:"));
        by_category.entry((category, l.split)).or_default().merge(&score_midi(l));
    }
    let missing: Vec<&str> = set.iter().map(|l| l.id.as_str()).filter(|id| !latest.contains_key(id)).collect();
    if !missing.is_empty() {
        println!("\nnot recorded yet ({}): {}", missing.len(), missing.join(", "));
    }
    println!("\n{HEADER}");
    // Totals per group: pack loops, rendered loops as heard, and rendered loops from their MIDI.
    let group = |category: &str| match category.split_once(':') {
        Some(("r", _)) => "RENDERED",
        Some(("midi", _)) => "MIDI",
        _ => "REAL",
    };
    let mut totals: BTreeMap<(&str, Option<Split>), Score> = BTreeMap::new();
    for ((category, split), s) in &by_category {
        println!("{}", s.row(&format!("{category} ({split:?})").to_lowercase()));
        totals.entry((group(category), Some(*split))).or_default().merge(s);
        totals.entry((group(category), None)).or_default().merge(s);
    }
    println!();
    for ((name, split), s) in &totals {
        let label = match split {
            Some(split) => format!("{name} {split:?}").to_uppercase(),
            None => format!("{name} ALL"),
        };
        println!("{}", s.row(&label));
    }
    println!(
        "\nkey(l): key right where it is the loop's own; key(k): where it is the kit's (from a file \
         name); tonic: tonic right, any mode. Chords per beat: root; root and third; exact (quality, \
         and inversion where a rendered bass part gives it). bars: labelled chords heard in their bar. \
         bass: pitch class and exact note per 16th (rendered loops)."
    );
}

/// Host buffer size for hearing audio files: what Live typically gives a plugin.
const BLOCK: usize = 512;

/// A loop's audio file heard as the Ear plugin would hear it in Live: the plugin's analyzers fed in
/// host-sized blocks with a transport playing from beat 0 at the loop's tempo and meter.
fn hear_audio_file(l: &Loop, path: &Path) -> anyhow::Result<Recording> {
    let decoded = crate::audio_file::decode(path)?;
    let sr = decoded.sample_rate as f64;
    let mut bar_analyzer = live_mcp_analysis::BarAnalyzer::new(decoded.sample_rate as f32);
    let mut beat_analyzer = live_mcp_analysis::BeatAnalyzer::new(decoded.sample_rate as f32);
    let (mut bar_reports, mut beats) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < decoded.left.len() {
        let end = (i + BLOCK).min(decoded.left.len());
        let transport = live_mcp_analysis::Transport {
            playing: true,
            beat: Some(i as f64 / sr * l.tempo / 60.0),
            tempo: Some(l.tempo),
            time_signature: Some((l.meter[0], l.meter[1])),
        };
        bar_analyzer.process(&decoded.left[i..end], &decoded.right[i..end], &transport, &mut |r| bar_reports.push(r));
        beat_analyzer.process(&decoded.left[i..end], &decoded.right[i..end], &transport, &mut |r| beats.push(r));
        i = end;
    }
    // As listen does: the bars asked for, heard at least half.
    let bars = bar_reports
        .iter()
        .filter(|r| r.coverage >= 0.5 && (1..=l.bars as i64).contains(&r.bar))
        .map(|r| BarSpan { bar: r.bar, start_beat: r.start_beat, beats: r.beats })
        .collect();
    let length = l.bars as f64 * l.beats_per_bar();
    beats.retain(|b: &BeatReport| (b.beat as f64) < length);
    Ok(Recording { id: path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned(), bars, beats })
}

/// Scores loops from their audio files (`<loop id>.wav` in AUDIO), heard offline as the Ear would
/// hear them: for trying changes to the plugin's analysis without rebuilding or reinstalling it.
///
/// ```text
/// AUDIO=~/.live-mcp/recordings/run5/audio cargo test -p live-mcp-server --bins score_audio -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs audio files: AUDIO=<folder of <loop id>.wav>"]
fn score_audio() {
    let Ok(dir) = std::env::var("AUDIO") else { return };
    // AUDIO_OUT: also write what was heard as recordings (and takes.json), for the other tools.
    let out = std::env::var("AUDIO_OUT").ok();
    let set = load_set();
    let mut latest: BTreeMap<&str, (String, Recording)> = BTreeMap::new();
    let mut takes = BTreeMap::new();
    for l in &set {
        let path = Path::new(&dir).join(format!("{}.wav", l.id));
        if path.exists() {
            let rec = hear_audio_file(l, &path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            if let Some(out) = &out {
                let bars: Vec<Value> = rec
                    .bars
                    .iter()
                    .map(|b| json!({"bar": b.bar, "start_beat": b.start_beat, "beats": b.beats, "coverage": 1.0}))
                    .collect();
                let body = json!({"label": format!("{} heard offline", l.id), "tempo": l.tempo, "bars": bars, "beats": rec.beats});
                std::fs::create_dir_all(out).unwrap();
                std::fs::write(Path::new(out).join(format!("{}.json", l.id)), body.to_string()).unwrap();
                takes.insert(l.id.clone(), l.id.clone());
            }
            latest.insert(l.id.as_str(), ("audio".into(), rec));
        }
    }
    if let Some(out) = &out {
        std::fs::write(Path::new(out).join("takes.json"), serde_json::to_string_pretty(&takes).unwrap()).unwrap();
    }
    report(&set, &latest, std::env::var("BASS").is_ok());
}

#[test]
#[ignore = "writes the rendered loops' notes: RENDER=<file>"]
fn write_rendered_notes() {
    let Ok(path) = std::env::var("RENDER") else { return };
    let list: Vec<Value> = load_set()
        .iter()
        .filter(|l| l.render.is_some())
        .map(|l| {
            let r = l.render.as_ref().unwrap();
            let notes: Vec<Value> = notes(l)
                .iter()
                .map(|n| json!({"pitch": n.pitch, "start": n.start, "duration": n.duration, "velocity": n.velocity}))
                .collect();
            json!({
                "id": l.id,
                "tempo": l.tempo,
                "meter": l.meter,
                "bars": l.bars,
                "length": l.bars as f64 * l.beats_per_bar(),
                "instrument": r.instrument,
                "notes": notes,
            })
        })
        .collect();
    std::fs::write(path, serde_json::to_string_pretty(&list).unwrap()).unwrap();
}

// ------------------------------------------------------------------ checks that always run

#[test]
fn the_test_set_is_well_formed() {
    let set = load_set();
    let mut ids = HashSet::new();
    for l in &set {
        assert!(ids.insert(l.id.as_str()), "{} is listed twice", l.id);
        assert!(l.file.is_some() != l.render.is_some(), "{}: give a file or a render", l.id);
        assert!(!l.category.is_empty() && l.bars > 0 && l.tempo > 0.0, "{}", l.id);
        assert!([2, 4, 8, 16].contains(&l.meter[1]) && l.meter[0] > 0, "{}: meter", l.id);
        if let Some(key) = &l.key {
            assert!(expected_key(key).is_some(), "{}: key {key:?}", l.id);
        }
        assert!(l.key_source.is_none() || l.key.is_some(), "{}: a key source without a key", l.id);
        if let Some(changes) = l.changes() {
            changes.unwrap_or_else(|e| panic!("{}: {e}", l.id));
        }
        if let Some(r) = &l.render {
            assert!(l.chords.is_none(), "{}: a rendered loop's chords are its progression", l.id);
            assert_eq!(r.progression.split('|').count() as u32, l.bars, "{}: one bar of progression per bar", l.id);
        }
    }
}

/// The rendered progressions, read back from their own MIDI by `read_clip`'s interpreter: a check
/// of the notes written, and of the keys.
#[test]
fn rendered_loops_read_back_from_their_midi() {
    let mut wrong = Vec::new();
    let mut wrong_keys = Vec::new();
    let mut total = Tally::default();
    for l in load_set().iter().filter(|l| l.render.is_some()) {
        let notes = notes(l);
        let length = l.bars as f64 * l.beats_per_bar();
        let beats = midi::beats(&notes, 0.0, length);
        let bars: Vec<BarSpan> = (0..l.bars as i64)
            .map(|k| BarSpan { bar: k + 1, start_beat: k as f64 * l.beats_per_bar(), beats: l.beats_per_bar() })
            .collect();
        let analysis = analyze(&Section { bars: &bars, beats: &beats, source: Source::Midi, scale: None });
        let changes = l.changes().unwrap().unwrap();
        for (bar, fields) in &analysis.bars {
            let span = bars[(*bar - 1) as usize];
            let segments = heard_segments(fields);
            for index in span.start_beat.ceil() as i64..(span.start_beat + span.beats - 1e-9).ceil() as i64 {
                let want = chord_at(&changes, length, index as f64 + 0.5);
                let offset = index as f64 + 0.5 - span.start_beat;
                let got = segments.iter().rev().find(|(at, _)| *at <= offset).and_then(|(_, c)| *c);
                let right = want.map(|w| w.root) == got.map(|g| g.root);
                total.add(right);
                if !right {
                    wrong.push(format!("{} beat {index}: want {want:?}, got {got:?}", l.id));
                }
            }
        }
        if let Some(Expect::Key(want)) = l.key.as_deref().and_then(expected_key) {
            let got = analysis.summary.get("key").and_then(|k| k.get("name")).and_then(Value::as_str);
            if got.and_then(heard_key) != Some(want) {
                wrong_keys.push(format!("{}: {got:?}, want {}", l.id, want.name()));
            }
        }
    }
    // Every rendered progression's key, from its exact notes.
    assert!(wrong_keys.is_empty(), "keys wrong:\n{}", wrong_keys.join("\n"));
    // Not every beat: a chord whose bass note is not its root can read as another chord on exact
    // input too. Keys are left to the scores.
    assert!(
        total.right as f64 >= 0.9 * total.of as f64,
        "{} of {} beats wrong:\n{}",
        wrong.len(),
        total.of,
        wrong.join("\n")
    );
}

#[test]
fn reads_chord_names_back() {
    let c = |name| chords::parse(name).unwrap().unwrap();
    assert_eq!(c("Am7"), Chord { root: 9, quality: 4, bass: None });
    assert_eq!(c("C/E"), Chord { root: 0, quality: 0, bass: Some(4) });
    assert_eq!(c("F#m7b5"), Chord { root: 6, quality: 6, bass: None });
    assert_eq!(c("Bbmaj7"), Chord { root: 10, quality: 3, bass: None });
    assert_eq!(chords::parse("N"), Some(None));
    assert_eq!(chords::parse("C/F#"), None, "F# is not in C major");
    assert_eq!(chords::parse("Hm"), None);
    for state in 0..12 * QUALITIES.len() {
        let chord = Chord { root: (state % 12) as u8, quality: state / 12, bass: None };
        let name = chord.name(super::names::spelling(None));
        assert_eq!(chords::parse(&name), Some(Some(chord)), "{name}");
    }
}

#[test]
fn reads_progressions() {
    let p = progression("Am | Dm7 G7 | C F@4", 4.0).unwrap();
    let at: Vec<f64> = p.iter().map(|c| c.at).collect();
    assert_eq!(at, [0.0, 4.0, 6.0, 8.0, 11.0]);
    assert_eq!(chord_at(&p, 12.0, 6.5).map(|c| c.root), Some(7));
    assert_eq!(chord_at(&p, 12.0, 12.5).map(|c| c.root), Some(9), "it repeats");
    assert_eq!(progression("C D@2.5", 3.5).unwrap()[1].at, 1.5, "7/8");
    assert!(progression("C | ", 4.0).is_err());
    assert!(progression("C@2", 4.0).is_ok(), "a timed first chord");
    assert!(progression("C F@3 G", 4.0).is_err());
    assert!(progression("C F@5", 4.0).is_err());
}

#[test]
fn key_labels_name_their_mode() {
    let key = |tonic, minor| Some(Expect::Key(Key { tonic, minor }));
    assert_eq!(expected_key("C major"), key(0, false));
    assert_eq!(expected_key("C maj"), key(0, false));
    assert_eq!(expected_key("Cmaj"), key(0, false));
    assert_eq!(expected_key("Cm"), key(0, true));
    assert_eq!(expected_key("C minor"), key(0, true));
    assert_eq!(expected_key("Bb"), Some(Expect::Tonic(10)));
    assert_eq!(expected_key("none"), Some(Expect::NoKey));
    assert_eq!(expected_key("C dorian"), None);
}

#[test]
fn recordings_sort_by_session_then_snapshot() {
    let mut ids = vec!["1700000100-s2", "s10", "1700000100-s10", "s2", "1700000000-s5"];
    ids.sort_by_key(|id| order(id));
    assert_eq!(ids, ["s2", "s10", "1700000000-s5", "1700000100-s2", "1700000100-s10"]);
}

#[test]
fn renders_each_style() {
    let l = |style| Loop {
        id: "t".into(),
        category: "t".into(),
        split: Split::Tune,
        tempo: 120.0,
        meter: [4, 4],
        bars: 1,
        file: None,
        render: Some(Render { instrument: Vec::new(), progression: "C/E".into(), style, bass: true }),
        key: None,
        key_source: None,
        chords: None,
        note: None,
    };
    let sustain = notes(&l(Style::Sustain));
    let mut pitches: Vec<u8> = sustain.iter().map(|n| n.pitch).collect();
    pitches.sort_unstable();
    assert_eq!(pitches, [40, 55, 60, 64], "E1 in the bass, then G2 C3 E3");
    let stabs = notes(&l(Style::Stabs));
    assert_eq!(stabs.iter().filter(|n| n.pitch == 40).count(), 4, "bass on every beat");
    let hits: Vec<f64> = stabs.iter().filter(|n| n.pitch == 60).map(|n| n.start).collect();
    assert_eq!(hits, [0.0, 1.5, 3.0], "chords on 3+3+2 eighths");
    let arp = notes(&l(Style::Arp));
    assert_eq!(arp.iter().filter(|n| n.pitch != 40).count(), 16);
    assert!(arp.iter().all(|n| n.start + n.duration <= 4.0 + 1e-9));
}

/// For each loop with a key label: where the labelled key ranks, and by how much it trails the
/// winner (pitch profile, downbeat bass and the chords heard), from the audio of its latest take
/// and (rendered loops) from its MIDI.
#[test]
#[ignore = "needs recordings: HEARD=<run folder>[:<run folder>…]"]
fn key_diagnostics() {
    let Ok(dirs) = std::env::var("HEARD") else { return };
    let set = load_set();
    let mut latest: BTreeMap<String, Recording> = BTreeMap::new();
    for dir in dirs.split(':') {
        let dir = Path::new(dir);
        let takes: BTreeMap<String, String> = std::fs::read_to_string(dir.join("takes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for rec in load(dir) {
            if let Some(id) = takes.get(&rec.id) {
                latest.insert(id.clone(), rec);
            }
        }
    }
    for l in &set {
        let Some(Expect::Key(want)) = l.key.as_deref().and_then(expected_key) else { continue };
        let mut sources: Vec<(&str, Vec<BarSpan>, Vec<super::Beat>)> = Vec::new();
        if l.render.is_some() {
            let bars: Vec<BarSpan> = (0..l.bars as i64)
                .map(|k| BarSpan { bar: k + 1, start_beat: k as f64 * l.beats_per_bar(), beats: l.beats_per_bar() })
                .collect();
            sources.push(("midi", bars, midi::beats(&notes(l), 0.0, l.bars as f64 * l.beats_per_bar())));
        }
        if let Some(rec) = latest.get(&l.id) {
            sources.push(("audio", rec.bars.clone(), audio::beats(&rec.beats).0));
        }
        for (source, bars, beats) in &sources {
            let kind = if *source == "midi" { Source::Midi } else { Source::Audio };
            let analysis = analyze(&Section { bars, beats, source: kind, scale: None });
            let chords: Vec<Chord> = beats
                .iter()
                .filter(|b| b.pitched && !b.bass_only)
                .filter_map(|b| analysis.chords.get(&b.index).copied().flatten())
                .collect();
            let profile = super::key_profile(beats.iter(), bars);
            let hint = super::downbeat_bass(beats.iter(), bars);
            let Some(ranked) = super::key::ranking(&profile, hint.as_ref(), &chords) else {
                println!("{:30} {source:5} want {:9} -> no key", l.id, want.name());
                continue;
            };
            let rank = ranked.iter().position(|(k, _)| *k == want).unwrap();
            let gap = ranked[0].1 - ranked[rank].1;
            let top: Vec<String> = ranked.iter().take(3).map(|(k, s)| format!("{} {s:.3}", k.name())).collect();
            println!(
                "{:30} {source:5} {} want {:9} rank {:2} gap {gap:.3} | {}",
                l.id,
                if rank == 0 { "ok " } else { "XX " },
                want.name(),
                rank + 1,
                top.join(", ")
            );
        }
    }
}

/// Key accuracy for a range of weights of the chords' harmonic fit (`key::HARMONY_WEIGHT`), by
/// group and split: choose on "tune", report "holdout".
#[test]
#[ignore = "needs recordings: HEARD=<run folder>[:<run folder>…]"]
fn key_weight_search() {
    let Ok(dirs) = std::env::var("HEARD") else { return };
    let set = load_set();
    let mut latest: BTreeMap<String, Recording> = BTreeMap::new();
    for dir in dirs.split(':') {
        let dir = Path::new(dir);
        let takes: BTreeMap<String, String> = std::fs::read_to_string(dir.join("takes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for rec in load(dir) {
            if let Some(id) = takes.get(&rec.id) {
                latest.insert(id.clone(), rec);
            }
        }
    }
    let weights: Vec<f32> = (0..=12).map(|k| k as f32 * 0.1).collect();
    // (group, split) → right answers per weight, and tries.
    let mut tally: BTreeMap<(String, Split), (Vec<usize>, usize)> = BTreeMap::new();
    for l in &set {
        let Some(want) = l.key.as_deref().and_then(expected_key) else { continue };
        if want == Expect::NoKey {
            continue;
        }
        let mut sources: Vec<(String, Vec<BarSpan>, Vec<super::Beat>, Source)> = Vec::new();
        if l.render.is_some() {
            let bars: Vec<BarSpan> = (0..l.bars as i64)
                .map(|k| BarSpan { bar: k + 1, start_beat: k as f64 * l.beats_per_bar(), beats: l.beats_per_bar() })
                .collect();
            let beats = midi::beats(&notes(l), 0.0, l.bars as f64 * l.beats_per_bar());
            sources.push(("rendered midi".into(), bars, beats, Source::Midi));
        }
        if let Some(rec) = latest.get(&l.id) {
            let group = match (l.render.is_some(), l.key_source.unwrap_or(KeySource::Loop)) {
                (true, _) => "rendered audio",
                (false, KeySource::Loop) => "pack, own key",
                (false, KeySource::Kit) => "pack, kit key",
            };
            sources.push((group.into(), rec.bars.clone(), audio::beats(&rec.beats).0, Source::Audio));
        }
        for (group, bars, beats, source) in &sources {
            let analysis = analyze(&Section { bars, beats, source: *source, scale: None });
            let entry = tally.entry((group.clone(), l.split)).or_insert((vec![0; weights.len()], 0));
            entry.1 += 1;
            if analysis.summary.get("key").is_none_or(Value::is_null) {
                continue; // no key at any weight
            }
            let profile = super::key_profile(beats.iter(), bars);
            let hint = super::downbeat_bass(beats.iter(), bars);
            let chords: Vec<Chord> = beats
                .iter()
                .filter(|b| b.pitched && !b.bass_only)
                .filter_map(|b| analysis.chords.get(&b.index).copied().flatten())
                .collect();
            for (k, &w) in weights.iter().enumerate() {
                let Some(ranked) = super::key::ranking_weighted(&profile, hint.as_ref(), &chords, w) else { continue };
                let got = ranked[0].0;
                let right = match want {
                    Expect::Key(key) => got == key,
                    Expect::Tonic(t) => got.tonic == t,
                    Expect::NoKey => unreachable!(),
                };
                entry.0[k] += right as usize;
            }
        }
    }
    let header: Vec<String> = weights.iter().map(|w| format!("{w:>5.1}")).collect();
    println!("{:28} {:>4} {}", "group (split)", "of", header.join(""));
    for ((group, split), (right, of)) in &tally {
        let cells: Vec<String> = right.iter().map(|r| format!("{r:>5}")).collect();
        println!("{:28} {of:>4} {}", format!("{group} ({split:?})").to_lowercase(), cells.join(""));
    }
}

/// The bass line of the rendered loops, 16th by 16th: what the plugin's tracker reported and what
/// the server kept, against the notes played. Interval classes: exact, octave (±12, ±24), fifth
/// (±7, ±19: a note heard at 2/3 or 3/2 of its frequency), other, none.
#[test]
#[ignore = "needs recordings: HEARD=<run folder>[:<run folder>…]"]
fn bass_diagnostics() {
    let Ok(dirs) = std::env::var("HEARD") else { return };
    let set = load_set();
    let mut latest: BTreeMap<String, Recording> = BTreeMap::new();
    for dir in dirs.split(':') {
        let dir = Path::new(dir);
        let takes: BTreeMap<String, String> = std::fs::read_to_string(dir.join("takes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for rec in load(dir) {
            if let Some(id) = takes.get(&rec.id) {
                latest.insert(id.clone(), rec);
            }
        }
    }
    let class = |got: Option<u8>, want: u8| -> &'static str {
        match got.map(|g| g as i32 - want as i32) {
            None => "none",
            Some(0) => "exact",
            Some(12 | -12 | 24 | -24) => "octave",
            Some(7 | -7 | 19 | -19) => "fifth",
            Some(5 | -5 | 17 | -17) => "fourth",
            Some(_) => "other",
        }
    };
    let classes = ["exact", "octave", "fifth", "fourth", "other", "none"];
    let mut total_raw: BTreeMap<&str, usize> = BTreeMap::new();
    let mut total_kept: BTreeMap<&str, usize> = BTreeMap::new();
    // Why a raw note is missing: the tracker had none, or it was too unsure, or the beat too quiet.
    let mut missing: BTreeMap<&str, usize> = BTreeMap::new();
    // (raw − played, kept − played) in semitones → 16ths.
    let mut moves: BTreeMap<(String, String), usize> = BTreeMap::new();
    // 16ths with no bass note played, and those of them with a note kept anyway.
    let (mut silent, mut invented) = (0, 0);
    println!("{:26} {:>4}  raw: {}  |  kept: {}", "loop", "16ths", classes.join(" "), classes.join(" "));
    for l in set.iter().filter(|l| l.render.as_ref().is_some_and(|r| r.bass)) {
        let Some(rec) = latest.get(&l.id) else { continue };
        let truth: BTreeMap<i64, super::Beat> =
            midi::beats(&notes(l), 0.0, l.bars as f64 * l.beats_per_bar()).into_iter().map(|b| (b.index, b)).collect();
        let (kept, _) = audio::beats(&rec.beats);
        let kept: BTreeMap<i64, &super::Beat> = kept.iter().map(|b| (b.index, b)).collect();
        let mut raw_counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut kept_counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut n = 0;
        for r in &rec.beats {
            let Some(want) = truth.get(&r.beat) else { continue };
            for s in 0..4 {
                let Some(w) = want.bass_line[s] else {
                    // No bass note played here: a kept note is invented.
                    silent += 1;
                    invented += kept.get(&r.beat).is_some_and(|b| b.bass_line[s].is_some()) as usize;
                    continue;
                };
                n += 1;
                let raw = (r.bass_notes[s] > 0).then_some(r.bass_notes[s]);
                *raw_counts.entry(class(raw, w.pitch)).or_default() += 1;
                let got = kept.get(&r.beat).and_then(|b| b.bass_line[s]).map(|b| b.pitch);
                *kept_counts.entry(class(got, w.pitch)).or_default() += 1;
                let signed =
                    |g: Option<u8>| g.map_or("none".to_owned(), |g| format!("{:+}", g as i32 - w.pitch as i32));
                *moves.entry((signed(raw), signed(got))).or_default() += 1;
                if got.is_none() {
                    let why = if r.bass_notes[s] == 0 {
                        "tracker had no note"
                    } else if r.bass_confidence[s] < 64 {
                        "below confidence 64"
                    } else if r.level_db < -50.0 {
                        "beat below -50 dB"
                    } else {
                        "dropped by the server"
                    };
                    *missing.entry(why).or_default() += 1;
                }
            }
        }
        let cells = |c: &BTreeMap<&str, usize>| {
            classes.iter().map(|k| format!("{:>4}", c.get(k).copied().unwrap_or(0))).collect::<Vec<_>>().join(" ")
        };
        println!("{:26} {n:>4}  raw: {}  |  kept: {}", l.id, cells(&raw_counts), cells(&kept_counts));
        for (k, v) in raw_counts {
            *total_raw.entry(k).or_default() += v;
        }
        for (k, v) in kept_counts {
            *total_kept.entry(k).or_default() += v;
        }
    }
    let cells = |c: &BTreeMap<&str, usize>| {
        classes.iter().map(|k| format!("{:>4}", c.get(k).copied().unwrap_or(0))).collect::<Vec<_>>().join(" ")
    };
    println!("{:26} {:>4}  raw: {}  |  kept: {}", "ALL", "", cells(&total_raw), cells(&total_kept));
    println!("why kept notes are missing: {missing:?}");
    println!("16ths with no bass played: {silent}, with a note kept anyway: {invented}");
    let mut moves: Vec<_> = moves.into_iter().collect();
    moves.sort_by_key(|m| std::cmp::Reverse(m.1));
    println!("raw → kept (semitones from the note played), commonest first:");
    for ((raw, kept), n) in moves.iter().take(16) {
        println!("  {raw:>5} → {kept:>5}: {n}");
    }
}

/// One line per tracked bass 16th of a rendered loop, for working out octave and fifth corrections
/// offline: split, loop, raw − played (semitones), the raw note, its confidence, energy per octave
/// (share of the beat's strongest) and the bass chroma (share of its strongest), folded in tune.
#[test]
#[ignore = "writes BASS_DUMP=<file> from HEARD=<run folder>"]
fn bass_dump() {
    let (Ok(dirs), Ok(out)) = (std::env::var("HEARD"), std::env::var("BASS_DUMP")) else { return };
    let set = load_set();
    let mut latest: BTreeMap<String, Recording> = BTreeMap::new();
    for dir in dirs.split(':') {
        let dir = Path::new(dir);
        let takes: BTreeMap<String, String> = std::fs::read_to_string(dir.join("takes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for rec in load(dir) {
            if let Some(id) = takes.get(&rec.id) {
                latest.insert(id.clone(), rec);
            }
        }
    }
    let mut lines = Vec::new();
    for l in set.iter().filter(|l| l.render.as_ref().is_some_and(|r| r.bass)) {
        let Some(rec) = latest.get(&l.id) else { continue };
        let truth: BTreeMap<i64, super::Beat> =
            midi::beats(&notes(l), 0.0, l.bars as f64 * l.beats_per_bar()).into_iter().map(|b| (b.index, b)).collect();
        for r in &rec.beats {
            let Some(want) = truth.get(&r.beat) else { continue };
            let max_octave = r.octaves.iter().copied().max().unwrap_or(0).max(1) as f32;
            let bass: Vec<f32> = r.bass.iter().map(|bins| bins.iter().map(|&v| v as f32).sum()).collect();
            let max_bass = bass.iter().copied().fold(0.0, f32::max).max(1.0);
            for s in 0..4 {
                let (Some(w), true) = (want.bass_line[s], r.bass_notes[s] > 0) else { continue };
                let octaves: Vec<String> = r.octaves.iter().map(|&o| format!("{:.2}", o as f32 / max_octave)).collect();
                let chroma: Vec<String> = bass.iter().map(|&v| format!("{:.2}", v / max_bass)).collect();
                lines.push(format!(
                    "{:?}\t{}\t{}\t{}\t{}\t{}\t{}",
                    l.split,
                    l.id,
                    r.bass_notes[s] as i32 - w.pitch as i32,
                    r.bass_notes[s],
                    r.bass_confidence[s],
                    octaves.join(","),
                    chroma.join(",")
                ));
            }
        }
    }
    std::fs::write(out, lines.join("\n") + "\n").unwrap();
}

/// The bass notes the server keeps, per loop and 16th, for comparing two versions of the code on
/// loops without bass labels: KEPT_DUMP=<file>.
#[test]
#[ignore = "writes KEPT_DUMP=<file> from HEARD=<run folder>[:…]"]
fn bass_kept_dump() {
    let (Ok(dirs), Ok(out)) = (std::env::var("HEARD"), std::env::var("KEPT_DUMP")) else { return };
    let set = load_set();
    let mut latest: BTreeMap<String, Recording> = BTreeMap::new();
    for dir in dirs.split(':') {
        let dir = Path::new(dir);
        let takes: BTreeMap<String, String> = std::fs::read_to_string(dir.join("takes.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        for rec in load(dir) {
            if let Some(id) = takes.get(&rec.id) {
                latest.insert(id.clone(), rec);
            }
        }
    }
    let mut lines = Vec::new();
    for l in &set {
        let Some(rec) = latest.get(&l.id) else { continue };
        let beats = audio::beats(&rec.beats).0;
        let analysis = analyze(&Section { bars: &rec.bars, beats: &beats, source: Source::Audio, scale: None });
        for b in &beats {
            let tones: Vec<String> = analysis
                .chords
                .get(&b.index)
                .copied()
                .flatten()
                .map(|c| c.tones().map(|t| t.to_string()).collect())
                .unwrap_or_default();
            for (s, n) in b.bass_line.iter().enumerate() {
                let pitch = n.map_or(0, |n| n.pitch);
                lines.push(format!("{}\t{}\t{}\t{}\t{pitch}\t{}", l.category, l.id, b.index, s, tones.join(",")));
            }
        }
    }
    std::fs::write(out, lines.join("\n") + "\n").unwrap();
}
