use super::extract::{rot, MediaBoxGeo};
use super::font::PdfFont;
use super::layout::Para;
use super::{text, ASCENT, TR_LEADING};
use anyhow::{Context, Result};
use lopdf::{Dictionary, Object, ObjectId, Stream};
use std::collections::{BTreeMap, HashMap};
use subsetter::GlyphRemapper;

// ── content stream building ─────────────────────────────────────────────────

/// Number formatting for content streams: fixed 2 decimals, never scientific.
pub(super) fn pdfnum(x: f64) -> String {
    format!("{x:.2}")
}

pub(super) struct EmbeddedFont {
    pub(super) key: String,
    pub(super) id: ObjectId,
    pub(super) remap: BTreeMap<u16, u16>,
}
pub(super) struct ContentBuilder {
    out: Vec<u8>,
    pub(super) has_body: bool,
}

impl ContentBuilder {
    pub(super) fn new() -> Self {
        Self {
            out: Vec::new(),
            has_body: false,
        }
    }
    pub(super) fn finish(self) -> Vec<u8> {
        self.out
    }

    /// Explicit glyph positions preserve shaping advances, mark offsets and RTL
    /// order. ActualText retains the logical Unicode string for copying/search.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn shaped(
        &mut self,
        x: f64,
        y: f64,
        size: f64,
        angle: f64,
        line: &text::Line,
        rgb: (u8, u8, u8),
        fonts: &BTreeMap<usize, EmbeddedFont>,
    ) {
        let actual: String = line
            .text
            .encode_utf16()
            .map(|unit| format!("{unit:04X}"))
            .collect();
        self.out
            .extend_from_slice(format!("/Span << /ActualText <FEFF{actual}> >> BDC\n").as_bytes());
        let (sin, cos) = angle.sin_cos();
        let mut advance = 0.0;
        for glyph in &line.glyphs {
            let font = &fonts[&glyph.font];
            let gid = font.remap.get(&glyph.id).copied().unwrap_or(glyph.id);
            let (dx, dy) = rot(
                angle,
                (advance + glyph.x_offset) * size,
                glyph.y_offset * size,
            );
            let operation = format!(
                "q {} {} {} rg BT /{} {} Tf {} {} {} {} {} {} Tm <{gid:04X}> Tj ET Q\n",
                rgb.0 as f32 / 255.0,
                rgb.1 as f32 / 255.0,
                rgb.2 as f32 / 255.0,
                font.key,
                pdfnum(size),
                pdfnum(cos),
                pdfnum(sin),
                pdfnum(-sin),
                pdfnum(cos),
                pdfnum(x + dx),
                pdfnum(y + dy)
            );
            self.out.extend_from_slice(operation.as_bytes());
            advance += glyph.advance;
        }
        self.out.extend_from_slice(b"EMC\n");
        self.has_body = true;
    }

    /// A background-colored rectangle covering the original paragraph.
    pub(super) fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
        let line = format!(
            "q 1 1 1 rg {} {} {} {} re f Q\n",
            pdfnum(x),
            pdfnum(y),
            pdfnum(w),
            pdfnum(h)
        );
        self.out.extend_from_slice(line.as_bytes());
        self.has_body = true;
    }
}

pub(super) fn draw_translation(
    cb: &mut ContentBuilder,
    para: &Para,
    layout: &text::Layout,
    rgb: (u8, u8, u8),
    fonts: &BTreeMap<usize, EmbeddedFont>,
) {
    let angle = para.first().angle;
    let (mut x, mut y) = rot(angle, para.x0, para.top - layout.size * ASCENT);
    for line in &layout.lines {
        cb.shaped(x, y, layout.size, angle, line, rgb, fonts);
        let (dx, dy) = rot(angle, 0.0, -layout.size * TR_LEADING);
        x += dx;
        y += dy;
    }
}

// ── font embedding + page tree surgery (lopdf) ──────────────────────────────

/// Embed the shared font as a CID/Type0 (Identity-H) font, subset to the
/// glyphs actually drawn, and return `(resource key, object id, old→new
/// glyph-id map)`. `used` maps shaped glyph ids to their Unicode clusters
/// in the *original* face (drives subsetting, /W widths and the generated
/// ToUnicode map). [`subsetter`] renumbers glyphs contiguously — feeding the
/// ids ascending keeps `.notdef` at 0 — so every gid written into content
/// streams, /W and ToUnicode must go through the returned map. A full Noto
/// CJK face is ~16 MB; a book's worth of glyphs is a few hundred KB. If
/// subsetting fails the full face is embedded with an identity map.
pub(super) fn embed_font(
    doc: &mut lopdf::Document,
    font: &PdfFont,
    face: &ttf_parser::Face<'_>,
    used: &BTreeMap<u16, String>,
    index: usize,
) -> (String, ObjectId, BTreeMap<u16, u16>) {
    let font_name = format!("FerrymanFont{index}");
    let upem = face.units_per_em().max(1) as f64;
    let scale = |v: f64| (v / upem * 1000.0).round() as i64;

    let mut old_gids: Vec<u16> = used.keys().copied().collect();
    old_gids.sort_unstable();
    old_gids.dedup();
    let remapper = GlyphRemapper::new_from_glyphs_sorted(&old_gids);
    let (program, remap): (Vec<u8>, BTreeMap<u16, u16>) =
        match subsetter::subset(&font.standalone, 0, &remapper) {
            Ok(sub) => {
                let map = old_gids
                    .iter()
                    .map(|&old| (old, remapper.get(old).unwrap_or(old)))
                    .collect();
                (sub, map)
            }
            Err(_) => (font.standalone.to_vec(), BTreeMap::new()), // identity
        };

    // Font program stream.
    let mut ff_dict = Dictionary::new();
    if font.cff {
        ff_dict.set("Subtype", "OpenType");
    } else {
        ff_dict.set("Length1", program.len() as i64);
    }
    let mut ff_stream = Stream::new(ff_dict, program);
    let _ = ff_stream.compress();
    let ff_id = doc.add_object(ff_stream);

    // Font descriptor.
    let b = face.tables().head.global_bbox;
    let (x_min, y_min, x_max, y_max) = (
        b.x_min as f64,
        b.y_min as f64,
        b.x_max as f64,
        b.y_max as f64,
    );
    let mut fd = Dictionary::new();
    fd.set("Type", "FontDescriptor");
    fd.set("FontName", font_name.as_str());
    fd.set("Flags", 4_i64); // symbolic
    fd.set(
        "FontBBox",
        vec![
            Object::Integer(scale(x_min)),
            Object::Integer(scale(y_min)),
            Object::Integer(scale(x_max)),
            Object::Integer(scale(y_max)),
        ],
    );
    fd.set("ItalicAngle", 0_i64);
    fd.set("Ascent", scale(face.ascender() as f64));
    fd.set("Descent", scale(face.descender() as f64));
    fd.set(
        "CapHeight",
        scale(face.capital_height().unwrap_or(face.ascender()) as f64),
    );
    fd.set("StemV", 80_i64);
    fd.set(
        if font.cff { "FontFile3" } else { "FontFile2" },
        Object::Reference(ff_id),
    );
    let fd_id = doc.add_object(Object::Dictionary(fd));

    // CIDFont (descendant), with per-glyph widths for the glyphs we use.
    // /W takes `c [w…]` or `c_first c_last w` entries — one single-glyph
    // range per used glyph, ascending (the spec has no `cid w` pairs form).
    // Advances come from the original face (subsetting keeps metrics);
    // the ids are the subset's new gids.
    let mut by_gid: BTreeMap<u16, i64> = BTreeMap::new();
    for &old in &old_gids {
        let adv = face
            .glyph_hor_advance(ttf_parser::GlyphId(old))
            .unwrap_or((upem * 0.55) as u16) as f64;
        by_gid.insert(remap.get(&old).copied().unwrap_or(old), scale(adv));
    }
    let mut w = Vec::new();
    for (&g, &adv) in &by_gid {
        w.push(Object::Integer(g as i64));
        w.push(Object::Integer(g as i64));
        w.push(Object::Integer(adv));
    }
    let mut cid = Dictionary::new();
    cid.set("Type", "Font");
    cid.set(
        "Subtype",
        if font.cff {
            "CIDFontType0"
        } else {
            "CIDFontType2"
        },
    );
    cid.set("BaseFont", font_name.as_str());
    let mut csi = Dictionary::new();
    // Registry/Ordering are *strings* in the spec — lopdf's &str converts to
    // a Name, which poppler rejects ("Invalid CIDSystemInfo dictionary").
    csi.set("Registry", Object::string_literal("Adobe"));
    csi.set("Ordering", Object::string_literal("Identity"));
    csi.set("Supplement", 0_i64);
    cid.set("CIDSystemInfo", Object::Dictionary(csi));
    cid.set("FontDescriptor", Object::Reference(fd_id));
    cid.set("DW", 1000_i64);
    cid.set("W", Object::Array(w));
    if !font.cff {
        cid.set("CIDToGIDMap", "Identity");
    }
    let cid_id = doc.add_object(Object::Dictionary(cid));

    // ToUnicode CMap: gid → UTF-16BE, so the drawn text stays extractable.
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let entries: Vec<(&u16, &String)> = used.iter().collect();
    for chunk in entries.chunks(100) {
        cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (old, text) in chunk {
            let g = remap.get(old).copied().unwrap_or(**old);
            let mut hex = String::new();
            for unit in text.encode_utf16() {
                hex.push_str(&format!("{unit:04X}"));
            }
            cmap.push_str(&format!("<{g:04X}> <{hex}>\n"));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    let tu_stream = Stream::new(Dictionary::new(), cmap.into_bytes());
    let tu_id = doc.add_object(tu_stream);

    // Type0 wrapper.
    let mut t0 = Dictionary::new();
    t0.set("Type", "Font");
    t0.set("Subtype", "Type0");
    t0.set("BaseFont", font_name.as_str());
    t0.set("Encoding", "Identity-H");
    t0.set("DescendantFonts", vec![Object::Reference(cid_id)]);
    t0.set("ToUnicode", Object::Reference(tu_id));
    let t0_id = doc.add_object(Object::Dictionary(t0));

    (format!("FerHyZH{index}"), t0_id, remap)
}

/// Record each leaf page's parent Pages node id by walking the tree.
pub(super) fn walk_page_tree(
    doc: &lopdf::Document,
    node_id: ObjectId,
    parent_of: &mut HashMap<ObjectId, ObjectId>,
) -> Result<()> {
    let node = doc.get_dictionary(node_id)?;
    let kids = node
        .get(b"Kids")
        .and_then(Object::as_array)
        .context("page tree node without Kids")?
        .clone();
    for kid in kids {
        let id = kid
            .as_reference()
            .context("Kids entry is not a reference")?;
        let is_pages = doc
            .get_dictionary(id)
            .ok()
            .and_then(|d| d.get(b"Type").ok())
            .and_then(|o| o.as_name().ok())
            .is_some_and(|t| t == b"Pages");
        if is_pages {
            walk_page_tree(doc, id, parent_of)?;
        } else {
            parent_of.insert(id, node_id);
        }
    }
    Ok(())
}

/// Effective (nearest ancestor) value of a page attribute, following
/// /Parent links.
pub(super) fn inherited<'a>(
    doc: &'a lopdf::Document,
    mut dict: &'a Dictionary,
    key: &[u8],
) -> Option<&'a Object> {
    loop {
        if let Ok(v) = dict.get(key) {
            return Some(v);
        }
        let parent = dict.get(b"Parent").and_then(Object::as_reference).ok()?;
        dict = doc.get_dictionary(parent).ok()?;
    }
}

/// Create the translation page (same size as the original, mirrored layout)
/// and return its object id — not yet linked into the page tree.
pub(super) fn make_translation_page(
    doc: &mut lopdf::Document,
    parent: ObjectId,
    media: MediaBoxGeo,
    rotate: Option<i64>,
    fonts: &BTreeMap<usize, EmbeddedFont>,
    content: Vec<u8>,
) -> Result<ObjectId> {
    let mut stream = Stream::new(Dictionary::new(), content);
    let _ = stream.compress();
    let contents_id = doc.add_object(stream);

    let mut dict = Dictionary::new();
    dict.set("Type", "Page");
    dict.set("Parent", Object::Reference(parent));
    dict.set(
        "MediaBox",
        vec![
            Object::Real(media.0 as f32),
            Object::Real(media.1 as f32),
            Object::Real(media.2 as f32),
            Object::Real(media.3 as f32),
        ],
    );
    // Match the original page's rotation so the viewer shows both the same.
    if let Some(r) = rotate {
        dict.set("Rotate", r);
    }
    let mut font = Dictionary::new();
    for embedded in fonts.values() {
        font.set(embedded.key.as_str(), Object::Reference(embedded.id));
    }
    let mut res = Dictionary::new();
    res.set("Font", Object::Dictionary(font));
    dict.set("Resources", Object::Dictionary(res));
    dict.set("Contents", Object::Reference(contents_id));
    Ok(doc.add_object(Object::Dictionary(dict)))
}

/// Splice translation pages into the page tree right after their original
/// pages, recomputing /Count along the way. Returns the leaf count.
pub(super) fn splice_translation_pages(
    doc: &mut lopdf::Document,
    node_id: ObjectId,
    insert_after: &HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<usize> {
    let kids: Vec<Object> = doc
        .get_dictionary(node_id)?
        .get(b"Kids")
        .and_then(Object::as_array)
        .context("Kids")?
        .clone();
    let mut new_kids = Vec::with_capacity(kids.len());
    let mut count = 0usize;
    for kid in kids {
        let id = kid
            .as_reference()
            .context("Kids entry is not a reference")?;
        let is_pages = doc
            .get_dictionary(id)
            .ok()
            .and_then(|d| d.get(b"Type").ok())
            .and_then(|o| o.as_name().ok())
            .is_some_and(|t| t == b"Pages");
        if is_pages {
            // Keep the intermediate node: only its own Kids/Count change.
            new_kids.push(kid);
            count += splice_translation_pages(doc, id, insert_after)?;
        } else {
            count += 1;
            new_kids.push(kid);
            if let Some(new) = insert_after.get(&id) {
                for nid in new {
                    new_kids.push(Object::Reference(*nid));
                    count += 1;
                }
            }
        }
    }
    let node = doc.get_object_mut(node_id).and_then(Object::as_dict_mut)?;
    node.set("Kids", Object::Array(new_kids));
    node.set("Count", count as i64);
    Ok(count)
}

/// Add the embedded font to a page's /Resources without breaking inherited
/// ones. Inherited resources are cloned shallowly onto the page first (an
/// extra font entry in a shared dict object harms nobody, but pages with no
/// Resources of their own must not hijack the ancestor's dict). Direct
/// dictionaries are normalized into their own object so one mutation point
/// works for everything.
pub(super) fn add_font_to_page(
    doc: &mut lopdf::Document,
    page_id: ObjectId,
    key: &str,
    font_id: ObjectId,
) -> Result<()> {
    // 1. The page's own Resources dictionary object.
    let own = doc.get_dictionary(page_id)?.get(b"Resources").ok().cloned();
    let res_id = match own {
        Some(Object::Reference(r)) => r,
        own => {
            let base = match own {
                Some(Object::Dictionary(d)) => d,
                _ => inherited(doc, doc.get_dictionary(page_id)?, b"Resources")
                    .and_then(|o| match o {
                        Object::Reference(id) => doc.get_dictionary(*id).ok(),
                        other => other.as_dict().ok(),
                    })
                    .cloned()
                    .context("page tree has no Resources")?,
            };
            let id = doc.add_object(Object::Dictionary(base));
            doc.get_object_mut(page_id)?
                .as_dict_mut()?
                .set("Resources", Object::Reference(id));
            id
        }
    };
    // 2. The Resources' Font dictionary object (created if absent).
    let font_own = doc.get_dictionary(res_id)?.get(b"Font").ok().cloned();
    let font_dict_id = match font_own {
        Some(Object::Reference(r)) => r,
        own => {
            let base = match own {
                Some(Object::Dictionary(d)) => d,
                _ => Dictionary::new(),
            };
            let id = doc.add_object(Object::Dictionary(base));
            doc.get_object_mut(res_id)?
                .as_dict_mut()?
                .set("Font", Object::Reference(id));
            id
        }
    };
    // 3. The font entry itself.
    doc.get_object_mut(font_dict_id)?
        .as_dict_mut()?
        .set(key, Object::Reference(font_id));
    Ok(())
}
