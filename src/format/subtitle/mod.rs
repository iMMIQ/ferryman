//! Subtitle backend family — SRT, VTT, and (later) ASS/LRC.
//!
//! Subtitles are NOT translated cue-by-cue: each cue is short and only makes
//! sense in the flow around it, so sending one line per request would starve
//! the model of context and produce disjoint output. Instead the format opts
//! into [`crate::format::Strategy::Batched`] (see [`Document::strategy`]), and
//! the engine batches consecutive cues behind ONE prompt with a `<cN>` delimiter
//! alignment scheme (validated on Hy-MT2) that guarantees a strict **one-to-one
//! correspondence**: the model returns exactly as many translations as it
//! received, in order, never merging or splitting. See
//! [`crate::translate::translate_batch`].
//!
//! ## Format abstraction
//!
//! Every subtitle grammar implements [`SubFormat`]. SRT and VTT share the same
//! "timed cue" grammar — a block whose line containing `-->` is the timing,
//! the lines after it are the text, everything before is the cue index /
//! identifier — so both delegate to the shared [`parse_timed`] / [`render_timed`]
//! helpers here. The trait is the extension point for genuinely different
//! grammars (ASS, LRC): override [`SubFormat::parse`] / [`SubFormat::render`]
//! and the rest of the pipeline keeps working unchanged.

use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::marker::PhantomData;
use std::path::Path;

pub mod ass;
pub mod lrc;
pub mod srt;
pub mod vtt;

/// One subtitle cue as parsed from the source.
///
/// The translatable payload is [`Cue::text`] (the lines after the timing line;
/// internal newlines preserved). [`Cue::framing`] holds everything else — the
/// cue index / identifier and the timing line — verbatim, so [`render_timed`]
/// can reconstruct the cue byte-exactly and a translation only ever replaces
/// the text. Passthrough blocks (a VTT `WEBVTT` header, `NOTE`/`STYLE`/`REGION`
/// blocks, or anything without a `-->` line) carry no text and are re-emitted
/// unchanged.
#[derive(Clone, Debug)]
pub struct Cue {
    /// Lines after the timing line (joined with `\n`, internal newlines kept).
    /// Empty for passthrough blocks.
    pub text: String,
    /// Everything from the block start through (and including) the timing line,
    /// with NO trailing newline; the text is appended after it on write. For a
    /// passthrough block this is the whole block, again with no trailing newline.
    pub framing: String,
    /// `true` if the block has a timing line (a real, translatable cue); `false`
    /// for headers / NOTEs / STYLEs / anything without `-->`.
    pub timed: bool,
}

impl Cue {
    /// A verbatim line emitted unchanged — headers, `Format:`/`Comment:` lines,
    /// metadata tags, blanks. Shared by the line-oriented grammars (ASS, LRC).
    /// Private to this module, but visible to the grammar submodules (ass, lrc).
    fn passthrough(line: &str) -> Cue {
        Cue {
            text: String::new(),
            framing: line.to_string(),
            timed: false,
        }
    }
}

/// A subtitle grammar: how to split a file into [`Cue`]s and how to stitch them
/// back. Default methods use the shared timed-cue grammar; override for a
/// divergent format.
pub trait SubFormat: Sized {
    /// Lowercase name for logs, e.g. `"srt"`, `"vtt"`.
    const NAME: &'static str;

    /// Parse the raw source into cues (translatable + passthrough) in document
    /// order. Default: the `-->`-anchored timed-cue grammar ([`parse_timed`]).
    fn parse(src: &str) -> Result<Vec<Cue>> {
        parse_timed(src)
    }

    /// Serialize all cues back to the full file text, substituting each
    /// translatable cue's text per `mode` and emitting passthrough cues
    /// verbatim. `translations` is keyed by cue index (`SegmentId` == position
    /// in the parsed `Vec<Cue>`). Default: [`render_timed`].
    fn render(
        cues: &[Cue],
        translations: &HashMap<usize, String>,
        mode: OutputMode,
    ) -> String {
        render_timed(cues, translations, mode)
    }
}

/// Parse a timed-cue subtitle file (SRT or VTT) into [`Cue`]s.
///
/// Splits the source into blocks on blank lines (robust to CRLF and multiple
/// consecutive blank lines). A block with a `-->` line becomes a translatable
/// cue (timing line and anything before it = framing; lines after = text).
/// Any other block (VTT `WEBVTT` header, `NOTE`/`STYLE`/`REGION`, stray text)
/// becomes a passthrough cue emitted byte-for-byte on write.
fn parse_timed(src: &str) -> Result<Vec<Cue>> {
    // Normalize line endings first; SRT/VTT files are often CRLF on Windows.
    let norm = src.replace("\r\n", "\n").replace('\r', "\n");

    let mut cues = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    for line in norm.split('\n') {
        if line.trim().is_empty() {
            if let Some(cue) = block_to_cue(&block) {
                cues.push(cue);
            }
            block.clear();
        } else {
            block.push(line);
        }
    }
    if let Some(cue) = block_to_cue(&block) {
        cues.push(cue);
    }
    Ok(cues)
}

/// Build one [`Cue`] from a non-empty run of consecutive non-blank lines.
/// Returns `None` for an all-blank run (defensive; the splitter already skips
/// those).
fn block_to_cue(lines: &[&str]) -> Option<Cue> {
    if lines.is_empty() || lines.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    if let Some(tidx) = lines.iter().position(|l| l.contains("-->")) {
        // Real cue: framing = index/identifier + timing line; text = the rest.
        let text = lines[tidx + 1..].join("\n");
        let framing = lines[..=tidx].join("\n");
        Some(Cue {
            text,
            framing,
            timed: true,
        })
    } else {
        // No timing line → WEBVTT header / NOTE / STYLE / REGION / junk: pass through.
        Some(Cue {
            text: String::new(),
            framing: lines.join("\n"),
            timed: false,
        })
    }
}

/// Render cues back to a file. Translatable cues with a translation get
/// `framing + text` where text follows `mode` (`replace` = translation only,
/// `bilingual` = original then translation); untranslated cues keep their
/// original text; passthrough cues emit verbatim. Blocks are joined by a blank
/// line with a single trailing newline (SRT/VTT convention).
fn render_timed(cues: &[Cue], translations: &HashMap<usize, String>, mode: OutputMode) -> String {
    let mut blocks: Vec<String> = Vec::with_capacity(cues.len());
    for (i, c) in cues.iter().enumerate() {
        if !c.timed {
            blocks.push(c.framing.clone());
            continue;
        }
        let rendered = match translations.get(&i) {
            None => {
                // Untranslated (limit / failed / cancelled): keep the original.
                if c.text.is_empty() {
                    c.framing.clone()
                } else {
                    format!("{}\n{}", c.framing, c.text)
                }
            }
            Some(tr) => {
                let body = match mode {
                    OutputMode::Replace => tr.clone(),
                    OutputMode::Bilingual => {
                        if c.text.is_empty() {
                            tr.clone()
                        } else {
                            format!("{}\n{}", c.text, tr)
                        }
                    }
                };
                format!("{}\n{}", c.framing, body)
            }
        };
        blocks.push(rendered);
    }
    blocks.join("\n\n") + "\n"
}

/// A parsed subtitle document. Generic over the [`SubFormat`] so SRT and VTT
/// (and future formats) reuse one [`Document`] implementation.
pub struct SubtitleDoc<F: SubFormat> {
    cues: Vec<Cue>,
    _fmt: PhantomData<F>,
}

impl<F: SubFormat> SubtitleDoc<F> {
    pub fn open(path: &Path) -> Result<Self> {
        let src = fs::read_to_string(path)
            .with_context(|| format!("read subtitle {}", path.display()))?;
        let cues = F::parse(&src)?;
        let translatable = cues
            .iter()
            .filter(|c| c.timed && !c.text.trim().is_empty())
            .count();
        eprintln!(
            "{}: {} cue(s), {} translatable",
            F::NAME,
            cues.len(),
            translatable
        );
        Ok(SubtitleDoc {
            cues,
            _fmt: PhantomData,
        })
    }
}

impl<F: SubFormat> Document for SubtitleDoc<F> {
    fn format_name(&self) -> &'static str {
        F::NAME
    }

    /// One segment per translatable cue (timed + non-empty text). `id` is the
    /// cue's index in the full `Vec<Cue>` (NOT dense — passthrough cues leave
    /// gaps); the engine treats ids as opaque and [`Document::write`] maps them
    /// straight back to cue positions.
    fn segments(&self) -> Vec<Segment> {
        self.cues
            .iter()
            .enumerate()
            .filter(|(_, c)| c.timed && !c.text.trim().is_empty())
            .map(|(i, c)| Segment {
                id: i,
                text: c.text.clone(),
            })
            .collect()
    }

    fn write(
        &mut self,
        translations: &[(SegmentId, String)],
        out: &Path,
        mode: OutputMode,
    ) -> Result<()> {
        let map: HashMap<usize, String> = translations.iter().cloned().collect();
        let rendered = F::render(&self.cues, &map, mode);
        fs::write(out, rendered).with_context(|| format!("write subtitle {}", out.display()))?;
        Ok(())
    }

    /// Subtitles translate in contextual batches, not independently — see the
    /// module docs. Defaults match the validated Hy-MT2 settings; the CLI may
    /// override the parameters.
    fn strategy(&self) -> Strategy {
        Strategy::Batched {
            batch_size: 25,
            context: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT_SAMPLE: &str = "\
1
00:00:01,000 --> 00:00:02,000
Hello world.

2
00:00:02,500 --> 00:00:04,000
This is a
multi-line cue.
";

    const VTT_SAMPLE: &str = "\
WEBVTT

00:00:01.000 --> 00:00:02.000
Hello world.

NOTE this is a comment

00:00:03.000 --> 00:00:04.000
Second cue.
";

    #[test]
    fn parse_srt_into_timed_cues() {
        let cues = parse_timed(SRT_SAMPLE).unwrap();
        assert_eq!(cues.len(), 2);
        assert!(cues[0].timed);
        assert_eq!(cues[0].framing, "1\n00:00:01,000 --> 00:00:02,000");
        assert_eq!(cues[0].text, "Hello world.");
        assert_eq!(cues[1].framing, "2\n00:00:02,500 --> 00:00:04,000");
        assert_eq!(cues[1].text, "This is a\nmulti-line cue.");
    }

    #[test]
    fn parse_vtt_handles_header_and_note_as_passthrough() {
        let cues = parse_timed(VTT_SAMPLE).unwrap();
        assert_eq!(cues.len(), 4);
        // WEBVTT header — passthrough.
        assert!(!cues[0].timed);
        assert_eq!(cues[0].framing, "WEBVTT");
        assert!(cues[0].text.is_empty());
        // First real cue.
        assert!(cues[1].timed);
        assert_eq!(cues[1].text, "Hello world.");
        assert_eq!(cues[1].framing, "00:00:01.000 --> 00:00:02.000");
        // NOTE — passthrough.
        assert!(!cues[2].timed);
        assert_eq!(cues[2].framing, "NOTE this is a comment");
        // Second real cue.
        assert!(cues[3].timed);
        assert_eq!(cues[3].text, "Second cue.");
    }

    #[test]
    fn render_replace_swaps_only_text() {
        let cues = parse_timed(SRT_SAMPLE).unwrap();
        let mut tr = HashMap::new();
        tr.insert(0, "你好世界".to_string());
        tr.insert(1, "多行\n字幕".to_string());
        let out = render_timed(&cues, &tr, OutputMode::Replace);
        assert_eq!(
            out,
            "\
1
00:00:01,000 --> 00:00:02,000
你好世界

2
00:00:02,500 --> 00:00:04,000
多行
字幕
"
        );
    }

    #[test]
    fn render_bilingual_appends_translation() {
        let cues = parse_timed(SRT_SAMPLE).unwrap();
        let mut tr = HashMap::new();
        tr.insert(0, "你好".to_string());
        let out = render_timed(&cues, &tr, OutputMode::Bilingual);
        // cue 0 keeps original + adds translation; cue 1 (no translation) stays original.
        assert!(out.contains("Hello world.\n你好"));
        assert!(out.contains("This is a\nmulti-line cue."));
        assert!(!out.contains("This is a\nmulti-line cue.\n多行"));
    }

    #[test]
    fn render_vtt_preserves_passthrough_blocks() {
        let cues = parse_timed(VTT_SAMPLE).unwrap();
        let mut tr = HashMap::new();
        tr.insert(1, "你好".to_string());
        tr.insert(3, "第二".to_string());
        let out = render_timed(&cues, &tr, OutputMode::Replace);
        assert!(out.starts_with("WEBVTT\n\n"));
        assert!(out.contains("NOTE this is a comment"));
        assert!(out.contains("00:00:01.000 --> 00:00:02.000\n你好"));
        assert!(out.contains("00:00:03.000 --> 00:00:04.000\n第二"));
    }

    #[test]
    fn segments_skip_passthrough_and_empty() {
        // cue index 0 = WEBVTT (passthrough), 1 = real, 2 = NOTE, 3 = real.
        let doc = SubtitleDoc::<vtt::Vtt> {
            cues: parse_timed(VTT_SAMPLE).unwrap(),
            _fmt: PhantomData,
        };
        let segs = doc.segments();
        assert_eq!(segs.len(), 2);
        // ids are cue indices (gaps where passthrough sits), in document order.
        assert_eq!(segs[0].id, 1);
        assert_eq!(segs[0].text, "Hello world.");
        assert_eq!(segs[1].id, 3);
        assert_eq!(segs[1].text, "Second cue.");
        // batched strategy requested.
        assert!(matches!(doc.strategy(), Strategy::Batched { .. }));
    }

    #[test]
    fn parse_handles_crlf_and_extra_blanks() {
        let src = "1\r\n00:00:01,000 --> 00:00:02,000\r\nA.\r\n\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nB.\r\n";
        let cues = parse_timed(src).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "A.");
        assert_eq!(cues[1].text, "B.");
    }

    #[test]
    fn parse_empty_source_yields_no_cues() {
        assert!(parse_timed("").unwrap().is_empty());
        assert!(parse_timed("\n\n\n").unwrap().is_empty());
    }
}
