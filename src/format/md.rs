//! Markdown backend (`.md` / `.markdown`).
//!
//! Markdown carries structure (frontmatter, code blocks, headings, lists,
//! links), so unlike TXT we group **blank-line-separated blocks** — a block is
//! the translation unit, sent to the model with its inline markup intact
//! (Hy-MT2 preserves simple markdown when translating prose). Whole-paragraph
//! coherence and clean bilingual output (original block, blank line, translated
//! block) beat per-line segmentation.
//!
//! A small state machine (no dependency, not a full CommonMark AST) classifies
//! each block:
//! - **Passthrough** (never translated): YAML/TOML frontmatter (`---`/`+++`
//!   fence at the very first line), fenced code blocks (``` / ~~~), horizontal
//!   rules, reference link definitions (`[id]: url`), and blank lines.
//! - **Text** (translatable): everything else — headings, paragraphs, lists,
//!   blockquotes, table rows. The block's markup (`#`, `-`, `>`, `|`, `**…**`,
//!   `[text](url)`) rides along.
//!
//! Prose is continuous, so [`MdDoc`] opts into [`crate::format::Strategy::Batched`].
//!
//! ## Limitations (v1)
//! Not a spec-complete parser: setext headings (`Text\n===`) and indented
//! (4-space) code blocks aren't specially recognized; inline code spans and
//! link URLs are translated best-effort (the model usually leaves code-like
//! tokens and URLs alone, but isn't guaranteed to). Code-heavy or
//! structurally dense docs may want `--mode replace` and/or a real AST pass
//! (`pulldown-cmark`) later.

use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct MdDoc {
    /// One entry per block, in document order.
    blocks: Vec<MdBlock>,
}

enum MdBlock {
    /// Emitted verbatim: frontmatter, fenced code, HR, ref defs, blanks.
    Passthrough(String),
    /// Translatable block (markup included).
    Text(String),
}

impl MdDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let src =
            fs::read_to_string(path).with_context(|| format!("read md {}", path.display()))?;
        let blocks = parse_md(&src);
        let translatable = blocks
            .iter()
            .filter(|b| matches!(b, MdBlock::Text(_)))
            .count();
        eprintln!(
            "md: {} block(s), {} translatable",
            blocks.len(),
            translatable
        );
        Ok(MdDoc { blocks })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Normal,
    Frontmatter,
    CodeFence,
}

/// Parse markdown source into blocks. See the module docs for the grammar.
fn parse_md(src: &str) -> Vec<MdBlock> {
    let norm = src.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = norm.split('\n').collect();

    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut cur: Vec<&str> = Vec::new(); // accumulating Normal block lines
    let mut sp: Vec<&str> = Vec::new(); // accumulating a passthrough block (fence/frontmatter)
    let mut state = State::Normal;
    let mut fence_char = '`'; // active code-fence char
    let mut fm_delim = "---"; // active frontmatter closing delimiter

    // Flush `cur` as a Normal block, classifying a single HR/ref-def line as
    // passthrough.
    macro_rules! flush_text {
        () => {
            if !cur.is_empty() {
                let passthrough =
                    cur.len() == 1 && (is_hr(cur[0].trim()) || is_ref_def(cur[0].trim()));
                let joined = cur.join("\n");
                blocks.push(if passthrough {
                    MdBlock::Passthrough(joined)
                } else {
                    MdBlock::Text(joined)
                });
                cur.clear();
            }
        };
    }
    // Flush `sp` as an unconditional passthrough block.
    macro_rules! flush_pass {
        () => {{
            if !sp.is_empty() {
                blocks.push(MdBlock::Passthrough(sp.join("\n")));
                sp.clear();
            }
        }};
    }

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        match state {
            State::Frontmatter => {
                sp.push(line);
                if trimmed == fm_delim {
                    flush_pass!();
                    state = State::Normal;
                }
                continue;
            }
            State::CodeFence => {
                sp.push(line);
                if fence_close(trimmed, fence_char) {
                    flush_pass!();
                    state = State::Normal;
                }
                continue;
            }
            State::Normal => {
                // Frontmatter opens only at the very first line.
                if i == 0 && (trimmed == "---" || trimmed == "+++") {
                    state = State::Frontmatter;
                    fm_delim = trimmed;
                    sp.push(line);
                    continue;
                }
                // Fenced code block opens.
                if let Some(c) = fence_open(trimmed) {
                    flush_text!();
                    state = State::CodeFence;
                    fence_char = c;
                    sp.push(line);
                    continue;
                }
                // Blank line: ends the current block; preserve the blank.
                if trimmed.is_empty() {
                    flush_text!();
                    blocks.push(MdBlock::Passthrough(String::new()));
                    continue;
                }
                // Structural single lines that aren't prose: HR / ref-def are
                // classified on flush, so just accumulate here like normal text.
                cur.push(line);
            }
        }
    }
    // EOF inside a code fence / frontmatter: flush what we have (passthrough).
    match state {
        State::Normal => flush_text!(),
        State::Frontmatter | State::CodeFence => flush_pass!(),
    }

    blocks
}

/// Serialize blocks back, substituting each Text block per `mode`. Bilingual
/// emits the original block, a blank line, then the translated block; the
/// original blank-line passthroughs between blocks are preserved, so the output
/// reads as parallel bilingual prose.
fn render_md(blocks: &[MdBlock], tr: &HashMap<usize, String>, mode: OutputMode) -> String {
    let mut out: Vec<String> = Vec::with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        match b {
            MdBlock::Passthrough(s) => out.push(s.clone()),
            MdBlock::Text(orig) => match tr.get(&i) {
                None => out.push(orig.clone()),
                Some(t) => match mode {
                    OutputMode::Replace => out.push(t.clone()),
                    OutputMode::Bilingual => {
                        if orig.is_empty() {
                            out.push(t.clone());
                        } else {
                            out.push(format!("{orig}\n\n{t}"));
                        }
                    }
                },
            },
        }
    }
    // No forced trailing newline: a file ending in '\n' yields a trailing ""
    // passthrough block, so join("\n") round-trips the file exactly.
    out.join("\n")
}

impl Document for MdDoc {
    fn format_name(&self) -> &'static str {
        "md"
    }

    fn segments(&self) -> Vec<Segment> {
        self.blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| match b {
                MdBlock::Text(t) if !t.trim().is_empty() => Some(Segment {
                    id: i,
                    text: t.clone(),
                }),
                _ => None,
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
        let rendered = render_md(&self.blocks, &map, mode);
        fs::write(out, rendered).with_context(|| format!("write md {}", out.display()))?;
        Ok(())
    }

    fn strategy(&self) -> Strategy {
        // Prose blocks benefit from cross-block context (names, tone,
        // references to earlier sections), so batch like subtitle cues / txt
        // lines. 25/context-5 suits typical doc paragraphs; a pathologically
        // large block fails only itself (original kept).
        Strategy::Batched {
            batch_size: 25,
            context: 5,
        }
    }
}

/// Is `s` a horizontal rule? CommonMark: 3+ of one of `-`, `*`, `_`, optionally
/// space-separated, nothing else.
fn is_hr(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let dash = s.chars().filter(|&c| c == '-').count();
    let star = s.chars().filter(|&c| c == '*').count();
    let und = s.chars().filter(|&c| c == '_').count();
    let only_marks = s
        .chars()
        .all(|c| c == ' ' || c == '-' || c == '*' || c == '_');
    only_marks
        && ((dash >= 3 && star == 0 && und == 0)
            || (star >= 3 && dash == 0 && und == 0)
            || (und >= 3 && dash == 0 && star == 0))
}

/// Is `s` a reference link definition, i.e. `[label]: url`?
fn is_ref_def(s: &str) -> bool {
    s.starts_with('[') && s.contains("]:")
}

/// If `s` opens a fenced code block, return the fence char (`` ` `` or `~`).
fn fence_open(s: &str) -> Option<char> {
    let c = s.chars().next()?;
    if (c == '`' || c == '~') && s.chars().take_while(|&x| x == c).count() >= 3 {
        Some(c)
    } else {
        None
    }
}

/// Does `s` close the active code fence (3+ of `c`)?
fn fence_close(s: &str, c: char) -> bool {
    s.chars().take_while(|&x| x == c).count() >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD_SAMPLE: &str = "\
---
title: My Doc
author: Me
---

# Introduction

This is a paragraph
spanning two lines.

- item one
- item two

```rust
let x = 1;
```

A final paragraph.
";

    fn text_blocks(blocks: &[MdBlock]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parse_passes_frontmatter_and_code_verbatim() {
        let blocks = parse_md(MD_SAMPLE);
        let pass: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Passthrough(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        // frontmatter intact
        assert!(pass
            .iter()
            .any(|s| s.contains("title: My Doc") && s.starts_with("---")));
        // code fence intact, body not split out
        assert!(pass
            .iter()
            .any(|s| s.contains("```rust") && s.contains("let x = 1;")));
    }

    #[test]
    fn parse_groups_prose_into_blocks() {
        let blocks = parse_md(MD_SAMPLE);
        let t = text_blocks(&blocks);
        // 4 translatable blocks: heading, 2-line paragraph, list, final paragraph
        // (blank lines separate them; the heading is NOT merged with the para).
        assert_eq!(t.len(), 4);
        assert_eq!(t[0], "# Introduction");
        assert_eq!(t[1], "This is a paragraph\nspanning two lines.");
        assert!(t[2].contains("- item one") && t[2].contains("- item two"));
        assert_eq!(t[3], "A final paragraph.");
    }

    #[test]
    fn parse_treats_hr_and_refdef_as_passthrough() {
        let src = "para one\n\n---\n\n[home]: https://x.test\n\npara two\n";
        let blocks = parse_md(src);
        let t = text_blocks(&blocks);
        assert_eq!(t, vec!["para one".to_string(), "para two".to_string()]);
        // HR and ref-def survived as passthrough.
        let pass: Vec<String> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::Passthrough(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(pass.iter().any(|s| s == "---"));
        assert!(pass.iter().any(|s| s == "[home]: https://x.test"));
    }

    #[test]
    fn roundtrip_unchanged_with_no_translations() {
        let blocks = parse_md(MD_SAMPLE);
        let out = render_md(&blocks, &HashMap::new(), OutputMode::Replace);
        assert_eq!(out, MD_SAMPLE);
    }

    #[test]
    fn render_replace_swaps_text_blocks_only() {
        let blocks = parse_md(MD_SAMPLE);
        // text blocks are at indices: find them.
        let mut tr = HashMap::new();
        for (i, b) in blocks.iter().enumerate() {
            if matches!(b, MdBlock::Text(_)) {
                tr.insert(i, format!("[TR {i}]"));
            }
        }
        let out = render_md(&blocks, &tr, OutputMode::Replace);
        assert!(out.contains("[TR")); // translations present
                                      // frontmatter + code still verbatim.
        assert!(out.contains("title: My Doc"));
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn render_bilingual_emits_block_then_translation() {
        let blocks = parse_md(MD_SAMPLE);
        let mut tr = HashMap::new();
        for (i, b) in blocks.iter().enumerate() {
            if matches!(b, MdBlock::Text(_)) {
                tr.insert(i, format!("[TR {i}]"));
            }
        }
        let out = render_md(&blocks, &tr, OutputMode::Bilingual);
        // final paragraph block + its translation, blank-separated.
        assert!(out.contains("A final paragraph.\n\n[TR"));
        // original heading still there (bilingual keeps it).
        assert!(out.contains("# Introduction"));
    }

    #[test]
    fn is_hr_and_refdef_classify_correctly() {
        assert!(is_hr("---"));
        assert!(is_hr("***"));
        assert!(is_hr("* * *"));
        assert!(is_hr("___"));
        assert!(!is_hr("--")); // too short
        assert!(!is_hr("-*-")); // mixed
        assert!(!is_hr("text"));
        assert!(is_ref_def("[home]: https://x.test"));
        assert!(is_ref_def("[a]: <u>"));
        assert!(!is_ref_def("[link](url)")); // inline link, not a def
        assert!(!is_ref_def("plain text"));
    }

    #[test]
    fn fence_open_detects_backtick_and_tilde() {
        assert_eq!(fence_open("```rust"), Some('`'));
        assert_eq!(fence_open("~~~~"), Some('~'));
        assert_eq!(fence_open("``"), None); // too short
        assert_eq!(fence_open("text"), None);
        assert!(fence_close("```", '`'));
        assert!(fence_close("````", '`'));
        assert!(!fence_close("``", '`'));
    }
}
