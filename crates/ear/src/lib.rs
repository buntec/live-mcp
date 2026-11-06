//! LiveMCP Ear: a VST3 audio-analysis plugin for live-mcp.
//!
//! M2 step 1: audio passes through unchanged. This proves the plugin builds, bundles and
//! loads in Live. Measurement (in the `live-mcp-analysis` crate) and streaming results to
//! the MCP server come next.

use std::num::NonZeroU32;
use std::sync::Arc;

use nice_plug::prelude::*;

#[derive(Default)]
pub struct Ear {
    params: Arc<EarParams>,
}

#[derive(Params, Default)]
struct EarParams {}

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
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // The host has already copied input to output; leaving the buffer untouched passes audio
        // through. KeepAlive: an analyzer must keep running even while its input is silent.
        ProcessStatus::KeepAlive
    }
}

impl Vst3Plugin for Ear {
    const VST3_CLASS_ID: [u8; 16] = *b"LiveMcpEar\0\0\0\0\0\x01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Analyzer];
}

nice_export_vst3!(Ear);
