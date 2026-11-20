//! DOCX backend (`.docx`) — OOXML zip + `word/document.xml`.
//!
//! A `.docx` is a ZIP whose body text lives in `word/document.xml`: a flat
//! sequence of paragraphs (`<w:p>`), each holding runs (`<w:r>`) whose text is
//! in `<w:t>`. Word routinely fragments a single sentence across many runs (it
//! splits on spell-check / formatting boundaries), so translating run-by-run
//! would yield garbage — the unit of translation is the **paragraph**: each
//! paragraph's `<w:t>` text is concatenated into one segment.
//!
//! Write-back uses source-range edits. Bilingual mode inserts a translated
//! paragraph after the original. Replace mode distributes translated graphemes
//! across the existing text runs, preserving paragraph/run properties, links,
//! images, bookmarks and nested text boxes. Text belongs to its nearest
//! paragraph; the byte ranges come from roxmltree's `positions` feature.
//!
//! Strategy is [`Strategy::Independent`] — a
//! single paragraph can be large (academic prose), so each goes out as one
//! self-contained request (one oversized paragraph fails only itself).
//!
//! ## Scope (v1)
//! Only the main body (`word/document.xml`) is translated; headers, footers,
//! footnotes and endnotes (`word/header*.xml`, `word/footnotes.xml`, …) are
//! passed through unchanged. They share the same `<w:p>` grammar, so extending
//! is mechanical. Legacy binary `.doc` is not supported (it isn't a zip — it
//! needs a dedicated OLE parser).

use crate::archive::{read_entries, ArchiveLimits, Entry as DxEntry};
use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs::File;
#[cfg(test)]
use std::io::Read;
use std::io::Write;
use std::ops::Range;
use std::path::Path;
use unicode_segmentation::UnicodeSegmentation;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

/// The wordprocessingml namespace (the `w:` prefix used throughout document.xml).
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// A paragraph: its byte range in the original `document.xml` and its
/// concatenated plain text. Built once at open; [`Document::segments`] and
/// [`Document::write`] share it so ids stay consistent.
struct Par {
    range: Range<usize>,
    text: String,
    runs: Vec<TextRun>,
    controls: Vec<Range<usize>>,
}

struct TextRun {
    range: Range<usize>,
    opening: String,
    name: String,
    weight: usize,
}

pub struct DocxDoc {
    /// Original `document.xml` bytes (spliced at write time; never mutated).
    xml: String,
    /// Every paragraph in document order (translatable or not).
    paragraphs: Vec<Par>,
    /// Dense [`SegmentId`] (= index here) -> index into [`DocxDoc::paragraphs`],
    /// built by dropping empty paragraphs. Both `segments` and `write` consult
    /// it, so ids align end to end.
    seg_index: Vec<usize>,
    /// All zip parts (so parts we don't translate round-trip unchanged).
    entries: Vec<DxEntry>,
    /// Index into [`DocxDoc::entries`] of `word/document.xml`.
    doc_idx: usize,
}

impl DocxDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut za = ZipArchive::new(file).context("read docx zip")?;
        let entries = read_entries(&mut za, ArchiveLimits::default())?;

        let doc_idx = entries
            .iter()
            .position(|e| e.name == "word/document.xml")
            .ok_or_else(|| anyhow!("docx has no word/document.xml"))?;
        let xml = String::from_utf8_lossy(&entries[doc_idx].data).into_owned();
        let (paragraphs, seg_index) = parse_paragraphs(&xml)?;

        tracing::debug!(
            paragraphs = paragraphs.len(),
            translatable = seg_index.len(),
            "parsed docx"
        );

        Ok(DocxDoc {
            xml,
            paragraphs,
            seg_index,
            entries,
            doc_idx,
        })
    }
}

/// Parse `document.xml` into paragraphs (byte range + text) and a dense
/// translatable index. Namespace-qualifies `w:p` / `w:t` so DrawingML `a:p` /
/// `a:t` inside drawings are never mistaken for content.
fn parse_paragraphs(xml: &str) -> Result<(Vec<Par>, Vec<usize>)> {
    let doc = roxmltree::Document::parse(xml).context("parse document.xml")?;

    let mut paragraphs = Vec::new();
    let mut seg_index = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| n.is_element() && n.has_tag_name((W_NS, "p")))
    {
        // Text belongs to its nearest paragraph, including inside text boxes.
        let mut text = String::new();
        let mut runs = Vec::new();
        let mut controls = Vec::new();
        for t in node.descendants().filter(|n| n.is_element()) {
            if t.ancestors().skip(1).find(|n| n.has_tag_name((W_NS, "p"))) != Some(node) {
                continue;
            }
            // Tab stops inside paragraph properties are layout, not text tabs.
            if t.ancestors()
                .take_while(|ancestor| *ancestor != node)
                .any(|ancestor| {
                    ancestor.has_tag_name((W_NS, "pPr")) || ancestor.has_tag_name((W_NS, "rPr"))
                })
            {
                continue;
            }
            if t.has_tag_name((W_NS, "t")) {
                let value = t.text().unwrap_or("");
                text.push_str(value);
                if !value.is_empty() {
                    runs.push(text_run(xml, t, value));
                }
            } else if t.has_tag_name((W_NS, "tab")) {
                text.push('\t');
                controls.push(t.range());
            } else if t.has_tag_name((W_NS, "br")) || t.has_tag_name((W_NS, "cr")) {
                text.push('\n');
                // Page/column breaks remain structural even in replace mode.
                if t.attribute((W_NS, "type"))
                    .is_none_or(|kind| kind == "textWrapping")
                {
                    controls.push(t.range());
                }
            }
        }
        let idx = paragraphs.len();
        if !text.trim().is_empty() {
            seg_index.push(idx);
        }
        paragraphs.push(Par {
            range: node.range(),
            text,
            runs,
            controls,
        });
    }

    Ok((paragraphs, seg_index))
}

fn text_run(xml: &str, node: roxmltree::Node<'_, '_>, value: &str) -> TextRun {
    let range = node.range();
    let raw = &xml[range.clone()];
    let name = raw[1..]
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '>' | '/'))
        .next()
        .unwrap()
        .to_owned();
    // Find the tag end without mistaking a quoted '>' attribute value for it.
    let mut quote = None;
    let end = raw
        .bytes()
        .enumerate()
        .find_map(|(index, byte)| {
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    quote = None;
                }
            } else if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
            } else if byte == b'>' {
                return Some(index + 1);
            }
            None
        })
        .expect("parsed XML element has a closing delimiter");
    let mut opening = raw[..end].to_owned();
    if let Some(space) = node.attributes().find(|attr| {
        attr.namespace() == Some("http://www.w3.org/XML/1998/namespace") && attr.name() == "space"
    }) {
        let value = space.range_value();
        opening.replace_range(
            value.start - range.start..value.end - range.start,
            "preserve",
        );
    } else {
        opening.insert_str(opening.len() - 1, " xml:space=\"preserve\"");
    }
    TextRun {
        range,
        opening,
        name,
        weight: value.graphemes(true).count(),
    }
}

fn translated_run(run: &TextRun, text: &str) -> String {
    let mut output = run.opening.clone();
    for (index, part) in text.split_inclusive(['\n', '\t']).enumerate() {
        if index > 0 {
            output.push_str(&run.opening);
        }
        let control = part.chars().last().filter(|ch| matches!(ch, '\n' | '\t'));
        let value = if control.is_some() {
            &part[..part.len() - 1]
        } else {
            part
        };
        output.push_str(&xml_escape(value));
        output.push_str(&format!("</{}>", run.name));
        if let Some(control) = control {
            let tag = if control == '\n' { "br" } else { "tab" };
            output.push_str(&format!("<w:{tag} xmlns:w=\"{W_NS}\"/>"));
        }
    }
    if text.is_empty() {
        output.push_str(&format!("</{}>", run.name));
    }
    output
}

fn replacement_edits(paragraph: &Par, translation: &str) -> Vec<(Range<usize>, String)> {
    let translation = translation.replace("\r\n", "\n").replace('\r', "\n");
    let mut boundaries: Vec<_> = translation
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .collect();
    boundaries.push(translation.len());
    let total: usize = paragraph.runs.iter().map(|run| run.weight).sum();
    if total == 0 {
        return Vec::new();
    }
    let mut consumed = 0;
    let mut start = 0;
    let mut edits = Vec::new();
    for run in &paragraph.runs {
        consumed += run.weight;
        let end = ((consumed as u128 * (boundaries.len() - 1) as u128) / total as u128) as usize;
        edits.push((
            run.range.clone(),
            translated_run(run, &translation[boundaries[start]..boundaries[end]]),
        ));
        start = end;
    }
    edits.extend(
        paragraph
            .controls
            .iter()
            .map(|range| (range.clone(), String::new())),
    );
    edits
}

impl Document for DocxDoc {
    fn format_name(&self) -> &'static str {
        "docx"
    }

    fn segments(&self) -> Vec<Segment> {
        self.seg_index
            .iter()
            .enumerate()
            .map(|(id, &pidx)| Segment {
                id,
                text: self.paragraphs[pidx].text.clone(),
            })
            .collect()
    }

    fn write(
        &mut self,
        translations: &[(SegmentId, String)],
        out: &Path,
        mode: OutputMode,
    ) -> Result<()> {
        // Route translations to their owning paragraph (keyed by paragraph
        // index). Ids not in seg_index (e.g. out of range) are ignored.
        let mut tr_by_par: HashMap<usize, String> = HashMap::new();
        for (id, tr) in translations {
            if let Some(&pidx) = self.seg_index.get(*id) {
                tr_by_par.insert(pidx, tr.clone());
            }
        }

        // Insertions sorted by end offset also support paragraphs nested in
        // text boxes: source bytes are copied exactly once.
        let src = self.xml.as_str();
        let mut edits: Vec<(Range<usize>, String)> = tr_by_par
            .iter()
            .flat_map(|(&index, text)| {
                let paragraph = &self.paragraphs[index];
                match mode {
                    OutputMode::Bilingual => vec![(
                        paragraph.range.end..paragraph.range.end,
                        translated_paragraph(text),
                    )],
                    OutputMode::Replace => replacement_edits(paragraph, text),
                }
            })
            .collect();
        edits.sort_by_key(|(range, _)| range.start);
        let mut out_xml = String::new();
        let mut cursor = 0;
        for (range, text) in edits {
            out_xml.push_str(&src[cursor..range.start]);
            out_xml.push_str(&text);
            cursor = range.end;
        }
        out_xml.push_str(&src[cursor..]);

        self.entries[self.doc_idx].data = out_xml.into_bytes();
        write_zip(&self.entries, out)
    }

    fn strategy(&self) -> Strategy {
        Strategy::Independent
    }
}

/// A standalone translated paragraph: a small spacer + grey italic run, visually
/// distinct from the black original (mirrors epub's `.hy-zh` sibling). The `w:`
/// prefix is explicitly bound so alternate source prefixes also work.
fn translated_paragraph(text: &str) -> String {
    let esc = xml_escape(text);
    format!(
        "<w:p xmlns:w=\"{W_NS}\"><w:pPr><w:spacing w:after=\"160\"/></w:pPr>\
         <w:r><w:rPr><w:color w:val=\"6A6A9A\"/><w:i/><w:sz w:val=\"21\"/></w:rPr>\
         <w:t xml:space=\"preserve\">{esc}</w:t></w:r></w:p>"
    )
}

/// Escape text for XML element content (`&`, `<`, `>`) and strip the C0 control
/// chars XML 1.0 forbids (keeping `\t \n \r`) so a stray control char from the
/// model can't make the part unparseable.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            _ => out.push(c),
        }
    }
    out
}

/// Rewrite the zip preserving each part's original compression and order. DOCX
/// has no EPUB-style "mimetype first, stored" rule, so this is a plain round-trip
/// that only differs from the input in the (edited) document.xml bytes.
fn write_zip(entries: &[DxEntry], path: &Path) -> Result<()> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut zw = ZipWriter::new(file);
    for e in entries {
        if e.is_dir {
            zw.add_directory(&e.name, SimpleFileOptions::default())?;
        } else {
            let opts = SimpleFileOptions::default().compression_method(e.method);
            zw.start_file(&e.name, opts)?;
            zw.write_all(&e.data)?;
        }
    }
    zw.finish().context("finish docx zip")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_textboxes_and_alternate_prefixes_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.docx");
        let output = root.path().join("output.docx");
        let source = format!(
            r#"<x:document xmlns:x="{W_NS}" xmlns:v="urn:schemas-microsoft-com:vml"><x:body><x:p><x:r><x:t>Hello</x:t><x:tab/><x:t>world</x:t><x:br/><x:t>next</x:t><x:pict><v:shape><v:textbox><x:txbxContent><x:p><x:r><x:t>Textbox</x:t></x:r></x:p></x:txbxContent></v:textbox></v:shape></x:pict></x:r></x:p></x:body></x:document>"#
        );
        make_docx(&input, &source);
        let mut doc = DocxDoc::open(&input).unwrap();
        let segments = doc.segments();
        assert_eq!(
            segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["Hello\tworld\nnext", "Textbox"]
        );
        doc.write(
            &[
                (0, "Body translation".into()),
                (1, "Box translation".into()),
            ],
            &output,
            OutputMode::Bilingual,
        )
        .unwrap();
        let xml = read_document_xml(&output);
        let parsed = roxmltree::Document::parse(&xml).unwrap();
        let paragraphs: Vec<_> = parsed
            .descendants()
            .filter(|n| n.has_tag_name((W_NS, "p")))
            .collect();
        assert_eq!(paragraphs.len(), 4);
        assert_eq!(xml.matches("Box translation").count(), 1);
        assert_eq!(xml.matches("Body translation").count(), 1);
    }

    /// A minimal document.xml exercising run fragmentation, an empty paragraph,
    /// a table (paragraph nested in a cell), an entity, and DrawingML `a:p`/`a:t`
    /// (which must NOT be picked up).
    const SAMPLE_XML: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><w:body>\
<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>World</w:t></w:r></w:p>\
<w:p><w:pPr/></w:p>\
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>cell text</w:t></w:r></w:p></w:tc></w:tr></w:tbl>\
<w:p><w:r><w:t>Final &amp; line</w:t></w:r></w:p>\
<w:drawing><a:p><a:t>not me</a:t></a:p></w:drawing>\
</w:body></w:document>";

    #[test]
    fn parse_concatenates_runs_and_skips_empty_and_drawingml() {
        let (paragraphs, seg_index) = parse_paragraphs(SAMPLE_XML).unwrap();
        // 4 w:p (one empty); the DrawingML a:p is excluded.
        assert_eq!(paragraphs.len(), 4);
        assert_eq!(paragraphs[0].text, "Hello World"); // two runs joined
        assert_eq!(paragraphs[1].text, ""); // empty paragraph
        assert_eq!(paragraphs[2].text, "cell text"); // inside a table cell
        assert_eq!(paragraphs[3].text, "Final & line"); // entity resolved
                                                        // only the 3 non-empty paragraphs are translatable (empty one excluded).
        assert_eq!(seg_index, vec![0, 2, 3]);
    }

    #[test]
    fn parse_ranges_are_ascending_and_non_overlapping() {
        let (paragraphs, _) = parse_paragraphs(SAMPLE_XML).unwrap();
        for w in paragraphs.windows(2) {
            assert!(
                w[0].range.end <= w[1].range.start,
                "ranges overlap: {:?} then {:?}",
                w[0].range,
                w[1].range
            );
        }
    }

    #[test]
    fn xml_escape_replaces_entities_and_strips_controls() {
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(xml_escape("plain"), "plain");
        assert_eq!(xml_escape("你好"), "你好");
        // C0 controls (except \t \n \r) are stripped; \n survives.
        assert_eq!(xml_escape("a\x07b\nc"), "ab\nc");
        assert_eq!(xml_escape("x\ty"), "x\ty");
        assert_eq!(xml_escape(""), "");
    }

    #[test]
    fn translated_paragraph_is_well_formed_and_distinct() {
        let s = translated_paragraph("你好 & <world>");
        // opens/closes a w:p, has the styled run, preserves the spacer.
        assert!(s.starts_with(&format!(
            "<w:p xmlns:w=\"{W_NS}\"><w:pPr><w:spacing w:after=\"160\"/></w:pPr>"
        )));
        assert!(s.contains("<w:i/>"));
        assert!(s.contains("<w:t xml:space=\"preserve\">"));
        assert!(s.ends_with("</w:p>"));
        // text is escaped.
        assert!(s.contains("你好 &amp; &lt;world&gt;"));
        assert!(!s.contains("<world>"));
    }

    /// Build a minimal one-part docx (just word/document.xml) at `path`.
    fn make_docx(path: &Path, xml: &str) {
        let file = File::create(path).unwrap();
        let mut zw = ZipWriter::new(file);
        zw.start_file("word/document.xml", SimpleFileOptions::default())
            .unwrap();
        zw.write_all(xml.as_bytes()).unwrap();
        zw.finish().unwrap();
    }

    /// Read `word/document.xml` back out of a docx at `path`.
    fn read_document_xml(path: &Path) -> String {
        let f = File::open(path).unwrap();
        let mut za = ZipArchive::new(f).unwrap();
        let mut buf = Vec::new();
        za.by_index(0).unwrap().read_to_end(&mut buf).unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn roundtrip_inserts_translation_and_preserves_originals() {
        let tmp =
            std::env::temp_dir().join(format!("ferryman_docx_test_{}.docx", std::process::id()));
        make_docx(&tmp, SAMPLE_XML);

        let mut doc = DocxDoc::open(&tmp).unwrap();
        let segs = doc.segments();
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].text, "Hello World");

        // Translate only the first paragraph (dense id 0 -> paragraph 0).
        doc.write(&[(0, "你好 世界".to_string())], &tmp, OutputMode::Bilingual)
            .unwrap();

        let out = read_document_xml(&tmp);
        // Original paragraphs survive verbatim...
        assert!(out.contains("Hello </w:t></w:r><w:r><w:t>World"));
        assert!(out.contains("cell text"));
        assert!(out.contains("Final &amp; line"));
        // ...and a styled translated paragraph was inserted right after para 0.
        assert!(out.contains("<w:t xml:space=\"preserve\">你好 世界</w:t>"));
        // The translated paragraph sits before paragraph 2's text (ordering).
        let tr_pos = out.find("你好 世界").unwrap();
        let cell_pos = out.find("cell text").unwrap();
        assert!(tr_pos < cell_pos);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn write_with_no_translations_roundtrips_xml_unchanged() {
        let tmp = std::env::temp_dir().join(format!(
            "ferryman_docx_test_notr_{}.docx",
            std::process::id()
        ));
        make_docx(&tmp, SAMPLE_XML);
        let mut doc = DocxDoc::open(&tmp).unwrap();
        doc.write(&[], &tmp, OutputMode::Bilingual).unwrap();
        // No insertions → document.xml identical to the source.
        assert_eq!(read_document_xml(&tmp), SAMPLE_XML);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn replace_translates_existing_runs_and_keeps_missing_paragraphs() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.docx");
        let output = root.path().join("output.docx");
        make_docx(&input, SAMPLE_XML);
        let mut document = DocxDoc::open(&input).unwrap();
        document
            .write(
                &[(0, " 你好👩🏽‍💻e\u{301} & 世界 ".into())],
                &output,
                OutputMode::Replace,
            )
            .unwrap();
        let xml = read_document_xml(&output);
        let (paragraphs, indices) = parse_paragraphs(&xml).unwrap();
        assert_eq!(indices.len(), 3);
        assert_eq!(paragraphs[0].text, " 你好👩🏽‍💻e\u{301} & 世界 ");
        assert_eq!(paragraphs[2].text, "cell text");
        assert_eq!(paragraphs[3].text, "Final & line");
        assert!(!xml.contains("Hello") && !xml.contains("World"));
        assert!(xml.contains("<w:p><w:pPr/></w:p>"));
        assert!(xml.contains("<a:p><a:t>not me</a:t></a:p>"));
        assert_eq!(xml.matches("xml:space=\"preserve\"").count(), 2);
        let parsed = roxmltree::Document::parse(&xml).unwrap();
        let nodes: Vec<_> = parsed
            .descendants()
            .filter(|node| node.has_tag_name((W_NS, "t")))
            .take(2)
            .collect();
        let first = nodes[0].text().unwrap();
        assert!(" 你好👩🏽‍💻e\u{301} & 世界 "
            .grapheme_indices(true)
            .any(|(index, _)| index == first.len()));
    }

    #[test]
    fn replace_preserves_structure_assets_and_word_controls() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.docx");
        let output = root.path().join("output.docx");
        let source = format!(
            r#"<x:document xmlns:x="{W_NS}" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><x:body><x:p><x:pPr><x:pStyle x:val="Heading1"/><x:tabs><x:tab x:val="left" x:pos="720"/></x:tabs></x:pPr><x:bookmarkStart x:id="1" x:name="anchor"/><x:r><x:rPr><x:b/></x:rPr><x:t xml:space="default" data-note="a &gt; b">Hello</x:t><x:tab/><x:t>world</x:t><x:br/><x:t>next</x:t><x:br x:type="page"/><x:drawing/></x:r><x:hyperlink r:id="rId1"><x:r><x:rPr><x:i/></x:rPr><x:t>Link</x:t></x:r></x:hyperlink><x:r><x:pict><v:shape><v:textbox><x:txbxContent><x:p><x:r><x:t>Textbox</x:t></x:r></x:p></x:txbxContent></v:textbox></v:shape></x:pict></x:r><x:bookmarkEnd x:id="1"/></x:p></x:body></x:document>"#
        );
        make_docx(&input, &source);
        // Additional parts and their bytes must survive the archive rewrite.
        let mut document = DocxDoc::open(&input).unwrap();
        document.entries.push(DxEntry {
            name: "word/media/picture.png".into(),
            data: vec![1, 2, 3],
            method: zip::CompressionMethod::Stored,
            is_dir: false,
        });
        document.entries.push(DxEntry {
            name: "word/header1.xml".into(),
            data: b"unchanged header".to_vec(),
            method: zip::CompressionMethod::Deflated,
            is_dir: false,
        });
        let translation = "首行\t制表\r\n第二行<&>👩🏽‍💻";
        document
            .write(
                &[(0, translation.into()), (1, "文本框译文".into())],
                &output,
                OutputMode::Replace,
            )
            .unwrap();
        let xml = read_document_xml(&output);
        let parsed = roxmltree::Document::parse(&xml).unwrap();
        let (paragraphs, _) = parse_paragraphs(&xml).unwrap();
        assert_eq!(
            paragraphs[0].text.replace('\n', ""),
            translation.replace(['\r', '\n'], "")
        );
        assert_eq!(paragraphs[1].text, "文本框译文");
        assert!(!xml.contains("Hello") && !xml.contains("Textbox") && !xml.contains("default"));
        assert!(xml.contains("<x:rPr><x:b/></x:rPr>"));
        assert!(xml.contains("<x:rPr><x:i/></x:rPr>"));
        assert!(xml.contains("<x:tabs><x:tab x:val=\"left\" x:pos=\"720\"/></x:tabs>"));
        assert!(xml.contains("<x:hyperlink r:id=\"rId1\">"));
        assert!(xml.contains("<x:br x:type=\"page\"/>") && xml.contains("<x:drawing/>"));
        assert!(xml.contains("<x:bookmarkStart x:id=\"1\" x:name=\"anchor\"/>"));
        assert_eq!(
            parsed
                .descendants()
                .filter(|node| node.has_tag_name((W_NS, "tab"))
                    && node
                        .ancestors()
                        .any(|ancestor| ancestor.has_tag_name((W_NS, "r"))))
                .count(),
            1
        );
        assert_eq!(
            parsed
                .descendants()
                .filter(|node| node.has_tag_name((W_NS, "br"))
                    && node.attribute((W_NS, "type")).is_none())
                .count(),
            1
        );
        let mut archive = ZipArchive::new(File::open(&output).unwrap()).unwrap();
        let mut picture = Vec::new();
        archive
            .by_name("word/media/picture.png")
            .unwrap()
            .read_to_end(&mut picture)
            .unwrap();
        assert_eq!(picture, [1, 2, 3]);
        let mut header = String::new();
        archive
            .by_name("word/header1.xml")
            .unwrap()
            .read_to_string(&mut header)
            .unwrap();
        assert_eq!(header, "unchanged header");
    }

    #[test]
    fn replace_with_no_translations_preserves_xml_and_empty_translation_clears_text() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.docx");
        let output = root.path().join("output.docx");
        make_docx(&input, SAMPLE_XML);
        let mut document = DocxDoc::open(&input).unwrap();
        document.write(&[], &output, OutputMode::Replace).unwrap();
        assert_eq!(read_document_xml(&output), SAMPLE_XML);
        document
            .write(&[(0, String::new())], &output, OutputMode::Replace)
            .unwrap();
        let xml = read_document_xml(&output);
        roxmltree::Document::parse(&xml).unwrap();
        assert!(!xml.contains("Hello") && !xml.contains("World"));
        assert!(xml.contains("cell text"));
    }
}
