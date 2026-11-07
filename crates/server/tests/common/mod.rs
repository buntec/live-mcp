//! Shared harness: an MCP client driving `live-mcp serve`, which talks to a fake Live.

#![allow(dead_code)]

use std::path::PathBuf;

use live_mcp_bridge::paths::STATE_DIR_ENV;
use live_mcp_bridge::testkit::FakeLive;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

pub struct Harness {
    pub client: RunningService<RoleClient, ()>,
    pub fake: FakeLive,
}

impl Harness {
    pub async fn start() -> Option<Self> {
        Self::start_with(&[]).await
    }

    /// Starts `live-mcp <args> serve`.
    pub async fn start_with(args: &[&str]) -> Option<Self> {
        let fake = FakeLive::start()?;
        let state = fake.state_dir();
        let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_live-mcp")).configure(|cmd| {
            cmd.args(args).arg("serve").env(STATE_DIR_ENV, &state);
        });
        let client = ().serve(TokioChildProcess::new(command).expect("spawn live-mcp")).await.expect("initialize");
        Some(Self { client, fake })
    }

    /// Calls a tool; Ok with the parsed JSON result, or Err with the tool's error text.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let arguments = args.as_object().cloned().expect("arguments must be an object");
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
            .await
            .expect("tools/call");
        let text = result.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
        if result.is_error == Some(true) {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} returned non-JSON ({e}): {text}")))
        }
    }

    pub async fn ok(&self, name: &str, args: Value) -> Value {
        self.call(name, args).await.unwrap_or_else(|e| panic!("{name} failed: {e}"))
    }

    pub async fn err(&self, name: &str, args: Value) -> String {
        match self.call(name, args).await {
            Err(e) => e,
            Ok(v) => panic!("{name} unexpectedly succeeded: {v}"),
        }
    }
}

impl Harness {
    pub fn state_dir(&self) -> PathBuf {
        self.fake.state_dir()
    }
}

/// Writes a 16-bit PCM WAV file.
pub fn write_wav(path: &std::path::Path, sample_rate: u32, channels: &[Vec<f32>]) {
    let frames = channels[0].len();
    let n = channels.len() as u16;
    let data_len = (frames * channels.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * n as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(n * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        for channel in channels {
            out.extend_from_slice(&((channel[i].clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
        }
    }
    std::fs::write(path, out).unwrap();
}

/// A sine segment: (seconds, frequency in Hz, level in dBFS; below −200 means silence).
pub fn tone_sequence(sample_rate: u32, segments: &[(f64, f64, f64)]) -> Vec<f32> {
    let sr = sample_rate as f64;
    let mut out = Vec::new();
    for &(seconds, freq, dbfs) in segments {
        let amplitude = if dbfs < -200.0 { 0.0 } else { 10f64.powf(dbfs / 20.0) };
        let start = out.len();
        for k in 0..(seconds * sr).round() as usize {
            let t = (start + k) as f64 / sr;
            out.push((amplitude * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32);
        }
    }
    out
}
