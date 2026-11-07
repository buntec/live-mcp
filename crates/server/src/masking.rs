//! Which tracks share each frequency band: a first, band-level look at masking.

use live_mcp_analysis::{BAND_EDGES_HZ, BAND_NAMES, BarReport};
use serde_json::{Value, json};

use crate::live::{field, field_bool, field_str, items, obj_id};

/// A track whose share of a band is at least this (in dB of the band's total) carries it.
/// −6 dB is a quarter of the band's energy.
pub const CARRY_DB: f64 = -6.0;
/// Bands this far below the loudest band are too quiet to fight over.
const QUIET_BAND_DB: f64 = 30.0;
/// Shares below this are left out of the per-band lists.
const LIST_DB: f64 = -20.0;

/// One track as heard by its Ear, with the fader gain that comes after the Ear.
pub struct Heard<'a> {
    pub label: Value,
    pub name: String,
    /// Gain in dB between the Ear and the mix: the track's fader plus the faders of the groups
    /// it is in. `None` for −∞ (a fader all the way down).
    pub gain_db: Option<f64>,
    /// Why the track does not count, if it does not: muted, silenced by solo, ….
    pub left_out: Option<String>,
    pub bars: &'a [BarReport],
}

/// A regular or return track's switches, and the group track it sits in.
#[derive(Debug, Clone, Default)]
pub struct Strip {
    pub name: String,
    pub mute: bool,
    pub solo: bool,
    /// Index of the group track this track is in (groups are regular tracks).
    pub group: Option<usize>,
    pub is_return: bool,
}

/// How the regular tracks (first) and the return tracks (after them) reach the mix.
#[derive(Debug, Default)]
pub struct Mixer {
    pub strips: Vec<Strip>,
}

impl Mixer {
    /// From `get` of `live_set tracks` with fields name, mute, solo, group_track, and of
    /// `live_set return_tracks` with fields name, mute, solo.
    pub fn from_live(tracks: &Value, returns: &Value) -> Self {
        let tracks = items(tracks);
        let ids: Vec<Option<u64>> = tracks.iter().map(obj_id).collect();
        let strip = |t: &Value, is_return: bool| Strip {
            name: field_str(t, "name").to_owned(),
            mute: field_bool(t, "mute") == Some(true),
            solo: field_bool(t, "solo") == Some(true),
            group: field(t, "group_track")
                .and_then(obj_id)
                .and_then(|id| ids.iter().position(|other| *other == Some(id))),
            is_return,
        };
        let mut strips: Vec<Strip> = tracks.iter().map(|t| strip(t, false)).collect();
        strips.extend(items(returns).iter().map(|t| strip(t, true)));
        Self { strips }
    }

    /// The groups track `k` is in, innermost first.
    pub fn groups(&self, k: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = self.strips.get(k).and_then(|s| s.group);
        while let Some(g) = at {
            if g == k || out.contains(&g) || g >= self.strips.len() {
                break; // a malformed answer; Live's groups do not loop
            }
            out.push(g);
            at = self.strips[g].group;
        }
        out
    }

    /// Whether group track `g` contains track `k`, at any depth.
    pub fn contains(&self, g: usize, k: usize) -> bool {
        self.groups(k).contains(&g)
    }

    /// Why track `k` does not reach the mix, or `None` if it does. Assumes Live's default Solo
    /// in Place: soloing a regular track keeps the return tracks playing.
    pub fn silenced(&self, k: usize) -> Option<&'static str> {
        let strip = self.strips.get(k)?;
        let groups = self.groups(k);
        if strip.mute {
            return Some("muted");
        }
        if groups.iter().any(|&g| self.strips[g].mute) {
            return Some("its group is muted");
        }
        if !self.strips.iter().any(|s| s.solo) {
            return None;
        }
        let heard = if strip.is_return {
            strip.solo || !self.strips.iter().any(|s| s.is_return && s.solo)
        } else {
            strip.solo
                || groups.iter().any(|&g| self.strips[g].solo)
                || (0..self.strips.len()).any(|j| self.strips[j].solo && self.contains(k, j))
        };
        (!heard).then_some("another track is soloed")
    }
}

fn energy_mean(levels: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = levels.fold((0.0, 0usize), |(s, n), db| (s + 10f64.powf(db / 10.0), n + 1));
    (n > 0 && sum > 0.0).then(|| 10.0 * (sum / n as f64).log10())
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0 + 0.0
}

fn band_label(i: usize) -> String {
    let fmt = |hz: f64| if hz >= 1000.0 { format!("{} kHz", hz / 1000.0) } else { format!("{hz} Hz") };
    match i {
        0 => format!("low (<{})", fmt(BAND_EDGES_HZ[0])),
        4 => format!("high (>{})", fmt(BAND_EDGES_HZ[3])),
        _ => format!("{} ({}–{})", BAND_NAMES[i], fmt(BAND_EDGES_HZ[i - 1]), fmt(BAND_EDGES_HZ[i])),
    }
}

/// Per band: each audible track's share of the band (post-fader), and the bands that two or
/// more tracks carry.
pub fn analyze(tracks: &[Heard<'_>]) -> Value {
    // Post-fader band levels of the tracks that reach the mix.
    let audible: Vec<(&Heard<'_>, [f64; 5])> = tracks
        .iter()
        .filter(|t| t.left_out.is_none())
        .filter_map(|t| {
            let gain = t.gain_db?;
            let levels: Option<Vec<f64>> = (0..5)
                .map(|i| energy_mean(t.bars.iter().filter(|r| r.coverage >= 0.5).map(|r| r.stats.bands_dbfs[i] as f64)))
                .collect();
            let levels = levels?;
            Some((t, std::array::from_fn(|i| levels[i] + gain)))
        })
        .collect();
    let excluded: Vec<Value> = tracks
        .iter()
        .filter_map(|t| {
            let why = t.left_out.as_deref().or(t.gain_db.is_none().then_some("volume at −∞"))?;
            Some(json!({"track": t.label, "name": t.name, "why": why}))
        })
        .collect();

    let mix: [Option<f64>; 5] = std::array::from_fn(|i| {
        let total: f64 = audible.iter().map(|(_, l)| 10f64.powf(l[i] / 10.0)).sum();
        (total > 0.0).then(|| 10.0 * total.log10())
    });
    let loudest = mix.iter().flatten().copied().fold(f64::MIN, f64::max);

    let mut bands = Vec::new();
    let mut contested = Vec::new();
    for i in 0..5 {
        let Some(total) = mix[i] else { continue };
        let mut shares: Vec<(&Heard<'_>, f64)> = audible.iter().map(|(t, l)| (*t, l[i] - total)).collect();
        shares.sort_by(|a, b| b.1.total_cmp(&a.1));
        let listed: Vec<Value> = shares
            .iter()
            .filter(|(_, s)| *s >= LIST_DB)
            .map(|(t, s)| json!({"track": t.label, "name": t.name, "share_db": r1(*s)}))
            .collect();
        bands.push(json!({"band": BAND_NAMES[i], "mix_dbfs": r1(total), "tracks": listed}));
        let carriers: Vec<&(&Heard<'_>, f64)> = shares.iter().filter(|(_, s)| *s >= CARRY_DB).collect();
        if carriers.len() >= 2 && total >= loudest - QUIET_BAND_DB {
            let who: Vec<String> = carriers.iter().map(|(t, s)| format!("{} ({:.1} dB)", t.name, s)).collect();
            contested.push(format!("{}: {} share it", band_label(i), join_names(&who)));
        }
    }
    let mut out = json!({
        "bands": bands,
        "contested": contested,
        "method": format!(
            "share_db is each track's part of the band's total across these tracks (0 dB = all of it; −6 dB = a \
             quarter). A band is contested when two or more tracks carry at least {CARRY_DB} dB of it. Levels are \
             after each track's fader and the faders of its groups; muted tracks and tracks silenced by solo \
             are left out. Pan is ignored. Sends count through the return tracks that have an Ear."
        ),
    });
    if !excluded.is_empty() {
        out["excluded"] = json!(excluded);
    }
    out
}

fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use live_mcp_analysis::{BarAnalyzer, Transport};

    use super::*;

    /// Bars 1–2 of the sum of sines (Hz, dBFS), as the Ear would report them.
    fn bars(partials: &[(f64, f64)]) -> Vec<BarReport> {
        let sr = 48_000.0;
        let mut analyzer = BarAnalyzer::new(sr as f32);
        let samples: Vec<f32> = (0..3 * 96_000)
            .map(|i| {
                partials.iter().map(|(f, db)| 10f64.powf(db / 20.0) * (2.0 * PI * f * i as f64 / sr).sin()).sum::<f64>()
                    as f32
            })
            .collect();
        let mut out = Vec::new();
        let t = Transport { playing: true, beat: Some(-4.0), tempo: Some(120.0), time_signature: None };
        analyzer.process(&samples, &samples, &t, &mut |r| out.push(r));
        out.retain(|r| r.bar >= 1); // drop the settling bar
        out
    }

    fn heard<'a>(name: &str, gain_db: Option<f64>, bars: &'a [BarReport]) -> Heard<'a> {
        Heard { label: json!(name), name: name.into(), gain_db, left_out: None, bars }
    }

    #[test]
    fn bass_and_kick_share_the_low_band() {
        let bass = bars(&[(70.0, -12.0), (220.0, -24.0)]);
        let kick = bars(&[(55.0, -13.0), (3000.0, -30.0)]);
        let pad = bars(&[(800.0, -15.0), (2500.0, -22.0)]);
        let out =
            analyze(&[heard("Bass", Some(0.0), &bass), heard("Kick", Some(0.0), &kick), heard("Pad", Some(0.0), &pad)]);
        let contested: Vec<&str> = out["contested"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        assert_eq!(contested.len(), 1, "{contested:?}");
        assert!(contested[0].starts_with("low (<120 Hz): Bass") && contested[0].contains("and Kick"), "{contested:?}");
        let low = &out["bands"][0];
        assert_eq!(low["tracks"][0]["name"], "Bass");
        assert!(low["tracks"].as_array().unwrap().iter().all(|t| t["name"] != "Pad"), "the pad has no low end: {low}");
    }

    #[test]
    fn the_fader_counts_and_muted_tracks_do_not() {
        let bass = bars(&[(70.0, -12.0)]);
        let kick = bars(&[(55.0, -12.0)]);
        // The kick's fader is 20 dB down, so the bass carries the low band alone.
        let out = analyze(&[heard("Bass", Some(0.0), &bass), heard("Kick", Some(-20.0), &kick)]);
        assert_eq!(out["contested"], json!([]));
        let kick_share = out["bands"][0]["tracks"][1]["share_db"].as_f64().unwrap();
        // About −20 dB: the fader gain, give or take how much of each tone the 120 Hz crossover passes.
        assert!((kick_share + 20.0).abs() < 1.0, "{}", out["bands"][0]);
        // Muted, or at −∞: left out entirely.
        let mut muted = heard("Kick", Some(0.0), &kick);
        muted.left_out = Some("muted".into());
        let out = analyze(&[heard("Bass", Some(0.0), &bass), muted, heard("Sub", None, &kick)]);
        assert_eq!(
            out["excluded"],
            json!([
                {"track": "Kick", "name": "Kick", "why": "muted"},
                {"track": "Sub", "name": "Sub", "why": "volume at −∞"},
            ])
        );
        assert_eq!(out["bands"][0]["tracks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn quiet_bands_are_not_contested() {
        // Both tracks have a little high end, 50 dB below the main band: not worth reporting.
        let a = bars(&[(1000.0, -10.0), (9000.0, -60.0)]);
        let b = bars(&[(9500.0, -60.0)]);
        let out = analyze(&[heard("A", Some(0.0), &a), heard("B", Some(0.0), &b)]);
        assert_eq!(out["contested"], json!([]), "{out}");
    }

    /// Tracks as `get live_set tracks` returns them: ids, and each group_track as an object.
    fn live_tracks(tracks: &[(&str, bool, bool, Option<usize>)]) -> Value {
        let id = |i: usize| 1000 + i as u64;
        Value::Array(
            tracks
                .iter()
                .enumerate()
                .map(|(i, (name, mute, solo, group))| {
                    json!({"$obj": id(i), "type": "Track", "fields": {
                        "name": name, "mute": mute, "solo": solo,
                        "group_track": group.map(|g| json!({"$obj": id(g), "type": "Track"})),
                    }})
                })
                .collect(),
        )
    }

    fn returns(list: &[(&str, bool, bool)]) -> Value {
        Value::Array(
            list.iter()
                .enumerate()
                .map(|(i, (name, mute, solo))| {
                    json!({"$obj": 2000 + i, "type": "Track", "fields": {"name": name, "mute": mute, "solo": solo}})
                })
                .collect(),
        )
    }

    #[test]
    fn groups_are_read_from_live_objects() {
        // Drums [Kick, Perc [Shaker]], Bass
        let mixer = Mixer::from_live(
            &live_tracks(&[
                ("Drums", false, false, None),
                ("Kick", false, false, Some(0)),
                ("Perc", false, false, Some(0)),
                ("Shaker", false, false, Some(2)),
                ("Bass", false, false, None),
            ]),
            &returns(&[("A-Reverb", false, false)]),
        );
        assert_eq!(mixer.strips.len(), 6);
        assert_eq!(mixer.groups(3), [2, 0]);
        assert_eq!(mixer.groups(1), [0]);
        assert!(mixer.groups(4).is_empty() && mixer.groups(5).is_empty());
        assert!(mixer.contains(0, 3) && !mixer.contains(2, 1));
        assert!(mixer.strips[5].is_return && mixer.strips[5].name == "A-Reverb");
        assert!((0..6).all(|k| mixer.silenced(k).is_none()));
    }

    #[test]
    fn a_muted_group_silences_its_tracks() {
        let mixer = Mixer::from_live(
            &live_tracks(&[
                ("Drums", true, false, None),
                ("Kick", false, false, Some(0)),
                ("Perc", false, false, Some(0)),
                ("Shaker", false, false, Some(2)),
                ("Bass", false, false, None),
            ]),
            &Value::Null,
        );
        assert_eq!(mixer.silenced(0), Some("muted"));
        assert_eq!(mixer.silenced(1), Some("its group is muted"));
        assert_eq!(mixer.silenced(3), Some("its group is muted"));
        assert_eq!(mixer.silenced(4), None);
    }

    #[test]
    fn solo_silences_what_is_not_soloed() {
        let set = |solo: [bool; 5], return_solo: bool| {
            Mixer::from_live(
                &live_tracks(&[
                    ("Drums", false, solo[0], None),
                    ("Kick", false, solo[1], Some(0)),
                    ("Perc", false, solo[2], Some(0)),
                    ("Shaker", false, solo[3], Some(2)),
                    ("Bass", false, solo[4], None),
                ]),
                &returns(&[("A", false, return_solo), ("B", false, false)]),
            )
        };
        let heard = |m: &Mixer| (0..m.strips.len()).filter(|&k| m.silenced(k).is_none()).collect::<Vec<_>>();
        // Soloing a track inside a group: the group plays, its other tracks don't. Returns play
        // on (Solo in Place).
        assert_eq!(heard(&set([false, false, false, true, false], false)), [0, 2, 3, 5, 6]);
        // Soloing a group: everything in it plays.
        assert_eq!(heard(&set([true, false, false, false, false], false)), [0, 1, 2, 3, 5, 6]);
        assert_eq!(set([true, false, false, false, false], false).silenced(4), Some("another track is soloed"));
        // Soloing a return: only that return.
        assert_eq!(heard(&set([false; 5], true)), [5]);
        // A track and a return.
        assert_eq!(heard(&set([false, false, false, false, true], true)), [4, 5]);
    }
}
