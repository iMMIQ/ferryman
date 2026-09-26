//! CommonMark parsing with source-range edits: syntax, code, URLs and metadata
//! never enter the translator. Bilingual mode duplicates complete source blocks.
use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{Context, Result};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use std::{collections::HashMap, fs, ops::Range, path::Path};

pub struct MdDoc {
    source: String,
    blocks: Vec<Block>,
    spans: Vec<Span>,
}
struct Block {
    footnote: bool,
    range: Range<usize>,
    spans: Range<usize>,
}
struct Span {
    range: Range<usize>,
    text: String,
}
impl MdDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let source =
            fs::read_to_string(path).with_context(|| format!("read md {}", path.display()))?;
        Ok(Self::parse(source))
    }

    fn parse(source: String) -> Self {
        // Metadata is only recognized at the beginning. Treat unclosed metadata
        // as opaque as well, rather than leaking configuration into translation.
        let first = source.lines().next().unwrap_or("").trim();
        let offset = if matches!(first, "---" | "+++") {
            let mut end = 0;
            for (index, line) in source.split_inclusive('\n').enumerate() {
                end += line.len();
                if index > 0 && line.trim() == first {
                    break;
                }
            }
            end
        } else {
            0
        };
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES;
        let mut blocks = Vec::new();
        let mut spans = Vec::new();
        let mut depth = 0;
        let mut opaque = 0;
        let mut block = None;
        for (event, range) in Parser::new_ext(&source[offset..], options).into_offset_iter() {
            let range = range.start + offset..range.end + offset;
            match event {
                Event::Start(tag) => {
                    if depth == 0 {
                        block = Some((
                            range,
                            spans.len(),
                            matches!(tag, Tag::FootnoteDefinition(_)),
                        ));
                    }
                    depth += 1;
                    if matches!(
                        tag,
                        Tag::CodeBlock(_)
                            | Tag::HtmlBlock
                            | Tag::MetadataBlock(_)
                            | Tag::Link {
                                link_type: LinkType::Autolink | LinkType::Email,
                                ..
                            }
                    ) {
                        opaque += 1;
                    }
                }
                Event::End(tag) => {
                    if matches!(
                        tag,
                        TagEnd::CodeBlock
                            | TagEnd::HtmlBlock
                            | TagEnd::MetadataBlock(_)
                            | TagEnd::Link
                    ) && opaque > 0
                    {
                        opaque -= 1;
                    }
                    depth -= 1;
                    if depth == 0 {
                        if let Some((mut range, start, footnote)) = block.take() {
                            range.end = range.start
                                + source[range.clone()].trim_end_matches(['\r', '\n']).len();
                            if spans.len() > start {
                                blocks.push(Block {
                                    footnote,
                                    range,
                                    spans: start..spans.len(),
                                });
                            }
                        }
                    }
                }
                Event::Text(text) if opaque == 0 && !text.trim().is_empty() => {
                    // Preserve surrounding whitespace in source, including CRLF.
                    let raw = &source[range.clone()];
                    let start = range.start + raw.len() - raw.trim_start().len();
                    let end = range.end - (raw.len() - raw.trim_end().len());
                    spans.push(Span {
                        range: start..end,
                        text: text.trim().to_owned(),
                    });
                }
                _ => {}
            }
        }
        Self {
            source,
            blocks,
            spans,
        }
    }

    fn render(&self, translations: &HashMap<usize, String>, mode: OutputMode) -> String {
        let mut output = String::new();
        let mut cursor = 0;
        let newline = if self.source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        for block in &self.blocks {
            output.push_str(&self.source[cursor..block.range.start]);
            let original = &self.source[block.range.clone()];
            let mut translated = String::new();
            let mut position = block.range.start;
            let mut changed = false;
            for index in block.spans.clone() {
                let span = &self.spans[index];
                translated.push_str(&self.source[position..span.range.start]);
                if let Some(text) = translations
                    .get(&index)
                    .filter(|t| !t.trim().is_empty() && t.trim() != span.text)
                {
                    changed = true;
                    if block.footnote && mode == OutputMode::Bilingual {
                        translated.push_str(&self.source[span.range.clone()]);
                        translated.push_str(" / ");
                    }
                    // A text node must stay a text node, not introduce Markdown.
                    for ch in text.trim().chars() {
                        if "\\`*_{}[]<>#!|~-+.&".contains(ch) {
                            translated.push('\\');
                        }
                        if ch == '\n' || ch == '\r' {
                            translated.push(' ');
                        } else {
                            translated.push(ch);
                        }
                    }
                } else {
                    translated.push_str(&self.source[span.range.clone()]);
                }
                position = span.range.end;
            }
            translated.push_str(&self.source[position..block.range.end]);
            if changed && mode == OutputMode::Bilingual && !block.footnote {
                output.push_str(original);
                if !original.ends_with('\n') {
                    output.push_str(newline);
                }
                output.push_str(newline);
            }
            output.push_str(&translated);
            cursor = block.range.end;
        }
        output.push_str(&self.source[cursor..]);
        output
    }
}
impl Document for MdDoc {
    fn format_name(&self) -> &'static str {
        "md"
    }
    fn segments(&self) -> Vec<Segment> {
        self.spans
            .iter()
            .enumerate()
            .map(|(id, span)| Segment {
                id,
                text: span.text.clone(),
            })
            .collect()
    }
    fn write(
        &mut self,
        translations: &[(SegmentId, String)],
        out: &Path,
        mode: OutputMode,
    ) -> Result<()> {
        fs::write(
            out,
            self.render(&translations.iter().cloned().collect(), mode),
        )
        .with_context(|| format!("write md {}", out.display()))
    }
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

    #[test]
    fn bilingual_footnotes_keep_one_definition() {
        let doc = MdDoc::parse("Hello[^note].\n\n[^note]: Original **bold** note.\n".into());
        let tr = doc
            .segments()
            .iter()
            .map(|s| (s.id, "译文".into()))
            .collect();
        let output = doc.render(&tr, OutputMode::Bilingual);
        assert_eq!(output.matches("[^note]:").count(), 1);
        let mut html = String::new();
        pulldown_cmark::html::push_html(
            &mut html,
            Parser::new_ext(&output, Options::ENABLE_FOOTNOTES),
        );
        assert_eq!(html.matches("id=\"note\"").count(), 1);
        assert!(html.contains("Original / 译文"));
    }

    #[test]
    fn commonmark_protects_code_metadata_urls_and_preserves_bytes() {
        let source = "---\r\ntitle: secret\r\n---\r\n\r\n# Hello\r\n\r\n    print(\"keep\")\r\n\r\n````text\r\n```\r\nDO_NOT_TRANSLATE\r\n````\r\n\r\nRead `code` and [label](https://example.com \"title\"). <https://example.org>\r\n\r\n[id]: https://example.net\r\n";
        let doc = MdDoc::parse(source.into());
        assert_eq!(doc.render(&HashMap::new(), OutputMode::Replace), source);
        let segments = doc.segments();
        assert!(segments.iter().any(|s| s.text == "Hello"));
        assert!(segments.iter().any(|s| s.text == "label"));
        assert!(segments.iter().all(|s| !s.text.contains("secret")
            && !s.text.contains("print")
            && !s.text.contains("DO_NOT")
            && !s.text.contains("https:")
            && !s.text.contains("code")));
        let tr = segments.iter().map(|s| (s.id, "译文".to_owned())).collect();
        let output = doc.render(&tr, OutputMode::Replace);
        for protected in [
            "title: secret",
            "print(\"keep\")",
            "DO_NOT_TRANSLATE",
            "`code`",
            "https://example.com \"title\"",
            "<https://example.org>",
            "[id]: https://example.net",
        ] {
            assert!(output.contains(protected), "{protected}");
        }
        assert!(output.contains("# 译文\r\n"));
    }
    #[test]
    fn bilingual_duplicates_complete_blocks_and_preserves_inline_structure() {
        let source = "> Read **bold** and `x`\n\n- First\n- Second\n\nHeading\n=======\n";
        let doc = MdDoc::parse(source.into());
        let tr = doc
            .segments()
            .iter()
            .map(|s| (s.id, "译文".into()))
            .collect();
        let output = doc.render(&tr, OutputMode::Bilingual);
        assert!(output.contains("> Read **bold** and `x`\n\n> 译文 **译文** 译文 `x`"));
        assert!(
            output.contains("- First\n- Second\n\n- 译文\n- 译文"),
            "{output:?}"
        );
        assert!(output.contains("Heading\n=======\n\n译文\n======="));
    }
    #[test]
    fn untranslated_and_escaped_nodes_roundtrip() {
        for source in [
            "+++\ntitle = 'x'\n",
            "Hello &amp; world\\*\n",
            "| A | B |\n|---|---|\n| x | y |\n",
            "- [x] Done\n\n[^a]: note\n",
        ] {
            let doc = MdDoc::parse(source.into());
            assert_eq!(doc.render(&HashMap::new(), OutputMode::Bilingual), source);
        }
    }
}
