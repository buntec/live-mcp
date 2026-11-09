//! Messages from the LiveMCP Ear plugin to the MCP server: newline-delimited JSON over TCP on
//! 127.0.0.1. The plugin connects to the server (the server listens), so plugins can come and go.

use live_mcp_analysis::{BarReport, BeatReport};
use serde::{Deserialize, Serialize};

/// Version 2 added [`EarMessage::Beat`]; version 3 added the bass line to beat reports
/// (`bass_notes`, `bass_cents`, `bass_confidence`, empty when parsed from version 2).
pub const EAR_PROTOCOL_VERSION: u32 = 3;
/// The oldest plugin protocol the server still accepts. Version 1 plugins report bars only.
pub const EAR_MIN_PROTOCOL_VERSION: u32 = 1;

/// Default port the MCP server listens on for plugins. `~/.live-mcp/config.json` can set
/// `{"ear_port": N}` (0 = any free port); the server advertises the port it actually bound in
/// `~/.live-mcp/ear.json`.
pub const EAR_DEFAULT_PORT: u16 = 19419;

/// The `Tag` parameter's range. The server writes a tag into each instance's parameter
/// through the Live API, and the plugin reports it, which tells the server which track an
/// instance is on.
pub const TAG_MAX: u32 = 4095;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EarMessage {
    /// First message on a connection.
    Hello {
        /// Contents of `~/.live-mcp/token`.
        token: String,
        protocol: u32,
        /// Random per plugin instance, stable for the instance's lifetime.
        instance: u64,
        plugin_version: String,
        sample_rate: f32,
    },
    /// Sent a few times per second, playing or not.
    Heartbeat { tag: u32, playing: bool, beat: Option<f64> },
    /// One finished bar.
    Bar { tag: u32, report: BarReport },
    /// One finished beat: pitch and onset features (protocol 2).
    Beat { tag: u32, report: BeatReport },
}

/// Contents of `~/.live-mcp/ear.json`, written by the server once it listens for plugins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EarEndpoint {
    pub port: u16,
    pub pid: u32,
}

#[cfg(test)]
mod tests {
    use live_mcp_analysis::{BarAnalyzer, Transport};

    use super::*;

    #[test]
    fn messages_round_trip_as_tagged_json() {
        let hello = EarMessage::Hello {
            token: "t".into(),
            protocol: EAR_PROTOCOL_VERSION,
            instance: 42,
            plugin_version: "0.1.0".into(),
            sample_rate: 48_000.0,
        };
        let text = serde_json::to_string(&hello).unwrap();
        assert!(text.starts_with(r#"{"type":"hello""#), "{text}");
        assert_eq!(serde_json::from_str::<EarMessage>(&text).unwrap(), hello);

        let mut analyzer = BarAnalyzer::new(48_000.0);
        let mut reports = Vec::new();
        let t = Transport { playing: true, beat: Some(0.0), tempo: Some(240.0), time_signature: Some((4, 4)) };
        analyzer.process(&[0.25; 48_000], &[0.25; 48_000], &t, &mut |r| reports.push(r));
        let bar = EarMessage::Bar { tag: 3, report: reports[0] };
        let text = serde_json::to_string(&bar).unwrap();
        assert_eq!(serde_json::from_str::<EarMessage>(&text).unwrap(), bar);
    }
}
