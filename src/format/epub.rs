//! EPUB backend — the reference [`Document`] implementation.
//!
//! Wraps the existing zip/OPF layer ([`crate::archive::Epub`]) and the `lol_html`
//! extract/apply helpers ([`crate::html`]). Holds one [`FileIr`] per spine
//! content document (the placeholder-injected HTML + its blocks) plus a dense
//! `seg_index` mapping global [`SegmentId`] -> (file, block), so translations
//! can be routed back without re-parsing. Behavior is identical to the old
//! in-`main` EPUB loop; only its home has changed.

use crate::archive::{Entry, Epub};
use crate::format::{Document, OutputMode, Segment, SegmentId};
use crate::html::{self, Block};
use anyhow::Result;
use std::path::Path;

/// Per-content-document intermediate state produced by [`html::extract`].
struct FileIr {
    /// Index into [`EpubDoc::entries`] — the entry this file's bytes live in.
    entry_idx: usize,
    /// HTML with placeholders + injected style, awaiting translation fill-in.
    rewritten_html: String,
    /// Blocks from the extract pass; [`html::apply_translations`] consults
    /// `block.tag` to pick the bilingual wrapper element.
    blocks: Vec<Block>,
}

pub struct EpubDoc {
    entries: Vec<Entry>,
    files: Vec<FileIr>,
    /// Dense `SegmentId` (= index here) -> (file_idx, block_id). Built once at
    /// open; both [`segments`](Document::segments) and [`write`](Document::write)
    /// consult it, so ids stay consistent end to end.
    seg_index: Vec<(usize, usize)>,
}

impl EpubDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let epub = Epub::load(path)?;
        let content = epub.content_files()?;
        let entries = epub.entries;

        let mut files = Vec::with_capacity(content.len());
        for cpath in &content {
            let idx = match entries.iter().position(|e| &e.name == cpath) {
                Some(i) => i,
                None => {
                    eprintln!("warn: spine item {} not found in archive, skipping", cpath);
                    continue;
                }
            };
            let src = String::from_utf8_lossy(&entries[idx].data).into_owned();
            let (rewritten_html, blocks) = match html::extract(&src) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("warn: extract {} failed: {} — copying unchanged", cpath, e);
                    continue;
                }
            };
            files.push(FileIr {
                entry_idx: idx,
                rewritten_html,
                blocks,
            });
        }

        anyhow::ensure!(
            !files.is_empty(),
            "EPUB spine contains no readable content documents"
        );

        // Dense SegmentId assignment over leaf blocks, in document order.
        // (block.leaf is only ever set true for non-empty leaf blocks — see
        // html.rs — so it's the exact same filter the old main used.)
        let mut seg_index = Vec::new();
        for (fidx, file) in files.iter().enumerate() {
            for (bid, block) in file.blocks.iter().enumerate() {
                if block.leaf {
                    seg_index.push((fidx, bid));
                }
            }
        }

        eprintln!(
            "epub: {} content file(s), {} translatable block(s)",
            files.len(),
            seg_index.len()
        );

        Ok(EpubDoc {
            entries,
            files,
            seg_index,
        })
    }
}

impl Document for EpubDoc {
    fn format_name(&self) -> &'static str {
        "epub"
    }

    fn segments(&self) -> Vec<Segment> {
        self.seg_index
            .iter()
            .enumerate()
            .map(|(id, (f, b))| Segment {
                id,
                text: self.files[*f].blocks[*b].text.clone(),
            })
            .collect()
    }

    fn write(
        &mut self,
        translations: &[(SegmentId, String)],
        out: &Path,
        mode: OutputMode,
    ) -> Result<()> {
        // Route translations to their owning file, keyed by block id (as
        // html::apply_translations expects). Ids not in seg_index are ignored.
        let mut per_file: Vec<Vec<(usize, String)>> = vec![Vec::new(); self.files.len()];
        for (id, tr) in translations {
            if let Some(&(f, b)) = self.seg_index.get(*id) {
                per_file[f].push((b, tr.clone()));
            }
        }

        // Apply per file. A file with no translations still gets its injected
        // style: apply_translations replaces zero placeholders and strips none,
        // returning the rewritten HTML (placeholders for untranslated leaves
        // are removed by its strip_leftover_placeholders pass).
        for (fidx, file) in self.files.iter().enumerate() {
            let final_html = match mode {
                OutputMode::Bilingual => {
                    html::apply_translations(&file.rewritten_html, &file.blocks, &per_file[fidx])
                }
                OutputMode::Replace => {
                    let source = String::from_utf8_lossy(&self.entries[file.entry_idx].data);
                    html::replace_translations(&source, &file.blocks, &per_file[fidx])
                }
            };
            self.entries[file.entry_idx].data = final_html.into_bytes();
        }

        // Hand the (mutated) entries to the zip layer; it keeps mimetype first
        // & stored, preserving original compression for every other entry.
        let out_epub = Epub {
            entries: std::mem::take(&mut self.entries),
        };
        out_epub.write(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::CompressionMethod;

    #[test]
    fn replace_writes_an_epub_preserving_assets_and_missing_translations() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input.epub");
        let output = root.path().join("translated.epub");
        let file = |name: &str, bytes: &[u8]| Entry {
            name: name.into(),
            data: bytes.into(),
            method: CompressionMethod::Deflated,
            is_dir: false,
        };
        Epub { entries: vec![
            file("mimetype", b"application/epub+zip"),
            file("META-INF/container.xml", br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#),
            file("OEBPS/content.opf", br#"<package><manifest><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#),
            file("OEBPS/chapter.xhtml", br#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Title</title></head><body><h1 id="title">Original title</h1><p>A <em>paragraph</em>.</p><p>Untouched</p><img src="figure.png"/></body></html>"#),
            file("OEBPS/figure.png", &[1, 2, 3, 4]),
        ] }.write(&input).unwrap();
        let mut document = EpubDoc::open(&input).unwrap();
        assert_eq!(document.segments().len(), 3);
        document
            .write(
                &[(0, "译文标题".into()), (1, "翻译后的段落。".into())],
                &output,
                OutputMode::Replace,
            )
            .unwrap();
        let epub = Epub::load(&output).unwrap();
        let html = epub.read_text("OEBPS/chapter.xhtml").unwrap();
        assert!(html.contains("<h1 id=\"title\">译文标题</h1>"));
        assert!(html.contains("<p>Untouched</p>"));
        assert!(
            !html.contains("Original title")
                && !html.contains("paragraph")
                && !html.contains("HYZH")
        );
        let parsed = roxmltree::Document::parse(&html).unwrap();
        let paragraph = parsed
            .descendants()
            .find(|n| n.has_tag_name(("http://www.w3.org/1999/xhtml", "p")))
            .unwrap();
        assert_eq!(
            paragraph
                .descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>(),
            "翻译后的段落。"
        );
        assert_eq!(
            epub.entries[epub.find("OEBPS/figure.png").unwrap()].data,
            [1, 2, 3, 4]
        );
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
        let mimetype = archive.by_index(0).unwrap();
        assert_eq!(mimetype.name().unwrap(), "mimetype");
        assert_eq!(mimetype.compression(), CompressionMethod::Stored);
    }
}
