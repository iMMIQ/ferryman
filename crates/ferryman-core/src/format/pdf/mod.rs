//! PDF backend (`.pdf`) — layout-preserving bilingual translation.
//!
//! PDF is a *page-description* format: text is placed as positioned glyph
//! runs, paragraphs exist only visually, and editing text in place is
//! impossible without re-typesetting everything after it. But unlike the
//! other backends we don't have to throw the original pages away — figures,
//! diagrams and vector art live in the very content streams we can read.
//! This backend therefore **edits the original file**:
//!
//! 1. A custom [`pdf_extract::OutputDev`] collects every character with its
//!    text-rendering matrix, so each *visual* line keeps its position, font
//!    size and width. Characters are grouped into lines geometrically (same
//!    baseline, no big gaps) and lines into **paragraphs** (previous line
//!    full-width, same leading, flush left margin, dehyphenated) — a
//!    sentence wrapped across three lines is one segment, exactly like a
//!    docx paragraph. Paragraphs even continue across page breaks.
//! 2. Each paragraph is translated [`Strategy::Independent`] — it is a
//!    complete semantic unit, like epub/docx blocks.
//! 3. [`Document::write`] reloads the original bytes with [`lopdf`] and
//!    edits them in place:
//!    - **bilingual**: after every original page a *translation page* of the
//!      same size is inserted, drawing each paragraph's translation at the
//!      mirrored position in gray (shrink-to-fit between paragraphs). The
//!      original pages — figures included — are untouched.
//!    - **replace**: each paragraph is covered by a background-colored rect
//!      on its own page and the translation drawn in the same box.
//!
//! ## Scope (v1)
//! Scanned PDFs without a text layer and encrypted PDFs fail with a clear
//! error. Original text color is not recoverable (the extractor does not
//! expose it) and is assumed near-black; backgrounds are assumed white.
//! Rendering needs one CJK-capable OpenType font on disk — see
//! [`PdfFont::shared`]; in the Docker images one is bundled/installed.
//! The font is embedded as a CID/Type0 font (Identity-H), **subset to the
//! glyphs actually drawn** (a full Noto CJK face is ~16 MB; a book needs a
//! few hundred KB), with glyph ids from the font's own cmap plus a generated
//! ToUnicode map and ActualText for logical text. Unicode shaping, bidi and
//! grapheme-safe line breaking live in `pdf_text`; optional fallback fonts cover
//! additional scripts. The primary face is shared per process.

use crate::format::{Document, OutputMode, Segment, SegmentId, Strategy};
use anyhow::{anyhow, bail, Context, Result};
use lopdf::{Object, ObjectId};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

// ── style constants ─────────────────────────────────────────────────────────

/// Translation font size relative to the paragraph's original size.
const TR_SIZE_RATIO: f64 = 0.92;
/// Translation line spacing, per font size.
const TR_LEADING: f64 = 1.42;
/// Smallest font size we are willing to shrink a translation to.
const TR_MIN_SIZE: f64 = 4.5;
/// Translation color on interleaved pages (matches epub's `.hy-zh` #3a3a3a).
const TR_COLOR: (u8, u8, u8) = (58, 58, 58);
/// Translation color in replace mode (near-black, like body text).
const REPLACE_COLOR: (u8, u8, u8) = (17, 17, 17);
/// Original-page marker color (a muted gray, matches epub's #8aa accent hue).
const MARKER_COLOR: (u8, u8, u8) = (122, 136, 136);
/// Font size of the `[p. N]` marker on translation pages.
const MARKER_SIZE: f64 = 7.0;
/// Ascent fraction used to map a baseline to the visual top of its line.
const ASCENT: f64 = 0.82;
/// Descent fraction used to map a baseline to the visual bottom of its line.
const DESCENT: f64 = 0.24;
/// Bottom margin assumed when fitting a page's last paragraph.
const PAGE_BOTTOM_MARGIN: f64 = 22.0;

mod extract;
mod font;
mod layout;
mod render;
mod sfnt;
#[cfg(test)]
mod tests;
mod text;
mod widths;

use extract::{GeoCollector, MediaBoxGeo};
pub use font::PdfFont;
use layout::{build_paragraphs, merge_across_pages, page_cols, page_cols_if, PageCols, Para};
use render::*;
use widths::normalize_cid_widths;

// ── document IR ─────────────────────────────────────────────────────────────

struct PageData {
    media: MediaBoxGeo,
    cols: PageCols,
    paras: Vec<Para>,
}

pub struct PdfDoc {
    fonts: text::Fonts,
    /// Original file bytes; `write` re-parses them with lopdf and edits in
    /// place, so the parsed form isn't held while translations are in flight.
    raw: Vec<u8>,
    pages: Vec<PageData>,
    /// Dense [`SegmentId`] → (page index, paragraph index).
    seg_index: Vec<(usize, usize)>,
}

impl PdfDoc {
    pub fn open(path: &Path) -> Result<Self> {
        let raw = fs::read(path).with_context(|| format!("read pdf {}", path.display()))?;
        let mut doc = lopdf::Document::load_mem(&raw)
            .map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
        if doc.is_encrypted() {
            bail!(
                "{} is encrypted; encrypted PDFs are not supported",
                path.display()
            );
        }
        normalize_cid_widths(&mut doc);
        let mut collector = GeoCollector::default();
        // pdf-extract panics on some malformed PDFs; contain that to this file
        // instead of the whole batch run.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::output_doc(&doc, &mut collector)
        }))
        .map_err(|p| {
            let reason = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".to_string());
            anyhow!(
                "pdf extractor panicked while parsing {}: {reason}",
                path.display()
            )
        })?
        .map_err(|e| anyhow!("extract text from {}: {e}", path.display()))?;

        let mut pages: Vec<(MediaBoxGeo, Vec<Para>)> = collector
            .pages
            .into_iter()
            .map(|(media, lines)| {
                if std::env::var_os("FERRYMAN_PDF_DEBUG").is_some() {
                    let cols = page_cols(&lines);
                    tracing::debug!(?media, ?cols, "pdf page geometry");
                    for l in &lines {
                        tracing::debug!(
                            "  line y={:.1} x0={:.1} x1={:.1} size={:.1} gap={:.1} {:?}",
                            l.y,
                            l.x0,
                            l.x1,
                            l.size,
                            l.max_gap,
                            &l.text[..l.text.len().min(50)]
                        );
                    }
                }
                (media, build_paragraphs(lines))
            })
            .collect();
        merge_across_pages(&mut pages);

        let mut any_text = false;
        let mut page_data = Vec::with_capacity(pages.len());
        let mut seg_index = Vec::new();
        for (media, paras) in pages {
            let cols = page_cols_if(&paras).unwrap_or(PageCols {
                x0: media.0,
                x1: media.2,
                leading: 13.5,
            });
            for (pi, p) in paras.iter().enumerate() {
                if p.translatable && !p.merged_up {
                    seg_index.push((page_data.len(), pi));
                    any_text = true;
                }
            }
            page_data.push(PageData { media, cols, paras });
        }
        if !any_text {
            bail!(
                "no extractable text in {} (scanned/image-only PDF?)",
                path.display()
            );
        }
        // Resolve configured fallbacks before any translation requests. Broken
        // font paths must not surface only after the document is translated.
        let fonts = text::Fonts::new(PdfFont::shared()?.clone())?;
        tracing::debug!(
            pages = page_data.len(),
            paragraphs = page_data.iter().map(|p| p.paras.len()).sum::<usize>(),
            translatable = seg_index.len(),
            "parsed pdf"
        );
        Ok(PdfDoc {
            fonts,
            raw,
            pages: page_data,
            seg_index,
        })
    }
}

impl Document for PdfDoc {
    fn format_name(&self) -> &'static str {
        "pdf"
    }

    fn segments(&self) -> Vec<Segment> {
        self.seg_index
            .iter()
            .enumerate()
            .map(|(id, &(pi, xi))| Segment {
                id,
                text: self.pages[pi].paras[xi].text.clone(),
            })
            .collect()
    }

    fn write(
        &mut self,
        translations: &[(SegmentId, String)],
        out: &Path,
        mode: OutputMode,
    ) -> Result<()> {
        // Dense segment id → (page, para) → translation text.
        let mut tr_by_para: HashMap<(usize, usize), &str> = HashMap::new();
        for (id, tr) in translations {
            if let Some(&(p, x)) = self.seg_index.get(*id) {
                let mut fragments = vec![(p, x)];
                for (pi, page) in self.pages.iter().enumerate() {
                    for (xi, para) in page.paras.iter().enumerate() {
                        if para.merged_into == Some((p, x)) {
                            fragments.push((pi, xi));
                        }
                    }
                }
                let weights: Vec<usize> = fragments
                    .iter()
                    .map(|&(pi, xi)| {
                        self.pages[pi].paras[xi]
                            .lines
                            .iter()
                            .map(|line| line.text.len())
                            .sum::<usize>()
                            .max(1)
                    })
                    .collect();
                let pieces = split_page_translation(tr, &weights);
                for (position, piece) in fragments.into_iter().zip(pieces) {
                    tr_by_para.insert(position, piece);
                }
            }
        }

        let mut doc = lopdf::Document::load_mem(&self.raw).context("re-parse pdf for writing")?;
        let fonts = &self.fonts;
        let mut layouts = HashMap::new();
        for (&(page_index, para_index), &translation) in &tr_by_para {
            let page = &self.pages[page_index];
            let para = &page.paras[para_index];
            let bottom = if mode == OutputMode::Replace {
                para.bot
            } else {
                page.paras
                    .get(para_index + 1)
                    .map(|next| next.top)
                    .unwrap_or(page.media.1 + PAGE_BOTTOM_MARGIN)
            };
            layouts.insert(
                (page_index, para_index),
                fonts.layout(
                    translation,
                    (page.cols.x1 - para.x0).max(para.size * 4.0),
                    (para.top - bottom).max(para.size * 0.8),
                    para.size * TR_SIZE_RATIO,
                )?,
            );
        }
        let markers: Vec<_> = (1..=self.pages.len())
            .map(|page| fonts.shape(&format!("[p. {page}]")))
            .collect::<Result<_>>()?;
        let used = text::collect_used(
            layouts
                .values()
                .flat_map(|layout| layout.lines.iter())
                .chain(markers.iter()),
            fonts.fonts.len(),
        );
        let mut embedded = BTreeMap::new();
        for (index, (font, used)) in fonts.fonts.iter().zip(&used).enumerate() {
            if used.is_empty() {
                continue;
            }
            let face = ttf_parser::Face::parse(&font.standalone, 0)
                .map_err(|error| anyhow!("parse PDF font: {error:?}"))?;
            let (key, id, remap) = embed_font(&mut doc, font, &face, used, index);
            embedded.insert(index, EmbeddedFont { key, id, remap });
        }

        let pages_in_order: Vec<ObjectId> = doc.get_pages().into_values().collect();
        anyhow::ensure!(
            pages_in_order.len() == self.pages.len(),
            "page tree changed between open and write ({} vs {})",
            pages_in_order.len(),
            self.pages.len()
        );

        match mode {
            OutputMode::Bilingual => {
                // Find each original page's parent node first (new pages
                // must point their /Parent at the node holding the original).
                let mut parent_of: HashMap<ObjectId, ObjectId> = HashMap::new();
                let root = doc
                    .catalog()
                    .and_then(|c| c.get(b"Pages"))
                    .and_then(Object::as_reference)
                    .context("page tree root")?;
                walk_page_tree(&doc, root, &mut parent_of)?;

                let mut insert_after: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
                for (i, &page_id) in pages_in_order.iter().enumerate() {
                    let page = &self.pages[i];
                    let mut cb = ContentBuilder::new();
                    for (xi, para) in page.paras.iter().enumerate() {
                        let Some(layout) = layouts.get(&(i, xi)) else {
                            continue;
                        };
                        draw_translation(&mut cb, para, layout, TR_COLOR, &embedded);
                    }
                    if cb.has_body {
                        // Small centered marker so the mirrored page is
                        // identifiable while scrolling.
                        let marker = &markers[i];
                        let (x, y) = (
                            (page.media.0 + page.media.2) / 2.0 - marker.width * MARKER_SIZE / 2.0,
                            page.media.3 - MARKER_SIZE * 1.4,
                        );
                        cb.shaped(x, y, MARKER_SIZE, 0.0, marker, MARKER_COLOR, &embedded);

                        let rotate = doc
                            .get_dictionary(page_id)
                            .ok()
                            .and_then(|d| inherited(&doc, d, b"Rotate").cloned())
                            .and_then(|o| o.as_i64().ok());
                        let new_id = make_translation_page(
                            &mut doc,
                            parent_of.get(&page_id).copied().unwrap_or(root),
                            page.media,
                            rotate,
                            &embedded,
                            cb.finish(),
                        )?;
                        insert_after.insert(page_id, vec![new_id]);
                    }
                }
                splice_translation_pages(&mut doc, root, &insert_after)?;
            }
            OutputMode::Replace => {
                for (i, &page_id) in pages_in_order.iter().enumerate() {
                    let page = &self.pages[i];
                    let mut cb = ContentBuilder::new();
                    for (xi, para) in page.paras.iter().enumerate() {
                        let Some(layout) = layouts.get(&(i, xi)) else {
                            continue;
                        };
                        // Cover the original paragraph, then draw the
                        // translation in the same box.
                        cb.fill_rect(
                            para.x0 - 1.0,
                            para.bot - 1.0,
                            para.x1 - para.x0 + 2.0,
                            para.top - para.bot + 2.0,
                        );
                        draw_translation(&mut cb, para, layout, REPLACE_COLOR, &embedded);
                    }
                    if cb.has_body {
                        doc.add_page_contents(page_id, cb.finish())?;
                        for font in embedded.values() {
                            add_font_to_page(&mut doc, page_id, &font.key, font.id)?;
                        }
                    }
                }
            }
        }

        doc.save(out)
            .with_context(|| format!("write pdf {}", out.display()))?;
        Ok(())
    }

    fn strategy(&self) -> Strategy {
        // Paragraphs are complete semantic units (like docx paragraphs and
        // epub blocks): translate each on its own, no cross-segment flow to
        // preserve.
        Strategy::Independent
    }
}

// Distribute a translated cross-page paragraph over its original page slots.
// UAX #14 boundaries keep graphemes/words intact whenever a break is available.
fn split_page_translation<'a>(text: &'a str, weights: &[usize]) -> Vec<&'a str> {
    let total: usize = weights.iter().sum();
    let mut cumulative = 0;
    let mut start = 0;
    let mut pieces = Vec::new();
    for (index, weight) in weights.iter().enumerate() {
        cumulative += weight;
        let end = if index + 1 == weights.len() {
            text.len()
        } else {
            let target = text.len().saturating_mul(cumulative) / total.max(1);
            unicode_linebreak::linebreaks(text)
                .map(|(end, _)| end)
                .filter(|&end| end >= start)
                .min_by_key(|&end| end.abs_diff(target))
                .unwrap_or(text.len())
        };
        pieces.push(text[start..end].trim());
        start = end;
    }
    pieces
}
