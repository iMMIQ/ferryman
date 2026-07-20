//! Format-agnostic document abstraction.
//!
//! Each input format (EPUB, SRT, VTT today; txt / docx later) implements
//! [`Document`]: it parses the file into translatable [`Segment`]s (plain
//! text) and knows how to write translations back. The translation engine
//! ([`crate::engine`]) is format-agnostic — with one caveat: subtitle formats
//! set [`Document::batched`] so the engine translates their cues in contextual
//! batches rather than independently (see [`crate::engine`]).

use crate::format::epub::EpubDoc;
use crate::format::subtitle::ass::Ass;
use crate::format::subtitle::srt::Srt;
use crate::format::subtitle::vtt::Vtt;
use crate::format::subtitle::SubtitleDoc;
use anyhow::{anyhow, Result};
use std::path::Path;

pub mod epub;
pub mod subtitle;

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

    /// Whether this document's segments must be translated in contiguous,
    /// context-carrying batches rather than independently. `false` (the
    /// default) suits formats whose segments are self-contained blocks (EPUB).
    /// Subtitle formats override to `true`: their cues are short and only make
    /// sense in the surrounding flow, so the engine batches them behind one
    /// prompt and aligns the result strictly one-to-one (no merge/split). See
    /// [`crate::engine::Engine::translate_subtitles`].
    fn batched(&self) -> bool {
        false
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
    // TODO: Txt, Docx, …
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
            // TODO: "txt" | "md" => Ok(Format::Txt), …
            other => Err(anyhow!(
                "unsupported input format {:?} ({}); supported: epub, srt, vtt, ass",
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
        assert!(Format::from_path(Path::new("book.txt")).is_err());
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
    }
}
