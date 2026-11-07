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
    /// Gain in dB between the Ear and the mix: the track's fader plus the faders of the tracks
    /// its audio passes through (its groups, or a track it is routed to). `None` for −∞ (a
    /// fader all the way down).
    pub gain_db: Option<f64>,
    /// Why the track does not count, if it does not: muted, silenced by solo, ….
    pub left_out: Option<String>,
    pub bars: &'a [BarReport],
}

/// Where a track's output goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Into another regular or return track (its group, or a bus), by strip index.
    Strip(usize),
    /// Straight to Main.
    Main,
    /// Not into the mix: Ext. Out, Sends Only, No Output, …; Live's name for it.
    Elsewhere(String),
}

/// A regular or return track's switches, and where its audio goes.
#[derive(Debug, Clone)]
pub struct Strip {
    pub name: String,
    pub mute: bool,
    pub solo: bool,
    /// Live's own answer to whether another track's solo silences this one.
    pub muted_via_solo: bool,
    /// Index of the group track this track is in (groups are regular tracks).
    pub group: Option<usize>,
    pub is_group: bool,
    pub out: Out,
}

/// How the regular tracks (first) and the return tracks (after them) reach the mix.
#[derive(Debug, Default)]
pub struct Mixer {
    pub strips: Vec<Strip>,
}

/// Live's `RoutingTypeCategory`s that mean "Main" and "the group this track is in".
const ROUTE_MASTER: u64 = 3;
const ROUTE_PARENT_GROUP: u64 = 5;

impl Mixer {
    /// From `get` of `live_set tracks` with fields name, mute, solo, muted_via_solo,
    /// group_track and is_foldable, and of `live_set return_tracks` with the same fields; then
    /// for each of those tracks in turn its `output_routing_type`'s display_name, category and
    /// attached_object (`Null` where they could not be read), and the master track object.
    pub fn from_live(tracks: &Value, returns: &Value, routes: &[Value], master: &Value) -> Self {
        let all: Vec<&Value> = items(tracks).iter().chain(items(returns)).collect();
        let ids: Vec<Option<u64>> = all.iter().map(|t| obj_id(t)).collect();
        let index_of = |v: Option<&Value>| v.and_then(obj_id).and_then(|id| ids.iter().position(|o| *o == Some(id)));
        let master = obj_id(master);
        let strips = all
            .iter()
            .enumerate()
            .map(|(k, t)| {
                let group = index_of(field(t, "group_track")).filter(|&g| g != k);
                let route = routes.get(k).filter(|r| r.is_object());
                let out = match route {
                    // Unknown: assume the default, through the group if there is one.
                    None => group.map_or(Out::Main, Out::Strip),
                    Some(r) => {
                        let attached = r.get("attached_object").filter(|v| !v.is_null());
                        let category = r.get("category").and_then(Value::as_u64);
                        if attached.and_then(obj_id).is_some_and(|id| Some(id) == master) {
                            Out::Main
                        } else if let Some(j) = index_of(attached).filter(|&j| j != k) {
                            Out::Strip(j)
                        } else if category == Some(ROUTE_MASTER) {
                            Out::Main
                        } else if category == Some(ROUTE_PARENT_GROUP) {
                            group.map_or(Out::Main, Out::Strip)
                        } else {
                            let name = r.get("display_name").and_then(Value::as_str).unwrap_or("no output");
                            Out::Elsewhere(name.to_owned())
                        }
                    }
                };
                Strip {
                    name: field_str(t, "name").to_owned(),
                    mute: field_bool(t, "mute") == Some(true),
                    solo: field_bool(t, "solo") == Some(true),
                    muted_via_solo: field_bool(t, "muted_via_solo") == Some(true),
                    group,
                    is_group: field_bool(t, "is_foldable") == Some(true),
                    out,
                }
            })
            .collect();
        Self { strips }
    }

    /// The tracks track `k`'s audio passes through on its way out, starting with `k` itself,
    /// and where it ends up: `None` for Main, or the name of where it goes instead.
    pub fn route(&self, k: usize) -> (Vec<usize>, Option<String>) {
        let mut path = Vec::new();
        let mut at = k;
        loop {
            let Some(strip) = self.strips.get(at) else { return (path, None) };
            if path.contains(&at) {
                return (path, None); // a malformed answer; Live's routing does not loop
            }
            path.push(at);
            match &strip.out {
                Out::Strip(j) => at = *j,
                Out::Main => return (path, None),
                Out::Elsewhere(name) => return (path, Some(name.clone())),
            }
        }
    }

    /// Why track `k` does not reach the mix, or `None` if it does: it, or a track its audio
    /// passes through, is muted or silenced by solo, or its output goes somewhere else.
    pub fn silenced(&self, k: usize) -> Option<String> {
        let (path, elsewhere) = self.route(k);
        for &s in &path {
            let strip = &self.strips[s];
            let through = |what: &str| {
                let kind = if strip.is_group { "its group" } else { "the track it plays through" };
                format!("{kind}, {}, is {what}", strip.name)
            };
            if strip.mute {
                return Some(if s == k { "muted".into() } else { through("muted") });
            }
            if strip.muted_via_solo {
                return Some(if s == k { "another track is soloed".into() } else { through("silenced by a solo") });
            }
        }
        let name = elsewhere?;
        let at = &self.strips[*path.last()?].name;
        Some(if path.len() == 1 {
            format!("its output goes to {name}, not Main")
        } else {
            format!("its audio goes on from {at} to {name}, not Main")
        })
    }

    /// The tracks whose output goes straight into track `k`.
    fn feeders(&self, k: usize) -> impl Iterator<Item = usize> + '_ {
        self.strips.iter().enumerate().filter(move |(_, s)| s.out == Out::Strip(k)).map(|(j, _)| j)
    }

    /// Whether all of track `k`'s audible signal is measured by the Ears of `voices` (or there is
    /// none): `k` is silent, is one of them, or is a group whose tracks are all covered.
    fn covered(&self, k: usize, voices: &[usize], depth: usize) -> bool {
        self.silenced(k).is_some() || voices.contains(&k) || self.replaced(k, voices, depth)
    }

    /// Whether group `g`'s Ear can give way to its tracks' Ears: every track that plays into it
    /// is covered by `voices`, and at least one of them is audible. (Only for groups: another
    /// track that tracks are routed into can play clips of its own.)
    fn replaced(&self, g: usize, voices: &[usize], depth: usize) -> bool {
        if depth > self.strips.len() || !self.strips.get(g).is_some_and(|s| s.is_group) {
            return false;
        }
        let feeders: Vec<usize> = self.feeders(g).collect();
        feeders.iter().any(|&j| self.silenced(j).is_none())
            && feeders.iter().all(|&j| self.covered(j, voices, depth + 1))
    }

    /// Why the Ear on track `k`, one of `voices` (the tracks listened to whose Ears heard
    /// something and that reach the mix), does not count, so that no signal is counted twice:
    /// its tracks' Ears count instead, or it plays through a track whose Ear counts.
    pub fn overlap(&self, k: usize, voices: &[usize]) -> Option<String> {
        if self.replaced(k, voices, 0) {
            return Some("a group whose tracks have their own Ears, which count instead".into());
        }
        let (path, _) = self.route(k);
        let by = path.iter().skip(1).find(|&&s| voices.contains(&s) && !self.replaced(s, voices, 0))?;
        Some(format!("its audio is counted in the Ear on {}, which it plays through", self.strips[*by].name))
    }
}

/// A track's band levels over the bars its Ear heard (at least half of), or `None` if it heard
/// none.
fn band_levels(bars: &[BarReport]) -> Option<[f64; 5]> {
    let levels: Option<Vec<f64>> = (0..5)
        .map(|i| energy_mean(bars.iter().filter(|r| r.coverage >= 0.5).map(|r| r.stats.bands_dbfs[i] as f64)))
        .collect();
    levels.map(|l| std::array::from_fn(|i| l[i]))
}

/// Whether an Ear heard enough of these bars for its track to count.
pub fn heard_anything(bars: &[BarReport]) -> bool {
    band_levels(bars).is_some()
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
            let levels = band_levels(t.bars)?;
            Some((t, levels.map(|l| l + gain)))
        })
        .collect();
    let excluded: Vec<Value> = tracks
        .iter()
        .filter_map(|t| {
            let why = t.left_out.as_deref().or_else(|| {
                if t.gain_db.is_none() {
                    Some("volume at −∞")
                } else {
                    (!heard_anything(t.bars)).then_some("its Ear heard none of these bars")
                }
            })?;
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
             after each track's fader and the faders of the tracks its audio passes through on the way to Main \
             (its groups, or a track it is routed to); tracks that are muted, silenced by solo or do not reach \
             Main are left out, and so is an Ear whose signal another Ear already counts. Pan is ignored. Sends \
             count through the return tracks that have an Ear."
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

    /// Where a test track's output goes.
    #[derive(Clone, Copy)]
    enum To {
        /// Live's default: its group, or Main.
        Default,
        Main,
        Track(usize),
        Nowhere(&'static str),
    }

    /// A test track: name, whether it is a group, the group it is in, and where it plays to.
    #[derive(Clone, Copy)]
    struct T {
        name: &'static str,
        group: Option<usize>,
        is_group: bool,
        to: To,
        mute: bool,
        via_solo: bool,
    }

    fn track(name: &'static str) -> T {
        T { name, group: None, is_group: false, to: To::Default, mute: false, via_solo: false }
    }

    fn group(name: &'static str) -> T {
        T { is_group: true, ..track(name) }
    }

    impl T {
        fn r#in(self, g: usize) -> Self {
            T { group: Some(g), ..self }
        }
        fn to(self, to: To) -> Self {
            T { to, ..self }
        }
        fn muted(self) -> Self {
            T { mute: true, ..self }
        }
        fn via_solo(self) -> Self {
            T { via_solo: true, ..self }
        }
    }

    /// The mixer as `from_live` reads it from Live: tracks (then returns) with ids, group_track
    /// as an object, and each output_routing_type as Live reports it.
    fn mixer(tracks: &[T], returns: &[T]) -> Mixer {
        const MASTER: u64 = 9999;
        let id = |i: usize| 1000 + i as u64;
        let all: Vec<T> = tracks.iter().chain(returns).copied().collect();
        let list = |range: std::ops::Range<usize>| {
            Value::Array(
                range
                    .map(|i| {
                        let t = &all[i];
                        json!({"$obj": id(i), "type": "Track", "fields": {
                            "name": t.name, "mute": t.mute, "solo": false, "muted_via_solo": t.via_solo,
                            "is_foldable": t.is_group,
                            "group_track": t.group.map(|g| json!({"$obj": id(g), "type": "Track"})),
                        }})
                    })
                    .collect(),
            )
        };
        let main = json!({"display_name": "Main", "category": 3, "attached_object": {"$obj": MASTER, "type": "Track"}});
        let routes: Vec<Value> = all
            .iter()
            .map(|t| match (t.to, t.group) {
                (To::Default, Some(_)) => json!({"display_name": "Group", "category": 5, "attached_object": null}),
                (To::Default, None) | (To::Main, _) => main.clone(),
                (To::Track(j), _) => json!({
                    "display_name": all[j].name, "category": 4, "attached_object": {"$obj": id(j), "type": "Track"},
                }),
                (To::Nowhere(name), _) => json!({"display_name": name, "category": 6, "attached_object": null}),
            })
            .collect();
        Mixer::from_live(
            &list(0..tracks.len()),
            &list(tracks.len()..all.len()),
            &routes,
            &json!({"$obj": MASTER, "type": "Track"}),
        )
    }

    /// Drums [Kick, Perc [Shaker, Hat]], Bass, and a return.
    fn drums(change: impl Fn(&mut [T])) -> Mixer {
        let mut tracks = [
            group("Drums"),
            track("Kick").r#in(0),
            group("Perc").r#in(0),
            track("Shaker").r#in(2),
            track("Hat").r#in(2),
            track("Bass"),
        ];
        change(&mut tracks);
        mixer(&tracks, &[track("A-Reverb")])
    }

    #[test]
    fn routes_follow_the_output_routing() {
        let m = drums(|_| {});
        assert_eq!(m.strips.len(), 7);
        assert_eq!(m.route(3), (vec![3, 2, 0], None));
        assert_eq!(m.route(1), (vec![1, 0], None));
        assert_eq!(m.route(5), (vec![5], None));
        assert_eq!(m.route(6), (vec![6], None), "a return plays to Main");
        assert!(m.strips[0].is_group && !m.strips[1].is_group && m.strips[6].name == "A-Reverb");
        assert!((0..7).all(|k| m.silenced(k).is_none()));

        // A track in a group can play straight to Main, or into another track, past its group.
        let m = drums(|t| {
            t[1] = t[1].to(To::Main);
            t[3] = t[3].to(To::Track(5));
        });
        assert_eq!(m.route(1), (vec![1], None));
        assert_eq!(m.route(3), (vec![3, 5], None));
        assert_eq!(m.strips[1].group, Some(0), "still in its group");

        // A route that could not be read: the default, through its group.
        let list = json!([
            {"$obj": 1, "fields": {"name": "Drums", "is_foldable": true}},
            {"$obj": 2, "fields": {"name": "Kick", "group_track": {"$obj": 1}}},
        ]);
        let m = Mixer::from_live(&list, &Value::Null, &[Value::Null, Value::Null], &Value::Null);
        assert_eq!(m.route(1), (vec![1, 0], None));
    }

    #[test]
    fn muting_anywhere_on_the_route_silences() {
        let m = drums(|t| t[0] = t[0].muted());
        assert_eq!(m.silenced(0).as_deref(), Some("muted"));
        assert_eq!(m.silenced(1).as_deref(), Some("its group, Drums, is muted"));
        assert_eq!(m.silenced(3).as_deref(), Some("its group, Drums, is muted"));
        assert_eq!(m.silenced(5), None);

        // A track that plays past its muted group is still heard; one routed into a muted
        // track is not.
        let m = drums(|t| {
            t[0] = t[0].muted();
            t[1] = t[1].to(To::Main);
            t[5] = t[5].muted();
            t[4] = t[4].to(To::Track(5));
        });
        assert_eq!(m.silenced(1), None);
        assert_eq!(m.silenced(4).as_deref(), Some("the track it plays through, Bass, is muted"));
    }

    #[test]
    fn solo_is_read_from_live() {
        // Live says which tracks a solo silences, whatever the solo mode and routing.
        let m = drums(|t| {
            t[5] = t[5].via_solo();
            t[2] = t[2].via_solo();
        });
        assert_eq!(m.silenced(5).as_deref(), Some("another track is soloed"));
        assert_eq!(m.silenced(2).as_deref(), Some("another track is soloed"));
        assert_eq!(m.silenced(3).as_deref(), Some("its group, Perc, is silenced by a solo"));
        assert_eq!(m.silenced(1), None);
    }

    #[test]
    fn output_that_does_not_reach_main_is_left_out() {
        let m = drums(|t| {
            t[5] = t[5].to(To::Nowhere("Sends Only"));
            t[1] = t[1].to(To::Track(5));
        });
        assert_eq!(m.silenced(5).as_deref(), Some("its output goes to Sends Only, not Main"));
        assert_eq!(m.silenced(1).as_deref(), Some("its audio goes on from Bass to Sends Only, not Main"));
    }

    /// Which of `voices` count, given as names.
    fn counted(m: &Mixer, voices: &[usize]) -> Vec<&'static str> {
        let names = ["Drums", "Kick", "Perc", "Shaker", "Hat", "Bass", "A-Reverb"];
        voices.iter().filter(|&&k| m.overlap(k, voices).is_none()).map(|&k| names[k]).collect()
    }

    #[test]
    fn a_group_ear_gives_way_only_to_ears_that_cover_it() {
        // Every track in the group has an Ear: they count instead of the group.
        let m = drums(|_| {});
        assert_eq!(counted(&m, &[0, 1, 3, 4, 5]), ["Kick", "Shaker", "Hat", "Bass"]);
        assert_eq!(
            m.overlap(0, &[0, 1, 3, 4, 5]).as_deref(),
            Some("a group whose tracks have their own Ears, which count instead")
        );
        // The subgroup's Ear covers its tracks too.
        assert_eq!(counted(&m, &[0, 1, 2, 5]), ["Kick", "Perc", "Bass"]);
        // Hat has no Ear: Perc's Ear counts, and Shaker's (inside it) does not. Perc in turn
        // covers its part of Drums.
        assert_eq!(counted(&m, &[0, 1, 2, 3, 5]), ["Kick", "Perc", "Bass"]);
        assert_eq!(
            m.overlap(3, &[0, 1, 2, 3, 5]).as_deref(),
            Some("its audio is counted in the Ear on Perc, which it plays through")
        );
        // Without Perc's Ear, Drums is not covered, so Drums counts and the tracks in it don't.
        assert_eq!(counted(&m, &[0, 1, 3, 5]), ["Drums", "Bass"]);

        // A silent track needs no Ear, but a group whose only sound is its own Ear's stays.
        let m = drums(|t| t[4] = t[4].muted());
        assert_eq!(counted(&m, &[0, 1, 3, 5]), ["Kick", "Shaker", "Bass"]);
        let m = drums(|t| {
            t[3] = t[3].muted();
            t[4] = t[4].muted();
        });
        assert_eq!(counted(&m, &[2]), ["Perc"]);
    }

    #[test]
    fn a_muted_or_unheard_track_does_not_displace_its_group() {
        // Kick is muted (so not a voice): its Ear must not take the place of Drums, which
        // still carries the rest of the kit.
        let m = drums(|t| t[1] = t[1].muted());
        assert_eq!(counted(&m, &[0, 5]), ["Drums", "Bass"]);
        // Kick's Ear is not among the voices (muted, −∞ or nothing heard): Drums still counts.
        let m = drums(|_| {});
        assert_eq!(counted(&m, &[0, 5]), ["Drums", "Bass"]);
    }

    #[test]
    fn only_the_tracks_that_play_through_a_group_overlap_it() {
        // Kick plays straight to Main: it does not overlap Drums, and is not needed to cover it.
        let m = drums(|t| t[1] = t[1].to(To::Main));
        assert_eq!(counted(&m, &[0, 1, 5]), ["Drums", "Kick", "Bass"]);
        assert_eq!(counted(&m, &[0, 1, 2, 5]), ["Kick", "Perc", "Bass"]);
        // Into a bus that is not a group: the bus's Ear counts (it may play clips of its own).
        let m = drums(|t| t[1] = t[1].to(To::Track(5)));
        assert_eq!(counted(&m, &[1, 5]), ["Bass"]);
        assert_eq!(
            m.overlap(1, &[1, 5]).as_deref(),
            Some("its audio is counted in the Ear on Bass, which it plays through")
        );
    }
}
