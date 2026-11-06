//! Decoding audio files (WAV, AIFF, FLAC, MP3, AAC/M4A, Ogg Vorbis, ALAC, CAF) with symphonia.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// Longest file decoded. Decoded audio takes about 1.4 GB per hour (48 kHz stereo f32), so
/// this caps memory at roughly 700 MB.
const MAX_SECONDS: f64 = 30.0 * 60.0;

pub struct Decoded {
    pub sample_rate: u32,
    /// Channels in the file; the analysis uses the first two (mono is duplicated).
    pub channels: usize,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl Decoded {
    pub fn seconds(&self) -> f64 {
        self.left.len() as f64 / self.sample_rate as f64
    }
}

pub fn decode(path: &Path) -> Result<Decoded> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .with_context(|| format!("{} is not an audio file symphonia can read", path.display()))?;
    let track = format.default_track(TrackType::Audio).context("the file has no audio track")?;
    let track_id = track.id;
    let params =
        track.codec_params.as_ref().and_then(|p| p.audio()).context("the audio track has no codec parameters")?.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .context("unsupported audio codec")?;

    let mut sample_rate = params.sample_rate.unwrap_or(0);
    let mut channels = params.channels.as_ref().map_or(0, |c| c.count());
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut interleaved: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading the file"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            Err(SymphoniaError::DecodeError(_)) => continue, // a corrupt frame: skip it
            Err(e) => return Err(e).context("decoding the file"),
        };
        let spec = buffer.spec();
        sample_rate = spec.rate();
        channels = spec.channels().count().max(1);
        interleaved.clear();
        buffer.copy_to_vec_interleaved(&mut interleaved);
        for frame in interleaved.chunks_exact(channels) {
            left.push(frame[0]);
            right.push(*frame.get(1).unwrap_or(&frame[0]));
        }
        if sample_rate > 0 && left.len() as f64 / sample_rate as f64 > MAX_SECONDS {
            bail!("the file is longer than {} minutes", MAX_SECONDS / 60.0);
        }
    }
    if sample_rate == 0 || left.is_empty() {
        bail!("{} contains no audio", path.display());
    }
    Ok(Decoded { sample_rate, channels, left, right })
}

#[cfg(test)]
pub mod tests {
    use std::io::Write;

    use super::*;

    /// Writes a 16-bit PCM WAV file (the simplest format to produce without a dependency).
    pub fn write_wav(path: &Path, sample_rate: u32, channels: &[Vec<f32>]) {
        let frames = channels[0].len();
        let n = channels.len() as u16;
        let data_len = (frames * channels.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * n as u32 * 2).to_le_bytes());
        out.extend_from_slice(&(n * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            for channel in channels {
                let s = (channel[i].clamp(-1.0, 1.0) * 32767.0).round() as i16;
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        File::create(path).unwrap().write_all(&out).unwrap();
    }

    #[test]
    fn decodes_stereo_and_mono_wav() {
        let dir = std::env::temp_dir().join(format!("live-mcp-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ramp: Vec<f32> = (0..44_100).map(|i| (i as f32 / 44_100.0) - 0.5).collect();
        let quiet: Vec<f32> = ramp.iter().map(|x| x * 0.5).collect();

        let stereo = dir.join("stereo.wav");
        write_wav(&stereo, 44_100, &[ramp.clone(), quiet.clone()]);
        let decoded = decode(&stereo).unwrap();
        assert_eq!((decoded.sample_rate, decoded.channels, decoded.left.len()), (44_100, 2, 44_100));
        assert!((decoded.left[30_000] - ramp[30_000]).abs() < 1e-4);
        assert!((decoded.right[30_000] - quiet[30_000]).abs() < 1e-4);
        assert!((decoded.seconds() - 1.0).abs() < 1e-9);

        let mono = dir.join("mono.wav");
        write_wav(&mono, 22_050, std::slice::from_ref(&quiet));
        let decoded = decode(&mono).unwrap();
        assert_eq!((decoded.channels, decoded.left, decoded.right.len()), (1, decoded.right.clone(), 44_100));

        std::fs::write(dir.join("not-audio.wav"), b"hello").unwrap();
        assert!(decode(&dir.join("not-audio.wav")).is_err());
        let missing = decode(&dir.join("missing.wav")).err().expect("missing file is an error");
        assert!(missing.to_string().contains("opening"), "{missing}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
