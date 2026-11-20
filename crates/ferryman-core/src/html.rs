//! HTML extraction/rewriting using lol_html.
//!
//! Strategy: lol_html is a streaming HTML rewriter that preserves the original
//! byte stream and only mutates what we target — so source formatting is kept
//! intact. We make a single rewrite pass that:
//!   1. Injects a `<style>` block for `.hy-zh` into `<head>`.
//!   2. Tracks "leaf" block elements (p, headings, li, ...). A block is a leaf
//!      if it contains no other tracked block; containers are skipped so we
//!      don't double-translate nested content.
//!   3. Accumulates the plain-text content of each leaf block.
//!   4. Inserts a unique placeholder comment `<!--HYZH:{id}-->` right after each
//!      leaf block's end tag.
//!
//! Translation is async, so we collect texts now and replace the placeholders
//! afterwards (see [`apply_translations`]).

use anyhow::Result;
use lol_html::html_content::ContentType;
use lol_html::{element, end_tag, rewrite_str, text, RewriteStrSettings};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use unicode_segmentation::UnicodeSegmentation;

/// CSS injected into every content document to style the appended translations.
pub const STYLE_HTML: &str = concat!(
    "<style type=\"text/css\">",
    ".hy-zh{display:block;margin:.4em 0 .9em;padding:.15em 0 .15em .7em;",
    "border-left:2px solid #8aa;color:#3a3a3a;font-size:.96em;line-height:1.5;",
    "}",
    "</style>"
);

/// CSS selector for block elements we consider translation units.
const BLOCK_SEL: &str = "p, h1, h2, h3, h4, h5, h6, li, blockquote, figcaption, dt, dd";

/// Elements whose text content must NOT be collected (head metadata, code, etc.).
const SUPPRESS_SEL: &str = "head, title, script, style, pre, code, nav, textarea, svg";

#[derive(Default, Clone)]
pub struct Block {
    pub text: String,
    pub tag: String,
    /// Set true for leaf blocks that carry non-empty text and got a placeholder.
    pub leaf: bool,
    /// Original text-node byte ranges, excluding protected content and markup.
    pub text_ranges: Vec<Range<usize>>,
}

struct State {
    blocks: Vec<Block>,
    /// Stack of (block id, has_nested_tracked_child).
    stack: Vec<(usize, bool)>,
    /// >0 while inside a suppressed region.
    suppress: i32,
}

impl State {
    fn new() -> Self {
        State {
            blocks: Vec::new(),
            stack: Vec::new(),
            suppress: 0,
        }
    }
}

/// First pass: rewrite `html` inserting placeholders + style, and collect the
/// leaf blocks to translate. Returns (rewritten_html, blocks).
pub fn extract(html: &str) -> Result<(String, Vec<Block>)> {
    let st = Rc::new(RefCell::new(State::new()));
    let st_sup = st.clone();
    let st_blk = st.clone();
    let st_txt = st.clone();
    let st_break = st.clone();

    let rewritten = rewrite_str(
        html,
        vec![
            // Suppress collection inside non-content regions.
            element!(SUPPRESS_SEL, move |el| {
                st_sup.borrow_mut().suppress += 1;
                let s = st_sup.clone();
                let _ = el.on_end_tag(end_tag!(move |_| {
                    s.borrow_mut().suppress -= 1;
                    Ok(())
                }));
                Ok(())
            }),
            // Track block elements.
            element!(BLOCK_SEL, move |el| {
                let mut s = st_blk.borrow_mut();
                if s.suppress > 0 {
                    return Ok(());
                }
                // Mark the enclosing tracked block as a container.
                if let Some(top) = s.stack.last_mut() {
                    top.1 = true;
                }
                let tag = el.tag_name().to_string();
                let id = s.blocks.len();
                s.blocks.push(Block {
                    text: String::new(),
                    tag,
                    leaf: false,
                    text_ranges: Vec::new(),
                });
                s.stack.push((id, false));

                let s2 = st_blk.clone();
                let _ = el.on_end_tag(end_tag!(move |end| {
                    let (id, is_leaf) = match s2.borrow_mut().stack.pop() {
                        Some((id, nested)) => (id, !nested),
                        None => return Ok(()),
                    };
                    let has_text = !s2.borrow().blocks[id].text.trim().is_empty();
                    if is_leaf && has_text {
                        s2.borrow_mut().blocks[id].leaf = true;
                        end.after(&format!("<!--HYZH:{}-->", id), ContentType::Html);
                    }
                    Ok(())
                }));
                Ok(())
            }),
            element!("br", move |_| {
                let mut s = st_break.borrow_mut();
                if s.suppress == 0 {
                    if let Some(&(id, _)) = s.stack.last() {
                        s.blocks[id].text.push('\n');
                    }
                }
                Ok(())
            }),
            // Inject the translation style into <head>.
            element!("head", |el| {
                el.append(STYLE_HTML, ContentType::Html);
                Ok(())
            }),
            // Accumulate text content of blocks (scoped to <body>, so <head>
            // metadata is naturally excluded).
            text!("body", move |tc| {
                let mut s = st_txt.borrow_mut();
                if s.suppress > 0 {
                    return Ok(());
                }
                if let Some(top) = s.stack.last() {
                    let id = top.0;
                    s.blocks[id].text.push_str(tc.as_str());
                    let range = tc.source_location().bytes();
                    if !range.is_empty() {
                        let ranges = &mut s.blocks[id].text_ranges;
                        // Streaming chunks can split a text node or an entity.
                        // Merge them before assigning translated text to runs.
                        if let Some(last) = ranges.last_mut().filter(|last| last.end == range.start)
                        {
                            last.end = range.end;
                        } else {
                            ranges.push(range);
                        }
                    }
                }
                Ok(())
            }),
        ]
        .into_iter()
        .fold(RewriteStrSettings::new(), |settings, handler| {
            settings.append_element_content_handler(handler)
        }),
    )?;

    let mut blocks = Rc::try_unwrap(st).ok().unwrap().into_inner().blocks;
    // Decode after collecting all chunks so a split entity stays intact.
    for block in &mut blocks {
        block.text = html_escape::decode_html_entities(&block.text).into_owned();
    }
    Ok((rewritten, blocks))
}

/// Replace translated leaf text while retaining the original XHTML bytes for
/// tags, links, images, attributes, protected code and untranslated blocks.
/// Without word alignment, inline styles are retained by distributing target
/// graphemes proportionally across the original text runs.
pub fn replace_translations(
    source: &str,
    blocks: &[Block],
    translations: &[(usize, String)],
) -> String {
    let translations: HashMap<_, _> = translations.iter().map(|(id, text)| (*id, text)).collect();
    let mut edits = Vec::new();
    for (id, block) in blocks.iter().enumerate().filter(|(_, block)| block.leaf) {
        let Some(translation) = translations.get(&id) else {
            continue;
        };
        let runs: Vec<_> = block
            .text_ranges
            .iter()
            .map(|range| {
                let decoded = html_escape::decode_html_entities(&source[range.clone()]);
                let weight = if decoded.trim().is_empty() {
                    0
                } else {
                    decoded.graphemes(true).count()
                };
                (range, weight)
            })
            .collect();
        let total: usize = runs.iter().map(|(_, weight)| weight).sum();
        if total == 0 {
            continue;
        }
        let mut boundaries: Vec<_> = translation
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .collect();
        boundaries.push(translation.len());
        let mut consumed = 0;
        let mut start = 0;
        for (range, weight) in runs {
            consumed += weight;
            let end =
                ((consumed as u128 * (boundaries.len() - 1) as u128) / total as u128) as usize;
            let text = html_escape(&translation[boundaries[start]..boundaries[end]]);
            edits.push((range.clone(), text));
            start = end;
        }
    }
    edits.sort_unstable_by_key(|(range, _)| range.start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        output.push_str(&source[cursor..range.start]);
        output.push_str(&replacement);
        cursor = range.end;
    }
    output.push_str(&source[cursor..]);
    output
}

/// Second pass: replace each placeholder with a styled, escaped translation.
/// `translations` maps block id -> translated text. Any placeholder not
/// covered (e.g. due to `--limit` or a failed request) is removed so no
/// marker comments leak into the output.
pub fn apply_translations(
    html: &str,
    blocks: &[Block],
    translations: &[(usize, String)],
) -> String {
    let mut out = html.to_string();
    for (id, tr) in translations {
        let tag = blocks.get(*id).map(|b| b.tag.as_str()).unwrap_or("p");
        let wrapper_tag = match tag {
            "li" | "dt" | "dd" => tag,
            _ => "p",
        };
        let esc = html_escape(tr);
        let replacement = format!(
            "<{wt} class=\"hy-zh\">{esc}</{wt}>",
            wt = wrapper_tag,
            esc = esc
        );
        out = out.replace(&format!("<!--HYZH:{}-->", id), &replacement);
    }
    strip_leftover_placeholders(&out)
}

/// Remove any `<!--HYZH:...-->` markers that were never replaced.
fn strip_leftover_placeholders(s: &str) -> String {
    const MARKER: &str = "<!--HYZH:";
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(MARKER) {
            match s[i..].find("-->") {
                Some(end) => {
                    i += end + 3;
                    continue;
                }
                None => break,
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_keeps_markup_assets_and_untranslated_blocks() {
        let source = concat!(
            "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">",
            "<head><title>Book title</title></head><body><div>",
            "<p id=\"p1\" class=\"body\">Hello <em>world</em> &amp; <a href=\"#note\">friends</a>",
            "<img src=\"figure.png\" alt=\"figure\"/></p>",
            "<p id=\"note\">Untranslated <b>note</b>.</p>",
            "<p>Keep <code>x &lt; y</code> code.</p>",
            "</div></body></html>"
        );
        let (_, blocks) = extract(source).unwrap();
        let ids: Vec<_> = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.leaf)
            .map(|(id, _)| id)
            .collect();
        let translation = "你好，世界与朋友们<&>";
        let output = replace_translations(source, &blocks, &[(ids[0], translation.into())]);
        let doc = roxmltree::Document::parse(&output).unwrap();
        let paragraph = doc
            .descendants()
            .find(|n| n.attribute("id") == Some("p1"))
            .unwrap();
        let text: String = paragraph
            .descendants()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect();
        assert_eq!(text, translation);
        assert!(output.contains("<em>") && output.contains("<a href=\"#note\">"));
        assert!(output.contains("<img src=\"figure.png\" alt=\"figure\"/>"));
        assert!(output.contains("<p id=\"note\">Untranslated <b>note</b>.</p>"));
        assert!(output.contains("<p>Keep <code>x &lt; y</code> code.</p>"));
        assert!(!output.contains("Hello") && !output.contains("HYZH") && !output.contains("hy-zh"));
        assert_eq!(replace_translations(source, &blocks, &[]), source);
    }

    #[test]
    fn replace_handles_large_entity_runs_and_keeps_graphemes_whole() {
        let original = "a &amp; b ".repeat(2000);
        let source = format!("<html><body><p>{original}<b>tail</b></p></body></html>");
        let (_, blocks) = extract(&source).unwrap();
        let translation = "👩🏽‍💻e\u{301}中文".repeat(300);
        let output = replace_translations(&source, &blocks, &[(0, translation.clone())]);
        let doc = roxmltree::Document::parse(&output).unwrap();
        let text: String = doc
            .descendants()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .collect();
        assert_eq!(text, translation);
        let boundaries: std::collections::HashSet<_> = translation
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain([translation.len()])
            .collect();
        let mut end = 0;
        for node in doc.descendants().filter(|n| n.is_text()) {
            end += node.text().unwrap().len();
            assert!(boundaries.contains(&end));
        }
    }

    #[test]
    fn extracted_text_decodes_entities_and_preserves_breaks() {
        let (_, blocks) =
            extract("<html><body><p>Tom &amp; Jerry&lt;3 &#x4E2D;<br/>world</p></body></html>")
                .unwrap();
        assert_eq!(
            blocks.iter().find(|b| b.leaf).unwrap().text,
            "Tom & Jerry<3 中\nworld"
        );
    }

    #[test]
    fn html_escape_replaces_entities() {
        assert_eq!(html_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(html_escape("plain text"), "plain text");
        assert_eq!(html_escape("你好"), "你好");
        assert_eq!(html_escape(""), "");
    }

    #[test]
    fn html_escape_leaves_quotes_alone() {
        // Quotes aren't escaped — they only matter in attributes, and the
        // translation lands in element content.
        assert_eq!(html_escape(r#"she said "hi""#), r#"she said "hi""#);
    }

    #[test]
    fn strip_removes_markers() {
        assert_eq!(strip_leftover_placeholders("a<!--HYZH:5-->b"), "ab");
        assert_eq!(
            strip_leftover_placeholders("<!--HYZH:0--><!--HYZH:1-->"),
            ""
        );
    }

    #[test]
    fn strip_preserves_unrelated_content() {
        // A bare `-->` that isn't part of a HYZH marker must survive.
        assert_eq!(strip_leftover_placeholders("x-->y"), "x-->y");
        assert_eq!(
            strip_leftover_placeholders("no markers here"),
            "no markers here"
        );
    }

    #[test]
    fn apply_inserts_translation() {
        let html = "<p>orig</p><!--HYZH:0-->";
        let blocks = vec![Block {
            tag: "p".into(),
            ..Default::default()
        }];
        let out = apply_translations(html, &blocks, &[(0, "你好".into())]);
        assert_eq!(out, "<p>orig</p><p class=\"hy-zh\">你好</p>");
    }

    #[test]
    fn apply_picks_list_wrapper_tags() {
        for tag in ["li", "dt", "dd"] {
            let blocks = vec![Block {
                tag: tag.into(),
                ..Default::default()
            }];
            let out = apply_translations("<!--HYZH:0-->", &blocks, &[(0, "x".into())]);
            assert!(
                out.starts_with(&format!("<{} class=\"hy-zh\">", tag)),
                "tag {tag} produced {out}"
            );
        }
    }

    #[test]
    fn apply_escapes_translation() {
        let blocks = vec![Block::default()]; // empty tag → "p" wrapper
        let out = apply_translations("<!--HYZH:0-->", &blocks, &[(0, "<script>".into())]);
        assert_eq!(out, "<p class=\"hy-zh\">&lt;script&gt;</p>");
    }

    #[test]
    fn apply_strips_uncovered_placeholders() {
        // A placeholder with no translation must be removed, never leaked.
        let blocks = vec![Block::default(), Block::default()];
        let out = apply_translations(
            "<p>orig</p><!--HYZH:0--><!--HYZH:1-->",
            &blocks,
            &[(0, "done".into())],
        );
        assert!(out.contains("<p class=\"hy-zh\">done</p>"));
        assert!(!out.contains("HYZH"));
    }
}
