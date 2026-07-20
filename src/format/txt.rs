//! Plain-text backend for prose (novels, scripts, transcripts).
//!
//! TXT has no markup, so the unit of translation is the **line**: each
//! non-blank, non-structural line is one segment. Structural lines — blanks
//! (paragraph breaks) and anything starting with `[` (the cataloging header
//! `['id','title',…]` many ASR/novel dumps carry, `[chapter:…]` markers, etc.)
//! — pass through verbatim so the file's shape survives byte-exact.
//!
//! Prose is continuous, so [`TxtDoc`] opts into
//! [`crate::format::Strategy::Batched`]: consecutive lines go out behind one
//! prompt with a few lines of context, giving the model the surrounding flow
//! (pronouns, names, tone) the way subtitle cues get it.

use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct TxtDoc {
    /// One entry per source line, in document order.
    lines: Vec<TxtLine>,
}

enum TxtLine {
    /// Emitted verbatim: blank lines and structural lines (start with `[`).
    Passthrough(String),
    /// Translatable prose.
    Text(String),
}

impl TxtDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let src = fs::read_to_string(path)
            .with_context(|| format!("read txt {}", path.display()))?;
        let lines = parse_txt(&src);
        let translatable = lines.iter().filter(|l| matches!(l, TxtLine::Text(_))).count();
        eprintln!("txt: {} line(s), {} translatable", lines.len(), translatable);
        Ok(TxtDoc { lines })
    }
}

/// Split source text into lines, classifying each as translatable prose or
/// passthrough (blank, or starting with `[` — catalog header / `[chapter:…]`).
fn parse_txt(src: &str) -> Vec<TxtLine> {
    let norm = src.replace("\r\n", "\n").replace('\r', "\n");
    norm.split('\n')
        .map(|line| {
            if line.trim().is_empty() || line.trim_start().starts_with('[') {
                TxtLine::Passthrough(line.to_string())
            } else {
                TxtLine::Text(line.to_string())
            }
        })
        .collect()
}

/// Serialize lines back, substituting each prose line's text per `mode`.
fn render_txt(lines: &[TxtLine], tr: &HashMap<usize, String>, mode: OutputMode) -> String {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        match l {
            TxtLine::Passthrough(s) => out.push(s.clone()),
            TxtLine::Text(orig) => match tr.get(&i) {
                None => out.push(orig.clone()),
                Some(t) => match mode {
                    OutputMode::Replace => out.push(t.clone()),
                    // Bilingual: original line, then its translation on the next
                    // line. Blank passthrough lines between paragraphs survive.
                    OutputMode::Bilingual => {
                        out.push(orig.clone());
                        out.push(t.clone());
                    }
                },
            },
        }
    }
    // No forced trailing newline: a file ending in '\n' splits to a trailing ""
    // passthrough line, so join("\n") round-trips the file exactly.
    out.join("\n")
}

impl Document for TxtDoc {
    fn format_name(&self) -> &'static str {
        "txt"
    }

    fn segments(&self) -> Vec<Segment> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| match l {
                TxtLine::Text(t) => Some(Segment { id: i, text: t.clone() }),
                TxtLine::Passthrough(_) => None,
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
        let rendered = render_txt(&self.lines, &map, mode);
        fs::write(out, rendered).with_context(|| format!("write txt {}", out.display()))?;
        Ok(())
    }

    fn strategy(&self) -> Strategy {
        // Prose (novels, scripts) benefits from cross-line context, so batch
        // consecutive lines like subtitle cues. One-sentence-per-line novels
        // keep the unit small enough that 25/context-5 stays inside the context
        // window; a line that overflows fails only itself (original kept).
        Strategy::Batched {
            batch_size: 25,
            context: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXT_SAMPLE: &str = "\
['12345', 'Title', 'desc', ['tag'], 'author', 0]
第一章 序章

这是一段文字。
第二行继续。

[chapter:第二章]
新章节开始。
";

    #[test]
    fn parse_marks_structural_lines_passthrough() {
        let lines = parse_txt(TXT_SAMPLE);
        // 0: catalog header (starts with [) -> passthrough
        assert!(matches!(lines[0], TxtLine::Passthrough(_)));
        // 1: chapter heading text -> translatable
        assert!(matches!(lines[1], TxtLine::Text(_)));
        // 2: blank -> passthrough
        assert!(matches!(lines[2], TxtLine::Passthrough(_)));
        // 6: [chapter:...] -> passthrough
        assert!(matches!(lines[6], TxtLine::Passthrough(_)));
        // 7: prose -> translatable
        assert!(matches!(lines[7], TxtLine::Text(_)));
    }

    #[test]
    fn parse_counts_translatable() {
        let lines = parse_txt(TXT_SAMPLE);
        let n = lines
            .iter()
            .filter(|l| matches!(l, TxtLine::Text(_)))
            .count();
        // lines 1, 3, 4, 7 are prose.
        assert_eq!(n, 4);
    }

    #[test]
    fn roundtrip_unchanged_with_no_translations() {
        let lines = parse_txt(TXT_SAMPLE);
        let out = render_txt(&lines, &HashMap::new(), OutputMode::Replace);
        assert_eq!(out, TXT_SAMPLE);
    }

    #[test]
    fn render_bilingual_interleaves_translation() {
        let lines = parse_txt(TXT_SAMPLE);
        // translatable indices: 1, 3, 4, 7
        let mut tr = HashMap::new();
        tr.insert(3, "This is text.".to_string());
        let out = render_txt(&lines, &tr, OutputMode::Bilingual);
        assert!(out.contains("这是一段文字。\nThis is text."));
        // untranslated prose stays single.
        assert!(out.contains("第二行继续。"));
    }

    #[test]
    fn render_replace_swaps_prose_only() {
        let lines = parse_txt(TXT_SAMPLE);
        let mut tr = HashMap::new();
        tr.insert(7, "New chapter begins.".to_string());
        let out = render_txt(&lines, &tr, OutputMode::Replace);
        assert!(out.contains("New chapter begins."));
        // structural lines untouched.
        assert!(out.contains("[chapter:第二章]"));
        assert!(out.contains("['12345', 'Title'"));
    }
}
