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
use anyhow::{bail, Result};
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
        if mode == OutputMode::Replace {
            bail!("--mode replace is not yet supported for epub");
        }

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
            let final_html =
                html::apply_translations(&file.rewritten_html, &file.blocks, &per_file[fidx]);
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
