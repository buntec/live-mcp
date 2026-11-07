//! Audio measurements for live-mcp, independent of any plugin framework.
//!
//! [`BarAnalyzer`] takes blocks of audio plus the host's transport position and reports, for
//! every bar, loudness (ITU-R BS.1770), peak and RMS, crest factor, energy in five frequency
//! bands, stereo correlation and width, and clipping. Processing never allocates, so it can
//! run on the audio thread.
//!
//! [`BeatAnalyzer`] reports, for every beat, chroma (energy per pitch class, in thirds of a
//! semitone, for the treble and the bass), energy per octave, tonalness, level and onset strength
//! per 16th note: the raw material for naming keys, chords and rhythms. It does not allocate
//! either.
//!
//! [`ProgrammeLoudness`] adds gated integrated loudness and loudness range over a whole
//! programme, for offline analysis of files.

pub mod bars;
pub mod beats;
pub mod filter;
pub mod loudness;
pub mod meter;
pub mod onset;
pub mod pitch;
pub mod spectrum;

pub use bars::{BarAnalyzer, BarReport, Transport};
pub use beats::{BeatAnalyzer, BeatReport};
pub use loudness::ProgrammeLoudness;
pub use meter::{BAND_EDGES_HZ, BAND_NAMES, FLOOR_DB, SignalMeter, Stats, Sums};
