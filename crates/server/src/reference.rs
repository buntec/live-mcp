//! `analyze_file`: the same measurements as `listen`, for an audio file such as a reference
//! track, plus whole-file loudness (gated integrated loudness and loudness range).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use live_mcp_analysis::{BAND_NAMES, BarAnalyzer, BarReport, ProgrammeLoudness, SignalMeter, Sums, Transport};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::audio_file::{self, Decoded};
use crate::ear::{compact_bar, round1, round2};
use crate::live::default_true;
use crate::snapshots::{Kind, SnapshotStore};

/// Results larger than this many bars are summarised instead of listed.
const MAX_LISTED_BARS: usize = 400;
const CHUNK: usize = 4096;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeFileParams {
    /// Absolute path to the file (a leading ~ is expanded): WAV, AIFF, FLAC, MP3, AAC/M4A, Ogg
    /// Vorbis, ALAC or CAF.
    pub path: String,
    /// The file's tempo, to report musical bars. Without it, the file is cut into sections of
    /// `section_seconds` instead.
    pub bpm: Option<f64>,
    /// Time signature, e.g. "4/4" (default) or "6/8".
    pub time_signature: Option<String>,
    /// Where bar 1 starts, in seconds from the start of the file (default 0). Audio before it
    /// (a pickup or silence) counts towards the whole-file figures only.
    pub first_downbeat: Option<f64>,
    /// Section length when no bpm is given (default 2 s).
    pub section_seconds: Option<f64>,
    /// List the bars or sections (default true); false gives only the whole-file summary.
    #[serde(default = "default_true")]
    pub bars: bool,
}

pub async fn analyze_file(store: &SnapshotStore, params: AnalyzeFileParams) -> Result<Value> {
    let path = expand_home(&params.path)?;
    let decoded = tokio::task::spawn_blocking({
        let path = path.clone();
        move || audio_file::decode(&path)
    })
    .await
    .context("the decoder panicked")??;
    let (num, den) = parse_time_signature(params.time_signature.as_deref())?;
    let beats_per_bar = num as f64 * 4.0 / den as f64;
    let first_downbeat = params.first_downbeat.unwrap_or(0.0);
    if !(first_downbeat.is_finite() && first_downbeat >= 0.0 && first_downbeat < decoded.seconds()) {
        bail!("first_downbeat must be within the file (0–{:.1} s)", decoded.seconds());
    }
    // Without a tempo, choose one that makes each "bar" last section_seconds.
    let (tempo, unit) = match params.bpm {
        Some(bpm) if bpm.is_finite() && (20.0..=999.0).contains(&bpm) => (bpm, "bar"),
        Some(bpm) => bail!("bpm must be 20–999, got {bpm}"),
        None => {
            let seconds = params.section_seconds.unwrap_or(2.0);
            if !(0.1..=60.0).contains(&seconds) {
                bail!("section_seconds must be 0.1–60");
            }
            (beats_per_bar * 60.0 / seconds, "section")
        }
    };
    let analysis = tokio::task::spawn_blocking(move || measure(&decoded, tempo, (num, den), first_downbeat))
        .await
        .context("the analysis panicked")?;

    let seconds_per_beat = 60.0 / tempo;
    let mut out = json!({
        "file": path.display().to_string(),
        "duration_seconds": round2(analysis.seconds as f32),
        "sample_rate": analysis.sample_rate,
        "channels": analysis.channels,
        "summary": analysis.summary,
    });
    if unit == "bar" {
        out["tempo"] = json!(tempo);
        out["time_signature"] = json!(format!("{num}/{den}"));
    }
    if params.bars {
        let listed: Vec<Value> = analysis
            .bars
            .iter()
            .take(MAX_LISTED_BARS)
            .map(|r| {
                let mut entry = compact_bar(r);
                if unit == "section" {
                    entry.as_object_mut().expect("object").remove("bar");
                    entry["section"] = json!(r.bar);
                }
                entry["start_seconds"] = json!(round2((first_downbeat + r.start_beat * seconds_per_beat) as f32));
                entry
            })
            .collect();
        out[if unit == "bar" { "bars" } else { "sections" }] = json!(listed);
        if analysis.bars.len() > MAX_LISTED_BARS {
            out["note"] =
                json!(format!("only the first {MAX_LISTED_BARS} of {} {unit}s are listed", analysis.bars.len()));
        }
    }
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    let label = if unit == "bar" { format!("{name} at {tempo} BPM") } else { name };
    out["snapshot"] = json!(store.add(Kind::File, label, unit, analysis.bars));
    Ok(out)
}

struct Analysis {
    seconds: f64,
    sample_rate: u32,
    channels: usize,
    bars: Vec<BarReport>,
    summary: Value,
}

fn measure(decoded: &Decoded, tempo: f64, time_signature: (u32, u32), first_downbeat: f64) -> Analysis {
    let sr = decoded.sample_rate as f64;
    let mut programme = ProgrammeLoudness::new(sr);
    let mut whole = SignalMeter::new(sr);
    let mut whole_sums = Sums::default();
    programme.feed(&decoded.left, &decoded.right);
    whole.feed(&decoded.left, &decoded.right, &mut whole_sums);

    // Bars start at the first downbeat; the analyzer sees a transport playing from beat 0 there.
    let start = (first_downbeat * sr).round() as usize;
    let mut analyzer = BarAnalyzer::new(decoded.sample_rate as f32);
    let mut bars = Vec::new();
    let mut i = start;
    while i < decoded.left.len() {
        let end = (i + CHUNK).min(decoded.left.len());
        let beat = (i - start) as f64 / sr * tempo / 60.0;
        let transport =
            Transport { playing: true, beat: Some(beat), tempo: Some(tempo), time_signature: Some(time_signature) };
        analyzer.process(&decoded.left[i..end], &decoded.right[i..end], &transport, &mut |r| bars.push(r));
        i = end;
    }
    analyzer.flush(&mut |r| bars.push(r));

    let stats = whole_sums.stats();
    let mut bands = Map::new();
    for (name, db) in BAND_NAMES.iter().zip(stats.bands_dbfs) {
        bands.insert((*name).into(), json!(round1(db)));
    }
    let loudest = bars.iter().max_by(|a, b| a.stats.loudness_lufs.total_cmp(&b.stats.loudness_lufs)).map(|r| r.bar);
    let mut summary = json!({
        "integrated_lufs": round1(programme.integrated()),
        "loudness_range_lu": round1(programme.range()),
        "max_short_term_lufs": round1(programme.max_short_term()),
        "peak": round1(stats.peak_dbfs[0].max(stats.peak_dbfs[1])),
        "rms": round1(stats.rms_dbfs[0].max(stats.rms_dbfs[1])),
        "crest": round1(stats.crest_db),
        "bands": bands,
        "correlation": round2(stats.correlation),
        "width": round2(stats.width),
    });
    if let Some(bar) = loudest {
        summary["loudest_bar"] = json!(bar);
    }
    if stats.clipped_samples > 0 {
        summary["clipped"] = json!(stats.clipped_samples);
    }
    Analysis { seconds: decoded.seconds(), sample_rate: decoded.sample_rate, channels: decoded.channels, bars, summary }
}

fn expand_home(path: &str) -> Result<PathBuf> {
    let path = match path.strip_prefix("~/") {
        Some(rest) => live_mcp_bridge::paths::home_dir().context("no home directory")?.join(rest),
        None => PathBuf::from(path),
    };
    if !path.is_absolute() {
        bail!("give an absolute path (or one starting with ~/), got {}", path.display());
    }
    Ok(path)
}

fn parse_time_signature(text: Option<&str>) -> Result<(u32, u32)> {
    let Some(text) = text else { return Ok((4, 4)) };
    let parsed =
        text.split_once('/').and_then(|(n, d)| Some((n.trim().parse::<u32>().ok()?, d.trim().parse::<u32>().ok()?)));
    match parsed {
        Some((n, d)) if (1..=32).contains(&n) && [1, 2, 4, 8, 16, 32].contains(&d) => Ok((n, d)),
        _ => bail!("time_signature must look like \"4/4\" or \"6/8\", got {text:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_signatures_and_paths() {
        assert_eq!(parse_time_signature(None).unwrap(), (4, 4));
        assert_eq!(parse_time_signature(Some("6/8")).unwrap(), (6, 8));
        assert!(parse_time_signature(Some("4/3")).is_err());
        assert!(parse_time_signature(Some("four")).is_err());
        assert!(expand_home("relative.wav").is_err());
        assert!(expand_home("~/x.wav").unwrap().is_absolute());
    }
}
