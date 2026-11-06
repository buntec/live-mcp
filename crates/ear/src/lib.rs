//! LiveMCP Ear: a VST3 audio-analysis plugin for live-mcp.
//!
//! It passes audio through unchanged and measures it, bar by bar, at its place in Live's
//! signal chain (see `live-mcp-analysis`). The measurements stream to the live-mcp MCP server
//! over a local socket. A `Tag` parameter, which the server sets through the Live API, tells
//! the server which track each instance is on.

pub mod core;
pub mod net;

use std::num::NonZeroU32;
use std::sync::Arc;

use live_mcp_analysis::Transport;
use live_mcp_protocol::ear::TAG_MAX;
use nice_plug::prelude::*;

use crate::core::{EarCore, EarTask};
use crate::net::{Uplink, default_state_dir};

pub struct Ear {
    params: Arc<EarParams>,
    core: Option<EarCore>,
    uplink: Arc<Uplink>,
}

#[derive(Params)]
struct EarParams {
    /// Set by live-mcp to identify this instance; leave it alone.
    #[id = "tag"]
    tag: IntParam,
}

impl Default for EarParams {
    fn default() -> Self {
        Self { tag: IntParam::new("Tag", 0, IntRange::Linear { min: 0, max: TAG_MAX as i32 }) }
    }
}

impl Default for Ear {
    fn default() -> Self {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        Self {
            params: Arc::new(EarParams::default()),
            core: None,
            uplink: Arc::new(Uplink::new(u64::from_le_bytes(bytes), default_state_dir())),
        }
    }
}

impl Plugin for Ear {
    const NAME: &'static str = "LiveMCP Ear";
    const VENDOR: &'static str = "live-mcp";
    const URL: &'static str = "https://github.com/buntec/live-mcp";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = EarTask;

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let uplink = self.uplink.clone();
        Box::new(move |task: EarTask| uplink.send(task.into_message()))
    }

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn activate(
        &mut self,
        _layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl ActivateContext<Self>,
    ) -> bool {
        self.core = Some(EarCore::new(buffer_config.sample_rate));
        self.uplink.set_sample_rate(buffer_config.sample_rate);
        true
    }

    fn reset(&mut self) {
        if let Some(core) = &mut self.core {
            core.reset();
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Audio passes through untouched: the host has already copied input to output.
        if let Some(core) = &mut self.core {
            let host = context.transport();
            let transport = Transport {
                playing: host.playing,
                beat: host.pos_beats(),
                tempo: host.tempo,
                time_signature: host
                    .time_sig_numerator
                    .zip(host.time_sig_denominator)
                    .filter(|(n, d)| *n > 0 && *d > 0)
                    .map(|(n, d)| (n as u32, d as u32)),
            };
            let channels = buffer.as_slice_immutable();
            if let Some(left) = channels.first() {
                let right = channels.get(1).unwrap_or(left);
                let tag = self.params.tag.value().clamp(0, TAG_MAX as i32) as u32;
                core.process(left, right, &transport, tag, &mut |task| context.execute_background(task));
            }
        }
        // An analyzer has to keep running while its input is silent.
        ProcessStatus::KeepAlive
    }
}

impl Vst3Plugin for Ear {
    const VST3_CLASS_ID: [u8; 16] = *b"LiveMcpEar\0\0\0\0\0\x01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Analyzer];
}

nice_export_vst3!(Ear);
