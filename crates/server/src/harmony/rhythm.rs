//! Rhythm: a 16th-note onset grid per bar, density, syncopation and repeating patterns.

use std::collections::HashMap;

use super::{BarSpan, Beat};

/// Slot strengths relative to the section's 90th percentile: at least STRONG is `X`, at least
/// WEAK is `x`.
const STRONG: f32 = 0.6;
const WEAK: f32 = 0.25;
/// Bars whose onset strengths correlate at least this much share a pattern letter.
const SAME_PATTERN: f32 = 0.92;

#[derive(Debug, Clone, PartialEq)]
pub struct BarRhythm {
    /// One character per 16th, `X` strong, `x` weaker, `.` none, `-` not heard; a space between
    /// beats.
    pub grid: String,
    /// Onsets per beat.
    pub density: f32,
    /// 0 = every onset on a stronger position than the silence after it, 1 = maximally off.
    pub syncopation: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// One letter per bar, the same letter for the same pattern ("A A B A"); `-` for unheard bars.
    pub pattern: Option<String>,
    pub density: f32,
    pub syncopation: f32,
}

/// Metrical weight of a 16th slot (Longuet-Higgins & Lee): 0 on the downbeat, −1 on the half
/// bar of an even bar, −2 on other beats, −3 on the 8th between beats, −4 on other 16ths.
fn metric_weight(slot: usize, beats: f64) -> i32 {
    let (beat, sub) = (slot / 4, slot % 4);
    match sub {
        0 if beat == 0 => 0,
        0 if beats >= 4.0 && beats % 2.0 == 0.0 && beat as f64 * 2.0 == beats => -1,
        0 => -2,
        2 => -3,
        _ => -4,
    }
}

/// The onset strength `X` and `x` are measured against: the section's 90th percentile.
pub fn reference(beats: &[Beat]) -> f32 {
    let mut positive: Vec<f32> = beats.iter().flat_map(|b| b.onsets).filter(|v| *v > 0.0).collect();
    positive.sort_by(f32::total_cmp);
    positive.get(positive.len() * 9 / 10).or(positive.last()).copied().unwrap_or(0.0)
}

/// Whether an onset is strong (`X`) relative to the section's reference.
pub fn is_strong(onset: f32, reference: f32) -> bool {
    reference > 0.0 && onset / reference >= STRONG
}

pub fn analyze(beats: &[Beat], bars: &[BarSpan]) -> (Vec<Option<BarRhythm>>, Summary) {
    let mut strength: HashMap<i64, f32> = HashMap::new();
    let mut heard = std::collections::HashSet::new();
    for beat in beats {
        heard.insert(beat.index);
        for (sub, value) in beat.onsets.iter().enumerate() {
            strength.insert(beat.index * 4 + sub as i64, *value);
        }
    }
    let reference = reference(beats);

    let mut per_bar = Vec::with_capacity(bars.len());
    let mut vectors: Vec<Option<Vec<f32>>> = Vec::with_capacity(bars.len());
    for bar in bars {
        let first = (bar.start_beat * 4.0).round() as i64;
        let slots = (bar.beats * 4.0).round().max(1.0) as usize;
        let mut grid = String::with_capacity(slots + slots / 4);
        let mut levels = Vec::with_capacity(slots);
        let mut complete = true;
        let mut any = false;
        for j in 0..slots {
            if j > 0 && j % 4 == 0 {
                grid.push(' ');
            }
            let slot = first + j as i64;
            if !heard.contains(&slot.div_euclid(4)) {
                grid.push('-');
                levels.push(0.0);
                complete = false;
                continue;
            }
            any = true;
            let v = strength.get(&slot).copied().unwrap_or(0.0);
            let relative = if reference > 0.0 { v / reference } else { 0.0 };
            grid.push(if relative >= STRONG {
                'X'
            } else if relative >= WEAK {
                'x'
            } else {
                '.'
            });
            levels.push(if relative >= WEAK { v } else { 0.0 });
        }
        if !any {
            per_bar.push(None);
            vectors.push(None);
            continue;
        }
        let onsets: Vec<usize> = (0..slots).filter(|&j| levels[j] > 0.0).collect();
        let mut syncopation = 0.0;
        for (n, &i) in onsets.iter().enumerate() {
            let next = onsets.get(n + 1).copied().unwrap_or(onsets[0] + slots);
            let w = metric_weight(i, bar.beats);
            let stronger = (i + 1..next).map(|k| metric_weight(k % slots, bar.beats)).filter(|&wk| wk > w).max();
            if let Some(wk) = stronger {
                syncopation += (wk - w) as f32;
            }
        }
        let syncopation = if onsets.is_empty() { 0.0 } else { syncopation / (4.0 * onsets.len() as f32) };
        per_bar.push(Some(BarRhythm { grid, density: onsets.len() as f32 / bar.beats as f32, syncopation }));
        vectors.push(complete.then_some(levels));
    }

    let heard_bars: Vec<&BarRhythm> = per_bar.iter().flatten().collect();
    let mean = |f: fn(&BarRhythm) -> f32| {
        if heard_bars.is_empty() { 0.0 } else { heard_bars.iter().map(|b| f(b)).sum::<f32>() / heard_bars.len() as f32 }
    };
    let summary = Summary {
        pattern: (bars.len() >= 2).then(|| pattern_letters(&vectors)),
        density: mean(|b| b.density),
        syncopation: mean(|b| b.syncopation),
    };
    (per_bar, summary)
}

fn similar(a: &[f32], b: &[f32]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let (na, nb) = (a.iter().map(|x| x * x).sum::<f32>(), b.iter().map(|x| x * x).sum::<f32>());
    if na == 0.0 || nb == 0.0 {
        return na == nb;
    }
    dot / (na * nb).sqrt() >= SAME_PATTERN
}

fn pattern_letters(vectors: &[Option<Vec<f32>>]) -> String {
    let mut representatives: Vec<&[f32]> = Vec::new();
    let letters: Vec<String> = vectors
        .iter()
        .map(|v| match v {
            None => "-".to_owned(),
            Some(v) => {
                let index = representatives.iter().position(|r| similar(r, v)).unwrap_or_else(|| {
                    representatives.push(v);
                    representatives.len() - 1
                });
                char::from(b'A' + (index % 26) as u8).to_string()
            }
        })
        .collect();
    letters.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars(n: i64) -> Vec<BarSpan> {
        (0..n).map(|i| BarSpan { bar: i + 1, start_beat: i as f64 * 4.0, beats: 4.0 }).collect()
    }

    fn beats_from(grid: &[&str]) -> Vec<Beat> {
        let mut out = Vec::new();
        for (b, bar) in grid.iter().enumerate() {
            let chars: Vec<char> = bar.chars().filter(|c| !c.is_whitespace()).collect();
            for beat in 0..4 {
                let mut onsets = [0.0; 4];
                for (s, onset) in onsets.iter_mut().enumerate() {
                    *onset = match chars[beat * 4 + s] {
                        'X' => 1.0,
                        'x' => 0.4,
                        _ => 0.0,
                    };
                }
                out.push(Beat { index: (b * 4 + beat) as i64, onsets, ..Beat::default() });
            }
        }
        out
    }

    #[test]
    fn grids_patterns_and_density() {
        let four = "X... X... X... X...";
        let off = "..X. ..X. ..X. ..X.";
        let (per_bar, summary) = analyze(&beats_from(&[four, four, off, four]), &bars(4));
        assert_eq!(per_bar[0].as_ref().unwrap().grid, four);
        assert_eq!(per_bar[0].as_ref().unwrap().density, 1.0);
        assert_eq!(summary.pattern.as_deref(), Some("A A B A"));
        assert_eq!(per_bar[0].as_ref().unwrap().syncopation, 0.0);
        assert!(per_bar[2].as_ref().unwrap().syncopation > 0.0);
    }

    #[test]
    fn unheard_beats_show_as_dashes() {
        let mut beats = beats_from(&["X... X... X... X..."]);
        beats.truncate(2);
        let (per_bar, summary) = analyze(&beats, &bars(2));
        assert_eq!(per_bar[0].as_ref().unwrap().grid, "X... X... ---- ----");
        assert_eq!(per_bar[1], None);
        assert_eq!(summary.pattern.as_deref(), Some("- -"));
    }
}
