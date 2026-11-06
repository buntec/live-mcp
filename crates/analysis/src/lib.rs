//! Audio measurements for live-mcp, independent of any plugin framework.
//!
//! [`BarAnalyzer`] takes blocks of audio plus the host's transport position and reports, for
//! every bar, loudness (ITU-R BS.1770), peak and RMS, crest factor, energy in five frequency
//! bands, stereo correlation and width, and clipping. Processing never allocates, so it can
//! run on the audio thread.

pub mod bars;
pub mod filter;
pub mod meter;

pub use bars::{BarAnalyzer, BarReport, Transport};
pub use meter::{BAND_EDGES_HZ, BAND_NAMES, FLOOR_DB, SignalMeter, Stats, Sums};
