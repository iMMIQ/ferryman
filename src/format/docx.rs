//! DOCX backend (`.docx`) — OOXML zip + `word/document.xml`.
//!
//! A `.docx` is a ZIP whose body text lives in `word/document.xml`: a flat
//! sequence of paragraphs (`<w:p>`), each holding runs (`<w:r>`) whose text is
//! in `<w:t>`. Word routinely fragments a single sentence across many runs (it
//! splits on spell-check / formatting boundaries), so translating run-by-run
//! would yield garbage — the unit of translation is the **paragraph**: each
//! paragraph's `<w:t>` text is concatenated into one segment.
//!
//! Write-back is surgical, not a rebuild: we keep the original `document.xml`
//! bytes verbatim and, for each translated paragraph, splice in a brand-new
//! `<w:p>` carrying the translation right after the original's end tag. The
//! original runs and structure are never mutated, so all formatting survives
//! byte-for-byte. `w:p` never nests (even inside table cells a `<w:p>` is a
//! leaf structural unit), so a single left-to-right splice — inserting after
//! each paragraph's byte range — is correct everywhere. The byte ranges come
//! from roxmltree's `positions` feature (on by default).
//!
//! Like epub, `--mode replace` is unsupported here: rewriting fragmented runs
//! in place would discard inline formatting, and bilingual insertion already
//! preserves the original perfectly. Strategy is [`Strategy::Independent`] — a
//! single paragraph can be large (academic prose), so each goes out as one
//! self-contained request (one oversized paragraph fails only itself).
//!
//! ## Scope (v1)
//! Only the main body (`word/document.xml`) is translated; headers, footers,
//! footnotes and endnotes (`word/header*.xml`, `word/footnotes.xml`, …) are
//! passed through unchanged. They share the same `<w:p>` grammar, so extending
//! is mechanical. Legacy binary `.doc` is not supported (it isn't a zip — it
//! needs a dedicated OLE parser).

use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::ops::Range;
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// The wordprocessingml namespace (the `w:` prefix used throughout document.xml).
const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// One archive entry preserved for round-tripping (only document.xml is edited).
struct DxEntry {
    name: String,
    data: Vec<u8>,
    method: CompressionMethod,
    is_dir: bool,
}

/// A paragraph: its byte range in the original `document.xml` and its
/// concatenated plain text. Built once at open; [`Document::segments`] and
/// [`Document::write`] share it so ids stay consistent.
struct Par {
    range: Range<usize>,
    text: String,
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
        let mut entries = Vec::with_capacity(za.len());
        for i in 0..za.len() {
            let mut zf = za.by_index(i)?;
            let name = zf.name().to_string();
            let is_dir = zf.is_dir();
            let method = zf.compression();
            let mut data = Vec::new();
            zf.read_to_end(&mut data)?;
            entries.push(DxEntry {
                name,
                data,
                method,
                is_dir,
            });
        }

        let doc_idx = entries
            .iter()
            .position(|e| e.name == "word/document.xml")
            .ok_or_else(|| anyhow!("docx has no word/document.xml"))?;
        let xml = String::from_utf8_lossy(&entries[doc_idx].data).into_owned();
        let (paragraphs, seg_index) = parse_paragraphs(&xml)?;

        eprintln!(
            "docx: {} paragraph(s), {} translatable",
            paragraphs.len(),
            seg_index.len()
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
        // Concatenate every w:t under this paragraph (run fragmentation means a
        // sentence is split across many <w:t>; joining reconstructs the prose).
        let mut text = String::new();
        for t in node
            .descendants()
            .filter(|n| n.is_element() && n.has_tag_name((W_NS, "t")))
        {
            if let Some(s) = t.text() {
                text.push_str(s);
            }
        }
        let idx = paragraphs.len();
        if !text.trim().is_empty() {
            seg_index.push(idx);
        }
        paragraphs.push(Par {
            range: node.range(),
            text,
        });
    }

    Ok((paragraphs, seg_index))
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
        if mode == OutputMode::Replace {
            bail!("--mode replace is not yet supported for docx");
        }

        // Route translations to their owning paragraph (keyed by paragraph
        // index). Ids not in seg_index (e.g. out of range) are ignored.
        let mut tr_by_par: HashMap<usize, String> = HashMap::new();
        for (id, tr) in translations {
            if let Some(&pidx) = self.seg_index.get(*id) {
                tr_by_par.insert(pidx, tr.clone());
            }
        }

        // Left-to-right splice: paragraphs are in ascending byte order and never
        // overlap, so `src[cursor..range.end]` carries the gap before this
        // paragraph *and* the paragraph itself; a translated paragraph then gets
        // a fresh <w:p> appended right after its end tag. Everything outside
        // translated paragraphs is copied byte-for-byte.
        let src = self.xml.as_str();
        let mut out_xml =
            String::with_capacity(src.len() + tr_by_par.values().map(String::len).sum::<usize>());
        let mut cursor = 0usize;
        for (pidx, par) in self.paragraphs.iter().enumerate() {
            out_xml.push_str(&src[cursor..par.range.end]);
            cursor = par.range.end;
            if let Some(tr) = tr_by_par.get(&pidx) {
                out_xml.push_str(&translated_paragraph(tr));
            }
        }
        out_xml.push_str(&src[cursor..]); // trailing sectPr + </w:body></w:document>

        self.entries[self.doc_idx].data = out_xml.into_bytes();
        write_zip(&self.entries, out)
    }

    fn strategy(&self) -> Strategy {
        Strategy::Independent
    }
}

/// A standalone translated paragraph: a small spacer + grey italic run, visually
/// distinct from the black original (mirrors epub's `.hy-zh` sibling). The `w:`
/// prefix resolves against the root `<w:document>` namespace declaration.
fn translated_paragraph(text: &str) -> String {
    let esc = xml_escape(text);
    format!(
        "<w:p><w:pPr><w:spacing w:after=\"160\"/></w:pPr>\
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
        assert!(s.starts_with("<w:p><w:pPr><w:spacing w:after=\"160\"/></w:pPr>"));
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
        let tmp = std::env::temp_dir().join(format!(
            "ferryman_docx_test_{}.docx",
            std::process::id()
        ));
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
    fn write_replace_bails() {
        let tmp = std::env::temp_dir().join(format!(
            "ferryman_docx_test_replace_{}.docx",
            std::process::id()
        ));
        make_docx(&tmp, SAMPLE_XML);
        let mut doc = DocxDoc::open(&tmp).unwrap();
        let err = doc
            .write(&[(0, "x".to_string())], &tmp, OutputMode::Replace)
            .unwrap_err();
        assert!(format!("{err}").contains("replace"));
        let _ = std::fs::remove_file(&tmp);
    }
}
