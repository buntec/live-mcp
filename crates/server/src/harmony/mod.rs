//! Musical interpretation of per-beat pitch and onset features: key, chords, bass, pitch classes,
//! rhythm, register and fit with the set's scale. Pure functions over [`Beat`]s, so `read_clip`
//! (from notes) and the Ear (from audio) share them.

pub mod chords;
pub mod key;
pub mod midi;
pub mod names;
pub mod rhythm;

use serde_json::{Map, Value, json};

use self::key::{Key, KeyEstimate};
use self::names::{Spelling, note_name, spelling};
use crate::ear::round2;

/// Price of a chord change, by where in the bar it happens.
const CHANGE_ON_DOWNBEAT: f32 = 0.06;
const CHANGE_ON_HALF_BAR: f32 = 0.1;
const CHANGE_ELSEWHERE: f32 = 0.16;
/// Pitch classes at least this share of the bar's strongest are listed.
const PCS_MIN: f32 = 0.25;
const PCS_MAX_LISTED: usize = 6;
/// Octaves with at least this share of the bar's strongest octave count towards its range.
const OCTAVE_MIN: f32 = 0.1;
/// Out-of-scale pitch classes at least this share of the section's strongest are reported.
const OUTSIDE_SCALE_MIN: f32 = 0.15;
/// Sections at least this many bars long are checked for key changes, in windows this wide.
const KEY_CHANGE_MIN_BARS: usize = 12;
const KEY_WINDOW_BARS: usize = 8;
/// A local key must hold for this many bars to count as a key change.
const KEY_CHANGE_HOLD_BARS: usize = 4;

/// What was heard (or written) during one beat (quarter note).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Beat {
    /// Beat on the timeline the bars use.
    pub index: i64,
    /// Share of the beat that was heard.
    pub coverage: f32,
    /// Strength of the beat's content, comparable between beats of one source.
    pub weight: f32,
    /// Whether the beat has pitch content to name.
    pub pitched: bool,
    /// Pitch-class energy, C first. Only the shape matters.
    pub chroma: [f32; 12],
    /// Pitch-class energy of the lowest part.
    pub bass: [f32; 12],
    /// Energy per octave in Live's naming, octaves 0–6 (C0 = MIDI 24 … B6 = MIDI 107).
    pub octaves: [f32; 7],
    /// Onset strength per 16th.
    pub onsets: [f32; 4],
    /// Exact lowest and highest sounding pitch, when known (MIDI).
    pub span: Option<(u8, u8)>,
    /// 0 = noise, 1 = clearly pitched; audio only.
    pub tonal: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarSpan {
    pub bar: i64,
    pub start_beat: f64,
    pub beats: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))] // `Audio` comes with the Ear's beat reports
pub enum Source {
    Audio,
    Midi,
}

/// The set's scale (Live 12), to check what is heard against it.
#[derive(Debug, Clone, PartialEq)]
pub struct SetScale {
    pub root: u8,
    pub name: String,
    pub intervals: Vec<u8>,
}

impl SetScale {
    /// From live_set's `root_note`, `scale_name` and `scale_intervals`; `None` if any is missing.
    pub fn from_song(root: Option<&Value>, name: Option<&Value>, intervals: Option<&Value>) -> Option<Self> {
        let root = root?.as_u64()?;
        let name = name?.as_str()?.to_owned();
        let intervals: Vec<u8> = intervals?.as_array()?.iter().filter_map(|v| v.as_u64()).map(|v| v as u8).collect();
        (!intervals.is_empty()).then_some(SetScale { root: (root % 12) as u8, name, intervals })
    }

    fn contains(&self, pc: u8) -> bool {
        self.intervals.iter().any(|i| (self.root + i) % 12 == pc)
    }
}

pub struct Section<'a> {
    pub bars: &'a [BarSpan],
    /// Sorted by index.
    pub beats: &'a [Beat],
    pub source: Source,
    pub scale: Option<&'a SetScale>,
}

pub struct Analysis {
    /// Section-wide: key, key changes, progression, rhythm, scale fit.
    pub summary: Map<String, Value>,
    /// Per heard bar: chord(s), bass, pitch classes, rhythm grid, range, tonalness.
    pub bars: Vec<(i64, Map<String, Value>)>,
}

fn shape(values: &[f32; 12]) -> [f32; 12] {
    let max = values.iter().copied().fold(0.0, f32::max);
    if max > 0.0 { values.map(|v| v / max) } else { [0.0; 12] }
}

/// Weighted pitch-class profile of some beats, bass counted at half weight.
fn profile<'a>(beats: impl Iterator<Item = &'a Beat>, with_bass: bool) -> [f32; 12] {
    let mut sum = [0.0f32; 12];
    for beat in beats.filter(|b| b.pitched) {
        let (chroma, bass) = (shape(&beat.chroma), shape(&beat.bass));
        for pc in 0..12 {
            sum[pc] += beat.weight * (chroma[pc] + if with_bass { 0.5 * bass[pc] } else { 0.0 });
        }
    }
    sum
}

/// Positions in `beats` that fall in each bar.
fn bar_ranges(beats: &[Beat], bars: &[BarSpan]) -> Vec<std::ops::Range<usize>> {
    bars.iter()
        .map(|bar| {
            let from = beats.partition_point(|b| (b.index as f64) < bar.start_beat - 1e-9);
            let to = beats.partition_point(|b| (b.index as f64) < bar.start_beat + bar.beats - 1e-9);
            from..to.max(from)
        })
        .collect()
}

fn key_json(estimate: &KeyEstimate) -> Value {
    let mut out = json!({"name": estimate.key.name(), "confidence": round2(estimate.confidence)});
    if let Some(alt) = estimate.alt {
        out["alt"] = json!(alt.name());
    }
    out
}

fn key_changes(section: &Section, ranges: &[std::ops::Range<usize>]) -> Vec<(i64, Key)> {
    let n = section.bars.len();
    if n < KEY_CHANGE_MIN_BARS {
        return Vec::new();
    }
    let local: Vec<Option<Key>> = (0..n)
        .map(|i| {
            let from = i.saturating_sub(KEY_WINDOW_BARS / 2 - 1);
            let to = (i + KEY_WINDOW_BARS / 2 + 1).min(n);
            let beats = ranges[from..to].iter().flat_map(|r| &section.beats[r.clone()]);
            key::estimate(&profile(beats, true)).map(|e| e.key)
        })
        .collect();
    let mut changes: Vec<(i64, Key)> = Vec::new();
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && local[j] == local[i] {
            j += 1;
        }
        if let Some(key) = local[i]
            && j - i >= KEY_CHANGE_HOLD_BARS
            && changes.last().is_none_or(|(_, last)| *last != key)
        {
            changes.push((section.bars[i].bar, key));
        }
        i = j;
    }
    if changes.len() > 1 { changes } else { Vec::new() }
}

fn pcs_text(profile: &[f32; 12], spelling: Spelling) -> Option<String> {
    let profile = shape(profile);
    let mut listed: Vec<(usize, f32)> = (0..12).map(|pc| (pc, profile[pc])).filter(|(_, w)| *w >= PCS_MIN).collect();
    if listed.is_empty() {
        return None;
    }
    listed.sort_by(|a, b| b.1.total_cmp(&a.1));
    listed.truncate(PCS_MAX_LISTED);
    Some(listed.iter().map(|(pc, w)| format!("{} {:.1}", spelling[*pc], w)).collect::<Vec<_>>().join(" "))
}

fn range_text(beats: &[Beat], spelling: Spelling) -> Option<String> {
    let spans: Vec<(u8, u8)> = beats.iter().filter_map(|b| b.span).collect();
    if !spans.is_empty() {
        let low = spans.iter().map(|s| s.0).min()?;
        let high = spans.iter().map(|s| s.1).max()?;
        if low == high {
            return Some(note_name(low, spelling));
        }
        return Some(format!("{}–{}", note_name(low, spelling), note_name(high, spelling)));
    }
    let mut octaves = [0.0f32; 7];
    for beat in beats {
        for (sum, o) in octaves.iter_mut().zip(&beat.octaves) {
            *sum += o * beat.weight.max(0.0);
        }
    }
    let max = octaves.iter().copied().fold(0.0, f32::max);
    if max <= 0.0 {
        return None;
    }
    let low = octaves.iter().position(|o| *o >= OCTAVE_MIN * max)?;
    let high = octaves.iter().rposition(|o| *o >= OCTAVE_MIN * max)?;
    Some(format!("C{low}–B{high}"))
}

/// Interprets a section. Without any pitched beat (drums, silence) only rhythm is reported.
pub fn analyze(section: &Section) -> Analysis {
    let beats = section.beats;
    let pitched = beats.iter().any(|b| b.pitched);
    let ranges = bar_ranges(beats, section.bars);
    let estimate = key::estimate(&profile(beats.iter(), true));
    let key = estimate.map(|e| e.key);
    let spelling = spelling(key);

    let mut change_cost = vec![CHANGE_ELSEWHERE; beats.len()];
    for (bar, range) in section.bars.iter().zip(&ranges) {
        for t in range.clone() {
            let position = beats[t].index as f64 - bar.start_beat;
            change_cost[t] = if position.abs() < 1e-9 {
                CHANGE_ON_DOWNBEAT
            } else if bar.beats >= 4.0 && (position * 2.0 - bar.beats).abs() < 1e-9 {
                CHANGE_ON_HALF_BAR
            } else {
                CHANGE_ELSEWHERE
            };
        }
    }
    let labels = chords::label_beats(beats, &change_cost, key, section.source);
    let (rhythm_bars, rhythm) = rhythm::analyze(beats, section.bars);

    let mut bars = Vec::new();
    let mut progression = Vec::new();
    for ((bar, range), bar_rhythm) in section.bars.iter().zip(&ranges).zip(&rhythm_bars) {
        let bar_beats = &beats[range.clone()];
        if bar_beats.is_empty() {
            progression.push("-".to_owned());
            continue;
        }
        let mut out = Map::new();
        let segments = if pitched { chords::segments(beats, &labels, range.clone()) } else { Vec::new() };
        progression.push(segments.iter().map(|s| chords::label(s.chord, spelling)).collect::<Vec<_>>().join(" "));
        let chord_json = |s: &chords::Segment, out: &mut Map<String, Value>| {
            out.insert("chord".into(), json!(chords::label(s.chord, spelling)));
            if s.chord.is_some() {
                out.insert("confidence".into(), json!(round2(s.confidence)));
                if !s.alts.is_empty() {
                    let alts: Vec<String> = s.alts.iter().map(|c| c.name(spelling)).collect();
                    out.insert("alt".into(), json!(alts));
                }
            }
        };
        if let [only] = segments.as_slice() {
            chord_json(only, &mut out);
        } else if !segments.is_empty() {
            let list: Vec<Value> = segments
                .iter()
                .map(|s| {
                    let mut entry = Map::new();
                    let position = beats[s.first].index as f64 - bar.start_beat + 1.0;
                    entry.insert("beat".into(), json!(position));
                    chord_json(s, &mut entry);
                    Value::Object(entry)
                })
                .collect();
            out.insert("chords".into(), json!(list));
        }
        let mut bass = [0.0f32; 12];
        for beat in bar_beats.iter().filter(|b| b.pitched) {
            for (sum, b) in bass.iter_mut().zip(shape(&beat.bass)) {
                *sum += b * beat.weight;
            }
        }
        if let Some((pc, _)) = bass.iter().enumerate().filter(|(_, w)| **w > 0.0).max_by(|a, b| a.1.total_cmp(b.1)) {
            out.insert("bass".into(), json!(spelling[pc]));
        }
        if let Some(pcs) = pcs_text(&profile(bar_beats.iter(), false), spelling) {
            out.insert("pcs".into(), json!(pcs));
        }
        if let Some(r) = bar_rhythm {
            out.insert("rhythm".into(), json!(r.grid));
        }
        if let Some(range) = range_text(bar_beats, spelling) {
            out.insert("range".into(), json!(range));
        }
        let tonal: Vec<f32> = bar_beats.iter().filter_map(|b| b.tonal).collect();
        if !tonal.is_empty() {
            out.insert("tonal".into(), json!(round2(tonal.iter().sum::<f32>() / tonal.len() as f32)));
        }
        bars.push((bar.bar, out));
    }

    let mut summary = Map::new();
    if pitched {
        summary.insert("key".into(), estimate.as_ref().map_or(Value::Null, key_json));
        let changes = key_changes(section, &ranges);
        if !changes.is_empty() {
            let list: Vec<Value> = changes.iter().map(|(bar, key)| json!({"bar": bar, "key": key.name()})).collect();
            summary.insert("key_changes".into(), json!(list));
        }
        summary.insert("progression".into(), json!(progression.join(" | ")));
    }
    let mut rhythm_json = json!({"density": round2(rhythm.density), "syncopation": round2(rhythm.syncopation)});
    if let Some(pattern) = rhythm.pattern {
        rhythm_json["pattern"] = json!(pattern);
    }
    summary.insert("rhythm".into(), rhythm_json);
    if let Some(scale) = section.scale.filter(|_| pitched) {
        summary.insert("scale".into(), scale_fit(section, scale, &ranges, spelling));
    }
    Analysis { summary, bars }
}

fn scale_fit(section: &Section, scale: &SetScale, ranges: &[std::ops::Range<usize>], spelling: Spelling) -> Value {
    let whole = shape(&profile(section.beats.iter(), false));
    let bar_profiles: Vec<[f32; 12]> =
        ranges.iter().map(|r| shape(&profile(section.beats[r.clone()].iter(), false))).collect();
    let outside: Vec<Value> = (0..12u8)
        .filter(|&pc| !scale.contains(pc) && whole[pc as usize] >= OUTSIDE_SCALE_MIN)
        .map(|pc| {
            let bars: Vec<i64> = section
                .bars
                .iter()
                .zip(&bar_profiles)
                .filter(|(_, p)| p[pc as usize] >= PCS_MIN)
                .map(|(bar, _)| bar.bar)
                .collect();
            json!({"pc": spelling[pc as usize], "weight": round2(whole[pc as usize]), "bars": bars})
        })
        .collect();
    json!({"set": format!("{} {}", spelling[scale.root as usize], scale.name), "outside": outside})
}

#[cfg(test)]
mod tests {
    use super::midi::{MidiNote, beats};
    use super::*;

    fn chord(pitches: &[u8], start: f64, duration: f64) -> Vec<MidiNote> {
        pitches.iter().map(|&pitch| MidiNote { pitch, start, duration, velocity: 100.0 }).collect()
    }

    fn bars(n: i64) -> Vec<BarSpan> {
        (0..n).map(|i| BarSpan { bar: i + 1, start_beat: i as f64 * 4.0, beats: 4.0 }).collect()
    }

    #[test]
    fn a_progression_in_a_minor() {
        // Am | F | C G | E  — whole-bar chords, then two chords in bar 3, then E major (V).
        let mut notes = Vec::new();
        notes.extend(chord(&[45, 57, 60, 64], 0.0, 4.0));
        notes.extend(chord(&[41, 57, 60, 65], 4.0, 4.0));
        notes.extend(chord(&[48, 55, 60, 64], 8.0, 2.0));
        notes.extend(chord(&[43, 55, 59, 62], 10.0, 2.0));
        notes.extend(chord(&[40, 56, 59, 64], 12.0, 4.0));
        let beats = beats(&notes, 0.0, 16.0);
        let scale = SetScale { root: 0, name: "Major".into(), intervals: vec![0, 2, 4, 5, 7, 9, 11] };
        let bars = bars(4);
        let analysis = analyze(&Section { bars: &bars, beats: &beats, source: Source::Midi, scale: Some(&scale) });
        let s = &analysis.summary;
        assert_eq!(s["key"]["name"], "A minor", "{s:?}");
        assert_eq!(s["progression"], "Am | F | C G | E", "{s:?}");
        assert_eq!(s["scale"]["outside"][0]["pc"], "G#");
        assert_eq!(s["scale"]["outside"][0]["bars"], json!([4]));
        let (bar3, fields) = &analysis.bars[2];
        assert_eq!(*bar3, 3);
        assert_eq!(fields["chords"][1]["beat"], 3.0);
        assert_eq!(fields["chords"][1]["chord"], "G");
        assert_eq!(analysis.bars[0].1["bass"], "A");
        assert_eq!(analysis.bars[0].1["range"], "A1–E3");
        assert_eq!(analysis.bars[0].1["rhythm"], "X... .... .... ....");
    }

    #[test]
    fn key_changes_in_long_sections() {
        // 8 bars of C major (C, F, G, C …), then 8 bars of E major (E, A, B, E …).
        let mut notes = Vec::new();
        let c = [[48, 52, 55], [53, 57, 60], [55, 59, 62], [48, 52, 55]];
        let e = [[52, 56, 59], [57, 61, 64], [59, 63, 66], [52, 56, 59]];
        for bar in 0..16 {
            let triad = if bar < 8 { c[bar % 4] } else { e[bar % 4] };
            notes.extend(chord(&triad, bar as f64 * 4.0, 4.0));
        }
        let beats = beats(&notes, 0.0, 64.0);
        let bars = bars(16);
        let analysis = analyze(&Section { bars: &bars, beats: &beats, source: Source::Midi, scale: None });
        let changes = &analysis.summary["key_changes"];
        assert_eq!(changes[0]["key"], "C major", "{changes:?}");
        assert_eq!(changes[1]["key"], "E major", "{changes:?}");
    }
}
