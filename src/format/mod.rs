//! Format-agnostic document abstraction.
//!
//! Each input format (EPUB, SRT, VTT today; txt / docx later) implements
//! [`Document`]: it parses the file into translatable [`Segment`]s (plain
//! text) and knows how to write translations back. The translation engine
//! ([`crate::engine`]) is format-agnostic — each format merely hints at a
//! [`Strategy`] via [`Document::strategy`]: self-contained blocks (EPUB) go
//! out one per request, while short-flow segments (subtitle cues) ride in
//! contextual batches aligned strictly one-to-one (see [`crate::engine`]).

use crate::format::epub::EpubDoc;
use crate::format::subtitle::ass::Ass;
use crate::format::subtitle::lrc::Lrc;
use crate::format::subtitle::srt::Srt;
use crate::format::subtitle::vtt::Vtt;
use crate::format::subtitle::SubtitleDoc;
use crate::format::txt::TxtDoc;
use anyhow::{anyhow, Result};
use std::path::Path;

pub mod epub;
pub mod subtitle;
pub mod txt;

/// Identifier of a segment within a document. Dense `0..N`, assigned by the
/// backend in document order.
pub type SegmentId = usize;

/// One translatable unit: plain text. Backends retain richer structure (e.g.
/// the EPUB block's tag) in their own IR; the engine only ever sees this.
#[derive(Clone, Debug)]
pub struct Segment {
    pub id: SegmentId,
    pub text: String,
}

/// How translations are written back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputMode {
    /// Keep the original and add the translation alongside (bilingual).
    #[value(name = "bilingual")]
    Bilingual,
    /// Replace the original text with the translation.
    #[value(name = "replace")]
    Replace,
}

/// How the engine translates a document's segments.
///
/// A format picks its default via [`Document::strategy`]. Self-contained blocks
/// (EPUB paragraphs) translate one per request ([`Strategy::Independent`]);
/// short segments that only make sense in the surrounding flow (subtitle cues,
/// continuous prose) translate [`Strategy::Batched`] so the model sees context
/// and returns exactly one translation per segment, in order — no merge/split.
///
/// This is a *translation strategy*, orthogonal to format grammar: a novel
/// (block-structured like EPUB) may still prefer [`Strategy::Batched`] for
/// cross-paragraph coherence. The CLI can override the batch parameters.
#[derive(Clone, Copy, Debug)]
pub enum Strategy {
    /// One segment per request, no surrounding context. The default.
    Independent,
    /// Translate `batch_size` consecutive segments per request, each batch
    /// preceded by `context` read-only segments for fluency across boundaries.
    /// The model returns exactly one translation per segment, in order (see
    /// [`crate::translate::translate_batch`]).
    Batched {
        /// Segments sent per request and aligned strictly one-to-one.
        batch_size: usize,
        /// Read-only preceding segments riding along for context (not emitted).
        context: usize,
    },
}

/// A format backend: owns the parsed document and any rewrite intermediate
/// state, and knows how to extract translatable segments and write them back.
pub trait Document {
    /// Human-readable format name, for logs.
    fn format_name(&self) -> &'static str;

    /// All translatable segments in document order (dense ids `0..N`). Clones
    /// the text out; the backend keeps its IR for [`Document::write`].
    fn segments(&self) -> Vec<Segment>;

    /// Apply `translations` and serialize to `out`.
    ///
    /// `translations` MAY be a subset of the segments returned by
    /// [`Document::segments`] (due to `--limit` or a failed request); segments
    /// whose id is absent are emitted unchanged. Sync — local file IO only.
    fn write(
        &mut self,
        translations: &[(SegmentId, String)],
        out: &Path,
        mode: OutputMode,
    ) -> Result<()>;

    /// The translation strategy this format prefers. Default
    /// [`Strategy::Independent`] (self-contained blocks). Subtitle formats
    /// override to [`Strategy::Batched`]; the CLI may override the parameters.
    /// See [`crate::engine::Engine::translate`].
    fn strategy(&self) -> Strategy {
        Strategy::Independent
    }
}

/// Supported input formats. Add a variant here when a backend lands, plus a
/// matching arm in [`open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Epub,
    Srt,
    Vtt,
    Ass,
    Lrc,
    Txt,
    // TODO: Docx, Md, …
}

impl Format {
    /// Detect a format from the file extension (case-insensitive). Errors with
    /// a helpful message if the extension is unknown or unsupported.
    pub fn from_path(path: &Path) -> Result<Format> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("epub") => Ok(Format::Epub),
            Some("srt") => Ok(Format::Srt),
            Some("vtt") => Ok(Format::Vtt),
            Some("ass") | Some("ssa") => Ok(Format::Ass),
            Some("lrc") => Ok(Format::Lrc),
            Some("txt") => Ok(Format::Txt),
            // TODO: "md" | "docx" => …
            other => Err(anyhow!(
                "unsupported input format {:?} ({}); supported: epub, srt, vtt, ass, lrc, txt",
                other.unwrap_or("(none)"),
                path.display()
            )),
        }
    }
}

/// Open a document, dispatching on extension (or an explicit `hint`). This is
/// the only place a new format needs wiring up: add a [`Format`] variant, an
/// arm here, and the backend file.
pub fn open(path: &Path, hint: Option<Format>) -> Result<Box<dyn Document>> {
    let fmt = match hint {
        Some(f) => f,
        None => Format::from_path(path)?,
    };
    match fmt {
        Format::Epub => Ok(Box::new(EpubDoc::open(path)?)),
        Format::Srt => Ok(Box::new(SubtitleDoc::<Srt>::open(path)?)),
        Format::Vtt => Ok(Box::new(SubtitleDoc::<Vtt>::open(path)?)),
        Format::Ass => Ok(Box::new(SubtitleDoc::<Ass>::open(path)?)),
        Format::Lrc => Ok(Box::new(SubtitleDoc::<Lrc>::open(path)?)),
        Format::Txt => Ok(Box::new(TxtDoc::open(path)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn from_path_detects_epub_case_insensitively() {
        assert_eq!(Format::from_path(Path::new("book.epub")).unwrap(), Format::Epub);
        assert_eq!(Format::from_path(Path::new("BOOK.EPUB")).unwrap(), Format::Epub);
        assert_eq!(Format::from_path(Path::new("/a/b/c.EpUb")).unwrap(), Format::Epub);
    }

    #[test]
    fn from_path_rejects_unknown_and_missing_extension() {
        assert!(Format::from_path(Path::new("book.docx")).is_err());
        assert!(Format::from_path(Path::new("noext")).is_err());
    }

    #[test]
    fn from_path_detects_subtitles_case_insensitively() {
        assert_eq!(Format::from_path(Path::new("a.srt")).unwrap(), Format::Srt);
        assert_eq!(Format::from_path(Path::new("A.SRT")).unwrap(), Format::Srt);
        assert_eq!(Format::from_path(Path::new("a.vtt")).unwrap(), Format::Vtt);
        assert_eq!(Format::from_path(Path::new("/x/y.Z.VtT")).unwrap(), Format::Vtt);
        assert_eq!(Format::from_path(Path::new("a.ass")).unwrap(), Format::Ass);
        assert_eq!(Format::from_path(Path::new("a.SSA")).unwrap(), Format::Ass);
        assert_eq!(Format::from_path(Path::new("a.lrc")).unwrap(), Format::Lrc);
        assert_eq!(Format::from_path(Path::new("lyrics.LRC")).unwrap(), Format::Lrc);
        assert_eq!(Format::from_path(Path::new("novel.txt")).unwrap(), Format::Txt);
        assert_eq!(Format::from_path(Path::new("BOOK.TXT")).unwrap(), Format::Txt);
    }
}
