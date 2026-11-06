//! Which tracks share each frequency band: a first, band-level look at masking.

use live_mcp_analysis::{BAND_EDGES_HZ, BAND_NAMES, BarReport};
use serde_json::{Value, json};

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
    /// Track volume in dB (post-Ear). `None` for −∞ (fader all the way down).
    pub gain_db: Option<f64>,
    pub muted: bool,
    pub bars: &'a [BarReport],
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
        .filter(|t| !t.muted)
        .filter_map(|t| {
            let gain = t.gain_db?;
            let levels: Option<Vec<f64>> = (0..5)
                .map(|i| energy_mean(t.bars.iter().filter(|r| r.coverage >= 0.5).map(|r| r.stats.bands_dbfs[i] as f64)))
                .collect();
            let levels = levels?;
            Some((t, std::array::from_fn(|i| levels[i] + gain)))
        })
        .collect();
    let excluded: Vec<String> =
        tracks.iter().filter(|t| t.muted || t.gain_db.is_none()).map(|t| t.name.clone()).collect();

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
             post-fader; pan and sends are ignored."
        ),
    });
    if !excluded.is_empty() {
        out["excluded"] = json!(excluded);
        out["excluded_reason"] = json!("muted, or volume at −∞");
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
        Heard { label: json!(name), name: name.into(), gain_db, muted: false, bars }
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
        muted.muted = true;
        let out = analyze(&[heard("Bass", Some(0.0), &bass), muted, heard("Sub", None, &kick)]);
        assert_eq!(out["excluded"], json!(["Kick", "Sub"]));
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
}
