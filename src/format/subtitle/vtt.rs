//! WebVTT (`.vtt`) grammar.
//!
//! Shares the timed-cue grammar with SRT (see [`super`]). The default
//! [`super::SubFormat`] impl handles VTT — including the `WEBVTT` header and
//! `NOTE`/`STYLE`/`REGION` blocks, which have no `-->` line and so become
//! passthrough cues emitted verbatim. Decimal-comma-vs-dot timing is preserved
//! automatically because the framing is stored byte-exact.

use super::SubFormat;

/// Marker type for the WebVTT grammar.
pub struct Vtt;

impl SubFormat for Vtt {
    const NAME: &'static str = "vtt";
}
