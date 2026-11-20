//! SubRip (`.srt`) grammar.
//!
//! Shares the timed-cue grammar with VTT (see [`super`]); the default
//! [`super::SubFormat`] impl handles SRT exactly. This type exists so SRT can
//! diverge later without touching VTT, and so `Format::Srt` maps to a concrete
//! backend.

use super::SubFormat;

/// Marker type for the SubRip grammar.
pub struct Srt;

impl SubFormat for Srt {
    const NAME: &'static str = "srt";
}
