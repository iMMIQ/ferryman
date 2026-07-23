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
use lol_html::{element, rewrite_str, text, RewriteStrSettings};
use std::cell::RefCell;
use std::rc::Rc;

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

    let rewritten = rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![
                // Suppress collection inside non-content regions.
                element!(SUPPRESS_SEL, move |el| {
                    st_sup.borrow_mut().suppress += 1;
                    let s = st_sup.clone();
                    let _ = el.on_end_tag(move |_| {
                        s.borrow_mut().suppress -= 1;
                        Ok(())
                    });
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
                    });
                    s.stack.push((id, false));

                    let s2 = st_blk.clone();
                    let _ = el.on_end_tag(move |end| {
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
                    });
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
                    }
                    Ok(())
                }),
            ],
            ..RewriteStrSettings::default()
        },
    )?;

    let blocks = Rc::try_unwrap(st).ok().unwrap().into_inner().blocks;
    Ok((rewritten, blocks))
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
