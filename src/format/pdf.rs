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
use lopdf::{Dictionary, Object, ObjectId, Stream};
use pdf_extract::OutputDev;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use subsetter::GlyphRemapper;

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

// ── font discovery + sharing ────────────────────────────────────────────────

/// A CJK-capable OpenType font shared by every PDF document in the process.
#[derive(Clone)]
pub struct PdfFont {
    /// The selected face re-packed as a standalone sfnt (`Arc` so all
    /// clones share one copy; faces are extracted out of `.ttc` collections
    /// because a PDF FontFile may not be a collection).
    standalone: Arc<Vec<u8>>,
    /// CFF-flavored OpenType (chooses CIDFontType0/FontFile3 embedding, vs
    /// TrueType outlines' CIDFontType2/FontFile2).
    cff: bool,
}

impl PdfFont {
    /// The process-wide shared font, loaded on first use. The first existing
    /// file wins: `$FERRYMAN_PDF_FONT`, the Docker image's `/app/fonts`, then
    /// the usual distro locations for Noto CJK / WenQuanYi / Droid fallback.
    pub fn shared() -> Result<&'static Self> {
        static SHARED: OnceLock<PdfFont> = OnceLock::new();
        if let Some(f) = SHARED.get() {
            return Ok(f);
        }
        let candidates = candidate_font_paths();
        let path = candidates.iter().find(|p| p.is_file()).ok_or_else(|| {
            anyhow!(
                "no PDF font found (looked in: {}); install fonts-noto-cjk \
                 or point FERRYMAN_PDF_FONT at a CJK OpenType font (.ttc/.otf/.ttf)",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let font = Self::load(path).with_context(|| format!("load pdf font {}", path.display()))?;
        Ok(SHARED.get_or_init(|| font))
    }

    fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let index = preferred_face_index(&data);
        let standalone = Arc::new(extract_face(&data, index));
        let cff = face_is_cff(&standalone);
        Ok(PdfFont { standalone, cff })
    }

    /// A krilla font handle over the standalone face (test fixtures only).
    #[cfg(test)]
    fn krilla(&self) -> Option<krilla::text::Font> {
        krilla::text::Font::new(Arc::clone(&self.standalone).into(), 0)
    }
}

/// Where to look for a CJK font, most specific first. See [`PdfFont::shared`].
fn candidate_font_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(env) = std::env::var_os("FERRYMAN_PDF_FONT") {
        if !env.is_empty() {
            paths.push(PathBuf::from(env));
        }
    }
    paths.extend(
        [
            // Docker image (scripts/build-release.sh bundles it here).
            "/app/fonts/NotoSansCJK-Regular.ttc",
            // Debian/Ubuntu (fonts-noto-cjk).
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            // Fedora / Arch packaging of the same face.
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            // Serif variant: better fits book typography, still fine.
            "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
            // Common distro fallbacks.
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/droid/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/droid/DroidSansFallback.ttf",
        ]
        .map(PathBuf::from),
    );
    paths
}

/// Pick the face to use inside a `.ttc`: prefer the Simplified-Chinese face
/// (the default target language), then any CJK face, then face 0. Standalone
/// fonts parse only at index 0.
fn preferred_face_index(data: &[u8]) -> u32 {
    let mut first_cjk: Option<u32> = None;
    for i in 0.. {
        let Ok(face) = ttf_parser::Face::parse(data, i) else {
            break;
        };
        let name = face_name(&face);
        if name.contains("CJK SC") || name.contains(" SC") {
            return i;
        }
        if first_cjk.is_none() && name.contains("CJK") {
            first_cjk = Some(i);
        }
    }
    first_cjk.unwrap_or(0)
}

/// The font's English full face name ("" when absent), e.g. "Noto Sans CJK SC".
fn face_name(face: &ttf_parser::Face<'_>) -> String {
    face.names()
        .into_iter()
        .filter(|n| n.name_id == ttf_parser::name::name_id::FULL_NAME && n.is_unicode())
        .find_map(|n| n.to_string())
        .unwrap_or_default()
}

// ── sfnt plumbing (TTC face extraction, flavor detection) ───────────────────

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}
fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// One table directory record of an sfnt font.
struct TableRec {
    offset: usize,
    len: usize,
}

/// Read the table directory of the sfnt starting at `start`.
fn sfnt_tables(d: &[u8], start: usize) -> Option<Vec<([u8; 4], TableRec)>> {
    if d.len() < start + 12 {
        return None;
    }
    let num = be16(d, start + 4) as usize;
    if d.len() < start + 12 + num * 16 {
        return None;
    }
    let mut out = Vec::with_capacity(num);
    for i in 0..num {
        let r = start + 12 + i * 16;
        let tag: [u8; 4] = d.get(r..r + 4)?.try_into().ok()?;
        out.push((
            tag,
            TableRec {
                offset: be32(d, r + 8) as usize,
                len: be32(d, r + 12) as usize,
            },
        ));
    }
    Some(out)
}

/// Copy one face out of a TrueType/OpenType collection into a standalone
/// sfnt font (tables byte-identical, only the directory is rebuilt — a PDF
/// FontFile must be a single-face font program). Non-collections return a
/// plain copy of the bytes.
fn extract_face(data: &[u8], index: u32) -> Vec<u8> {
    let face_off = (|| {
        if data.len() < 12 || &data[..4] != b"ttcf" {
            return None;
        }
        let num = be32(data, 8);
        if index >= num {
            return None;
        }
        Some(be32(data, 12 + 4 * index as usize) as usize)
    })();
    let Some(face_off) = face_off else {
        return data.to_vec();
    };
    let Some(tables) = sfnt_tables(data, face_off) else {
        return data.to_vec();
    };

    // 12-byte sfnt header + one 16-byte record per table, tables 4-aligned.
    let mut head = Vec::with_capacity(12 + tables.len() * 16);
    head.extend_from_slice(&data[face_off..face_off + 12]);
    let mut body = Vec::new();
    let mut recs: Vec<(&[u8; 4], u32, u32)> = Vec::with_capacity(tables.len());
    for (tag, rec) in &tables {
        if rec.offset + rec.len > data.len() {
            return data.to_vec(); // malformed directory; embed raw bytes
        }
        let new_off = 12 + tables.len() * 16 + body.len();
        recs.push((tag, new_off as u32, rec.len as u32));
        body.extend_from_slice(&data[rec.offset..rec.offset + rec.len]);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    }
    for (tag, off, len) in recs {
        head.extend_from_slice(tag);
        head.extend_from_slice(&0u32.to_be_bytes()); // checksum: stale is fine for PDF
        head.extend_from_slice(&off.to_be_bytes());
        head.extend_from_slice(&len.to_be_bytes());
    }
    let mut out = head;
    out.extend_from_slice(&body);
    out
}

/// Whether the (standalone) font carries CFF outlines ('OTTO' sfnt or a
/// `CFF ` table) vs TrueType `glyf` outlines.
fn face_is_cff(f: &[u8]) -> bool {
    if f.len() >= 4 && &f[..4] == b"OTTO" {
        return true;
    }
    sfnt_tables(f, 0)
        .map(|t| t.iter().any(|(tag, _)| tag == b"CFF "))
        .unwrap_or(false)
}

// ── /W width-array normalization ─────────────────────────────────────────────

/// Numeric value of a PDF object (Integer or Real), as f64.
fn obj_num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// Rewrite every CIDFont `/W` array into the `cid [w…]` form before
/// extraction. pdf-extract 0.12 mis-parses the other legal forms: the
/// `c_first c_last w` range form reads the *first cid* as the width and its
/// loop skips `c_last`, so single-glyph ranges (`5 5 742`, what krilla and
/// various producers write) contribute nothing and every glyph falls back to
/// `/DW` — often 0 — collapsing all x geometry. Parsing spec-correctly here
/// and re-emitting as array form fixes extraction without forking the crate.
fn normalize_cid_widths(doc: &mut lopdf::Document) {
    let ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, o)| {
            o.as_dict()
                .ok()
                .is_some_and(|d| d.has(b"CIDSystemInfo") && d.get(b"W").is_ok())
        })
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        let Ok(w_arr) = doc
            .get_object(id)
            .and_then(|o| o.as_dict().and_then(|d| d.get(b"W")))
        else {
            continue;
        };
        let entries = match w_arr {
            Object::Reference(r) => doc
                .get_object(*r)
                .and_then(|o| o.as_array().cloned())
                .unwrap_or_default(),
            Object::Array(a) => a.clone(),
            _ => continue,
        };
        // Parse spec-correctly (PDF 32000-1 Table 120): `c [w…]` or
        // `c_first c_last w`.
        let mut widths: BTreeMap<i64, f64> = BTreeMap::new();
        let mut i = 0;
        while i + 1 < entries.len() {
            let Ok(c) = entries[i].as_i64() else { break };
            if let Ok(wa) = entries[i + 1].as_array() {
                for (j, w) in wa.iter().enumerate() {
                    if let Some(v) = obj_num(w) {
                        widths.insert(c + j as i64, v);
                    }
                }
                i += 2;
            } else {
                let (Ok(last), Some(w)) = (
                    entries[i + 1].as_i64(),
                    entries.get(i + 2).and_then(obj_num),
                ) else {
                    break;
                };
                for g in c..=last {
                    widths.insert(g, w);
                }
                i += 3;
            }
        }
        if widths.is_empty() {
            continue;
        }
        // Re-emit as array-form entries over runs of consecutive cids
        // (≤100 widths per run, splitting long runs).
        let mut new_w: Vec<Object> = Vec::new();
        let mut run: Vec<(i64, f64)> = Vec::new();
        let flush_run = |run: &mut Vec<(i64, f64)>, new_w: &mut Vec<Object>| {
            for chunk in run.chunks(100) {
                new_w.push(Object::Integer(chunk[0].0));
                new_w.push(Object::Array(
                    chunk
                        .iter()
                        .map(|(_, v)| {
                            let rounded = v.round();
                            if (v - rounded).abs() < 1e-6 {
                                Object::Integer(rounded as i64)
                            } else {
                                Object::Real(*v as f32)
                            }
                        })
                        .collect(),
                ));
            }
            run.clear();
        };
        for (&g, &v) in &widths {
            if let Some(last) = run.last() {
                if last.0 + 1 != g || run.len() >= 100 {
                    flush_run(&mut run, &mut new_w);
                }
            }
            run.push((g, v));
        }
        flush_run(&mut run, &mut new_w);
        let _ = doc
            .get_object_mut(id)
            .and_then(|o| o.as_dict_mut())
            .map(|d| d.set(b"W", Object::Array(new_w)));
    }
}

// ── extraction-side text cleanup ────────────────────────────────────────────

/// Ligatures TeX-y PDFs decode to (U+FB00..); expand so the model sees plain
/// letters and the display font needs no ligature glyphs.
const LIGATURES: &[(char, &str)] = &[
    ('\u{FB00}', "ff"),
    ('\u{FB01}', "fi"),
    ('\u{FB02}', "fl"),
    ('\u{FB03}', "ffi"),
    ('\u{FB04}', "ffl"),
    ('\u{FB05}', "st"),
    ('\u{FB06}', "st"),
];

/// Clean one extracted line for translation and display: expand ligatures,
/// map exotic spaces to ' ', drop soft hyphens/control chars/BOM, collapse
/// table-of-contents dot leaders and space runs, trim.
fn normalize_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        // Dot-leader runs (". . . ." / "....") → a single space, so TOC lines
        // translate as "1.1. Openings 7" instead of dots soup. Four or more
        // dots (spaces allowed between) is a leader; shorter runs stay as
        // written (sentence periods, the literary ". . ." ellipsis).
        if c == '.' {
            let mut lookahead = chars.clone();
            let mut dots = 1usize;
            loop {
                match lookahead.next() {
                    Some('.') => dots += 1,
                    Some(' ') => {} // spaces between dots are part of the run
                    _ => break,
                }
            }
            if dots >= 4 {
                out.push(' ');
                // Consume the whole run: every dot and the spaces between
                // them, up to whatever followed it (the page number).
                while matches!(chars.peek(), Some('.') | Some(' ')) {
                    chars.next();
                }
                continue;
            }
        }
        match c {
            '\u{00AD}' | '\u{FEFF}' => {}               // soft hyphen, BOM
            '\u{00A0}' | '\u{2007}' | '\u{2008}'        // exotic spaces → ' '
            | '\u{2009}' | '\u{200A}' | '\u{202F}' => out.push(' '),
            c if (c as u32) < 0x20 || (0x7F..0xA0).contains(&(c as u32)) => {} // controls
            c => {
                if let Some((_, expansion)) = LIGATURES.iter().find(|(l, _)| *l == c) {
                    out.push_str(expansion);
                } else {
                    out.push(c);
                }
            }
        }
    }
    // Collapse space runs (never leading, never consecutive) and trim.
    let mut collapsed = String::with_capacity(out.len());
    for c in out.chars() {
        if c == ' ' {
            if !collapsed.is_empty() && !collapsed.ends_with(' ') {
                collapsed.push(' ');
            }
        } else {
            collapsed.push(c);
        }
    }
    collapsed.trim_end().to_string()
}

/// Whether a normalized paragraph carries translatable content: it must
/// contain at least one letter, so page numbers ("9"), section numbers
/// ("1.1.2.") and rules ("———") pass through untouched.
fn is_translatable(line: &str) -> bool {
    line.chars().any(char::is_alphabetic)
}

// ── geometric extraction (custom OutputDev) ─────────────────────────────────

/// Rotate `(x, y)` by `angle` radians (about the origin).
fn rot(angle: f64, x: f64, y: f64) -> (f64, f64) {
    let (s, c) = angle.sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// One positioned run of characters — a "visual line" piece, already rotated
/// into a left-to-right reading frame (`y` still points up). All geometry a
/// paragraph or a redraw needs lives here.
#[derive(Debug, Clone)]
struct RawLine {
    text: String,
    /// Left edge of the first character.
    x0: f64,
    /// Right edge of the last character (origin + advance).
    x1: f64,
    /// Baseline `y`.
    y: f64,
    /// Font size (CTM-scaled).
    size: f64,
    /// Text direction in PDF user space, radians (0 = upright, left→right).
    angle: f64,
    /// Largest gap between consecutive characters (a dot-leader or column
    /// split inside the line; wrapped body text never exceeds a word space).
    max_gap: f64,
}

impl RawLine {
    /// Estimated top of the line's ink.
    fn top(&self) -> f64 {
        self.y + self.size * ASCENT
    }
    /// Estimated bottom of the line's ink.
    fn bot(&self) -> f64 {
        self.y - self.size * DESCENT
    }
}

/// An [`OutputDev`] that keeps character geometry instead of flattening to
/// text: characters accumulate into [`RawLine`]s while the baseline, angle
/// and gaps are consistent; pages collect sorted (top→bottom, left→right).
#[derive(Default)]
struct GeoCollector {
    pages: Vec<(MediaBoxGeo, Vec<RawLine>)>,
    media: Option<MediaBoxGeo>,
    lines: Vec<RawLine>,
    buf: String,
    buf_x0: f64,
    buf_y: f64,
    buf_size: f64,
    buf_angle: f64,
    buf_max_gap: f64,
    /// Width of the whitespace run currently in progress (spaces and
    /// positioning jumps), committed to `buf_max_gap when ink resumes.
    run_ws: f64,
    last_x1: f64,
}

type MediaBoxGeo = (f64, f64, f64, f64); // llx lly urx ury

/// Whether a raw line contains a dot-leader run — four or more dots with
/// only spaces between them (`. . . .`, `....`). The dots are ink, so the
/// whitespace-run tracker can't see the leader as one gap; the text is the
/// only witness. Three or fewer dots stay ordinary punctuation.
fn has_dot_leader(raw: &str) -> bool {
    let mut dots = 0;
    for c in raw.chars() {
        match c {
            '.' | '·' => dots += 1,
            ' ' => {}
            _ => {
                if dots >= 4 {
                    return true;
                }
                dots = 0;
            }
        }
    }
    dots >= 4
}

impl GeoCollector {
    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = normalize_line(&self.buf);
        if !text.is_empty() {
            let max_gap = if has_dot_leader(&self.buf) {
                self.buf_size * 8.0 // a leader: never merges with anything
            } else {
                self.buf_max_gap
            };
            self.lines.push(RawLine {
                text,
                x0: self.buf_x0,
                x1: self.last_x1,
                y: self.buf_y,
                size: self.buf_size,
                angle: self.buf_angle,
                max_gap,
            });
        }
        self.buf.clear();
        self.buf_max_gap = 0.0;
        self.run_ws = 0.0;
    }
}

impl OutputDev for GeoCollector {
    fn begin_page(
        &mut self,
        _page_num: u32,
        media_box: &pdf_extract::MediaBox,
        _art_box: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), pdf_extract::OutputError> {
        self.flush();
        self.media = Some((media_box.llx, media_box.lly, media_box.urx, media_box.ury));
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), pdf_extract::OutputError> {
        self.flush();
        if let Some(media) = self.media.take() {
            // 0.5pt-wide y buckets keep pieces on the same baseline together.
            self.lines.sort_by(|a, b| {
                let ka = ((a.y * 2.0).round() as i64, a.x0);
                let kb = ((b.y * 2.0).round() as i64, b.x0);
                kb.0.cmp(&ka.0).then_with(|| ka.1.total_cmp(&kb.1))
            });
            self.pages.push((media, std::mem::take(&mut self.lines)));
        }
        Ok(())
    }

    fn output_character(
        &mut self,
        trm: &pdf_extract::Transform,
        width: f64,
        spacing: f64,
        font_size: f64,
        char: &str,
    ) -> Result<(), pdf_extract::OutputError> {
        if char.is_empty() {
            return Ok(());
        }
        // trm = Tsm × Tm × CTM; its translation is the glyph origin in user
        // space and its linear part encodes scale + rotation. The effective
        // size is the geometric mean scale; the angle orients reading.
        let det = trm.m11 * trm.m22 - trm.m12 * trm.m21;
        let size = font_size * det.abs().sqrt().max(1e-6);
        let angle = trm.m12.atan2(trm.m11);
        let (x, y) = rot(-angle, trm.m31, trm.m32); // reading frame
        let adv = width * size;

        let same_line = !self.buf.is_empty()
            && (y - self.buf_y).abs() <= size.max(self.buf_size) * 0.35
            && (angle - self.buf_angle).abs() <= 0.1
            && x >= self.last_x1 - size * 0.25
            && x - self.last_x1 <= size * 1.7; // wider than this = a column gap
        if !same_line {
            self.flush();
            self.buf_x0 = x;
            self.buf_y = y;
            self.buf_size = size;
            self.buf_angle = angle;
        } else if char == " " {
            // A space glyph: its advance (+ word/char spacing) is whitespace.
            self.run_ws += adv + spacing;
        } else {
            let gap = x - self.last_x1;
            if gap > size * 0.14 && !self.buf.ends_with(' ') {
                self.buf.push(' '); // a word gap the PDF placed with positioning
                self.run_ws += gap;
            }
            // The whitespace run ends at this glyph: a leader-sized run is a
            // dot-leader or a TOC title→page-number fill, not a word space.
            if self.run_ws > self.buf_max_gap {
                self.buf_max_gap = self.run_ws;
            }
            self.run_ws = 0.0;
        }
        self.buf.push_str(char);
        self.buf_size = self.buf_size.max(size);
        self.last_x1 = x + adv;
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), pdf_extract::OutputError> {
        Ok(()) // line boundaries come from geometry, not operators
    }
}

// ── paragraph reconstruction ────────────────────────────────────────────────

/// Column geometry a page's lines imply (medians are robust to headings and
/// strays).
#[derive(Debug, Clone, Copy)]
struct PageCols {
    x0: f64,
    x1: f64,
    leading: f64,
}

fn page_cols(lines: &[RawLine]) -> PageCols {
    let mut starts: Vec<f64> = lines.iter().map(|l| l.x0).collect();
    let mut sizes: Vec<f64> = lines.iter().map(|l| l.size).collect();
    starts.sort_by(|a, b| a.total_cmp(b));
    sizes.sort_by(|a, b| a.total_cmp(b));
    let mid = starts.len() / 2;
    let col_x0 = starts.get(mid).copied().unwrap_or(0.0);
    let col_x1 = lines.iter().map(|l| l.x1).fold(f64::MIN, f64::max);
    let body = sizes.get(mid).copied().unwrap_or(10.0);

    let mut deltas: Vec<f64> = lines
        .windows(2)
        .map(|w| w[0].y - w[1].y)
        .filter(|d| *d > body * 0.3 && *d < body * 3.0)
        .collect();
    deltas.sort_by(|a, b| a.total_cmp(b));
    // Lower-middle median: continuation leading is more common than
    // paragraph gaps, so when the count is even the smaller delta is the
    // likelier true leading.
    let leading = deltas
        .get(deltas.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(body * 1.35)
        .max(body * 0.9);
    PageCols {
        x0: col_x0,
        x1: col_x1,
        leading,
    }
}

/// Whether line `b` is the wrapped continuation of `l` (i.e. the same
/// paragraph): similar size, normal line spacing, `l` ran full width, and
/// `b` starts flush at the column (an indented or centered start is a new
/// paragraph — continuation lines are always flush).
fn should_merge(l: &RawLine, b: &RawLine, cols: &PageCols) -> bool {
    let size_max = l.size.max(b.size);
    if (l.size - b.size).abs() > 0.18 * size_max {
        return false;
    }
    // A leader-sized gap inside either line means table-of-contents / index
    // entries (title ..... page#): each is self-contained, never a wrap.
    if l.max_gap > 6.0 * size_max || b.max_gap > 6.0 * size_max {
        return false;
    }
    // A bare page number is never part of a paragraph (TOC number columns
    // split from their titles, folios at page edges).
    if is_page_number(&l.text) || is_page_number(&b.text) {
        return false;
    }
    let lead = l.y - b.y; // l above b (reading frame, y up)
    if lead < 0.35 * size_max {
        return false; // same baseline, out of order, or overlapping
    }
    if lead > 2.0 * cols.leading {
        return false; // a paragraph gap
    }
    if l.x1 < cols.x1 - l.size * 1.7 {
        return false; // the line ended short → a paragraph end, not a wrap
    }
    if b.x0 > cols.x0 + 0.6 * b.size {
        return false; // indented / centered → first line of a new paragraph
    }
    true
}

/// Whether a line is nothing but a page number (arabic or roman) — TOC
/// number columns and folios must never join a paragraph.
fn is_page_number(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > 5 {
        return false;
    }
    t.chars().all(|c| c.is_ascii_digit())
        || t.chars()
            .all(|c| matches!(c, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm'))
}

/// Join two visual lines of one paragraph: dehyphenate when the first ends
/// with a hyphen before a lowercase continuation.
fn join_lines(a: &str, b: &str) -> String {
    if let Some(head) = a.strip_suffix('-') {
        if b.chars().next().is_some_and(|c| c.is_lowercase()) {
            return format!("{head}{b}");
        }
    }
    format!("{a} {b}")
}

/// A reconstructed paragraph: its original lines with geometry, the joined
/// segment text, and the box it occupies on the page.
struct Para {
    lines: Vec<RawLine>,
    text: String,
    x0: f64,
    x1: f64,
    top: f64,
    bot: f64,
    size: f64,
    translatable: bool,
    /// Continuation of an earlier page's paragraph. It shares that segment
    /// and receives a proportion of its translation when writing.
    merged_up: bool,
    merged_into: Option<(usize, usize)>,
}

impl Para {
    fn new(lines: Vec<RawLine>) -> Self {
        let x0 = lines.iter().map(|l| l.x0).fold(f64::MAX, f64::min);
        let x1 = lines.iter().map(|l| l.x1).fold(f64::MIN, f64::max);
        let top = lines.first().map(|l| l.top()).unwrap_or(0.0);
        let bot = lines.last().map(|l| l.bot()).unwrap_or(0.0);
        let mut sizes: Vec<f64> = lines.iter().map(|l| l.size).collect();
        sizes.sort_by(|a, b| a.total_cmp(b));
        let size = sizes[sizes.len() / 2];
        let text = lines
            .iter()
            .map(|l| l.text.as_str())
            .fold(String::new(), |acc, t| {
                if acc.is_empty() {
                    t.to_string()
                } else {
                    join_lines(&acc, t)
                }
            });
        let translatable = is_translatable(&text);
        Para {
            lines,
            text,
            x0,
            x1,
            top,
            bot,
            size,
            translatable,
            merged_up: false,
            merged_into: None,
        }
    }

    fn first(&self) -> &RawLine {
        &self.lines[0]
    }
    fn last(&self) -> &RawLine {
        &self.lines[self.lines.len() - 1]
    }
}

/// Group one page's sorted lines into paragraphs.
fn build_paragraphs(lines: Vec<RawLine>) -> Vec<Para> {
    let cols = page_cols(&lines);
    group_lines(lines, &cols)
}

/// [`build_paragraphs`] with the column geometry supplied, so unit tests can
/// feed a realistic body column without building a full page of full lines.
fn group_lines(lines: Vec<RawLine>, cols: &PageCols) -> Vec<Para> {
    let mut paras: Vec<Para> = Vec::new();
    for line in lines {
        let merge = paras
            .last()
            .is_some_and(|p| should_merge(p.last(), &line, cols));
        if merge {
            let p = paras.last_mut().unwrap();
            p.lines.push(line);
        } else {
            paras.push(Para::new(vec![line]));
        }
    }
    // rebuild text/geometry once, after all lines are placed
    paras.into_iter().map(|p| Para::new(p.lines)).collect()
}

/// Whether `cur` (the first line of a page) continues `prev` (the last line
/// of the previous page): same size, the previous page's line ran full width,
/// and the new page's line starts flush. [`should_merge`]'s leading test is
/// meaningless here — baselines on different pages are unrelated — so this is
/// a leaner check (continuation lines are always flush; a new paragraph on
/// the next page is indented in indent-styled books, and in block-styled
/// ones this can false-positive, merging two short paragraphs at a page
/// turn — acceptable for a paragraph-oriented translator).
fn should_merge_across(
    l: &RawLine,
    b: &RawLine,
    prev_cols: &PageCols,
    cur_cols: &PageCols,
) -> bool {
    let size_max = l.size.max(b.size);
    if (l.size - b.size).abs() > 0.18 * size_max {
        return false;
    }
    if l.max_gap > 6.0 * size_max || b.max_gap > 6.0 * size_max {
        return false; // TOC/index entry at a page turn
    }
    if is_page_number(&l.text) || is_page_number(&b.text) {
        return false;
    }
    if l.x1 < prev_cols.x1 - l.size * 1.7 {
        return false; // previous page's last line ended short
    }
    if b.x0 > cur_cols.x0 + 0.6 * b.size {
        return false; // indented / centered start
    }
    true
}

/// Merge page-boundary continuations: when page `i`'s first paragraph is the
/// wrapped continuation of page `i-1`'s last paragraph, append its text to
/// the earlier segment and mark the continuation `merged_up`.
fn merge_across_pages(pages: &mut [(MediaBoxGeo, Vec<Para>)]) {
    let cols: Vec<Option<PageCols>> = pages.iter().map(|(_, p)| page_cols_if(p)).collect();
    for i in 1..pages.len() {
        let (Some(prev_cols), Some(cur_cols)) = (cols[i - 1], cols[i]) else {
            continue;
        };
        let merge = match (pages[i - 1].1.last(), pages[i].1.first()) {
            (Some(prev), Some(cur)) => {
                should_merge_across(prev.last(), cur.first(), &prev_cols, &cur_cols)
            }
            _ => false,
        };
        if merge {
            let text = pages[i].1[0].text.clone();
            let previous = (i - 1, pages[i - 1].1.len() - 1);
            let owner = pages[previous.0].1[previous.1]
                .merged_into
                .unwrap_or(previous);
            let first = &mut pages[owner.0].1[owner.1];
            first.text = join_lines(&first.text, &text);
            first.translatable = is_translatable(&first.text);
            pages[i].1[0].merged_up = true;
            pages[i].1[0].merged_into = Some(owner);
        }
    }
}

/// [`page_cols`] over an already-built paragraph list (uses each line).
fn page_cols_if(paras: &[Para]) -> Option<PageCols> {
    let lines: Vec<&RawLine> = paras.iter().flat_map(|p| p.lines.iter()).collect();
    if lines.is_empty() {
        return None;
    }
    let owned: Vec<RawLine> = lines.into_iter().cloned().collect();
    Some(page_cols(&owned))
}

// ── line wrapping (for fitting translations into slots) ─────────────────────

#[path = "pdf_text.rs"]
mod text;

#[cfg(test)]
fn wrap_text(text: &str, max_w: f32, char_width: impl Fn(char) -> f32) -> Vec<String> {
    self::text::wrap(text, max_w as f64, |s| {
        Ok(s.chars().map(&char_width).sum::<f32>() as f64)
    })
    .unwrap()
}

// ── document IR ─────────────────────────────────────────────────────────────

struct PageData {
    media: MediaBoxGeo,
    cols: PageCols,
    paras: Vec<Para>,
}

pub struct PdfDoc {
    font: PdfFont,
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
                    eprintln!("pdf page: media={media:?} cols={cols:?}");
                    for l in &lines {
                        eprintln!(
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
        let font = PdfFont::shared()?;
        eprintln!(
            "pdf: {} page(s), {} paragraph(s), {} translatable",
            page_data.len(),
            page_data.iter().map(|p| p.paras.len()).sum::<usize>(),
            seg_index.len()
        );
        Ok(PdfDoc {
            font: font.clone(),
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
        let fonts = text::Fonts::new(self.font.clone())?;
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

// ── content stream building ─────────────────────────────────────────────────

/// Number formatting for content streams: fixed 2 decimals, never scientific.
fn pdfnum(x: f64) -> String {
    format!("{x:.2}")
}

struct EmbeddedFont {
    key: String,
    id: ObjectId,
    remap: BTreeMap<u16, u16>,
}
struct ContentBuilder {
    out: Vec<u8>,
    has_body: bool,
}

impl ContentBuilder {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            has_body: false,
        }
    }
    fn finish(self) -> Vec<u8> {
        self.out
    }

    /// Explicit glyph positions preserve shaping advances, mark offsets and RTL
    /// order. ActualText retains the logical Unicode string for copying/search.
    #[allow(clippy::too_many_arguments)]
    fn shaped(
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
    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64) {
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

fn draw_translation(
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
fn embed_font(
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
fn walk_page_tree(
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
fn inherited<'a>(
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
fn make_translation_page(
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
fn splice_translation_pages(
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
fn add_font_to_page(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_page_continuations_keep_all_text_and_replace_every_page() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("input.pdf");
        fs::write(
            &source,
            make_pdf(&[
                (600., 800., &[(50., 60., 12., "First page continuation")]),
                (600., 800., &[(50., 60., 12., "Second page continuation")]),
                (600., 800., &[(50., 60., 12., "Third page continuation")]),
            ]),
        )
        .unwrap();
        let mut pdf = PdfDoc::open(&source).unwrap();
        let segments = pdf.segments();
        assert_eq!(segments.len(), 1);
        for word in ["First", "Second", "Third"] {
            assert!(segments[0].text.contains(word));
        }
        let output = root.path().join("translated.pdf");
        pdf.write(
            &[(0, "第一页译文。第二页译文。第三页译文。".into())],
            &output,
            OutputMode::Replace,
        )
        .unwrap();
        let doc = lopdf::Document::load(&output).unwrap();
        for id in doc.get_pages().values() {
            let content = String::from_utf8_lossy(&doc.get_page_content(*id).unwrap()).into_owned();
            assert!(
                content.contains(" re f"),
                "every source fragment is covered"
            );
            assert!(
                content.contains("/ActualText"),
                "every page gets a translation fragment"
            );
        }
    }

    #[test]
    fn replace_resolves_indirect_inherited_resources() {
        let root = tempfile::tempdir().unwrap();
        let mut doc = lopdf::Document::load_mem(&make_pdf(&[(
            600.,
            800.,
            &[(50., 60., 12., "Hello world")],
        )]))
        .unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        let resources = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .clone();
        let resources = match resources {
            Object::Reference(id) => id,
            other => doc.add_object(other),
        };
        let parent = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        doc.get_object_mut(parent)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Resources", Object::Reference(resources));
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .remove(b"Resources");
        let input = root.path().join("input.pdf");
        doc.save(&input).unwrap();
        let mut pdf = PdfDoc::open(&input).unwrap();
        pdf.write(
            &[(0, "你好世界".into())],
            &root.path().join("output.pdf"),
            OutputMode::Replace,
        )
        .unwrap();
    }

    #[test]
    fn normalize_expands_ligatures_and_spaces() {
        assert_eq!(normalize_line("ﬁghting ﬂow"), "fighting flow");
        assert_eq!(normalize_line("a\u{00A0}b"), "a b");
        assert_eq!(normalize_line("x\u{00AD}y\u{FEFF}z"), "xyz");
        assert_eq!(normalize_line("a\x07b"), "ab");
    }

    #[test]
    fn normalize_collapses_space_runs_and_trims() {
        assert_eq!(normalize_line("  hello   world  "), "hello world");
        assert_eq!(normalize_line("   "), "");
    }

    #[test]
    fn normalize_collapses_toc_dot_leaders() {
        assert_eq!(
            normalize_line("1.1. Openings . . . . . . . . 7"),
            "1.1. Openings 7"
        );
        // A single '.' or '..' is sentence punctuation, not a leader.
        assert_eq!(normalize_line("It ends here. Next"), "It ends here. Next");
    }

    #[test]
    fn translatable_requires_a_letter() {
        assert!(is_translatable("hello"));
        assert!(is_translatable("围棋入门"));
        assert!(is_translatable("9x9 board"));
        assert!(!is_translatable("9"));
        assert!(!is_translatable("1.1.2."));
        assert!(!is_translatable("———"));
        assert!(!is_translatable(""));
    }

    /// Uniform 10-unit chars: wrapping is then purely count-based.
    fn uniform(_c: char) -> f32 {
        10.0
    }

    #[test]
    fn wrap_breaks_at_spaces_and_trims() {
        let lines = wrap_text("aaa bbb ccc ddd", 70.0, uniform);
        assert_eq!(lines, vec!["aaa bbb", "ccc ddd"]);
    }

    #[test]
    fn wrap_breaks_anywhere_after_cjk() {
        let lines = wrap_text("一二三四五六七", 50.0, uniform);
        assert_eq!(lines, vec!["一二三四五", "六七"]);
    }

    #[test]
    fn wrap_hard_breaks_unbreakable_tokens() {
        let lines = wrap_text("aaaaaaaaaabbbbbbbbbb", 100.0, uniform);
        assert_eq!(lines, vec!["aaaaaaaaaa", "bbbbbbbbbb"]);
    }

    #[test]
    fn wrap_breaks_after_hyphen() {
        let lines = wrap_text("abc-def-ghi", 50.0, uniform);
        assert_eq!(lines, vec!["abc-", "def-", "ghi"]);
    }

    #[test]
    fn wrap_empty_and_overwide_chars_go_out_alone() {
        assert!(wrap_text("", 50.0, uniform).is_empty());
        let lines = wrap_text("ab", 50.0, |_c| 60.0);
        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn wrap_drops_double_spaces_around_breaks() {
        let lines = wrap_text("aaa  bbb", 40.0, uniform);
        assert_eq!(lines, vec!["aaa", "bbb"]);
    }

    // ── paragraph reconstruction ─────────────────────────────────────────────

    fn line(x0: f64, y: f64, size: f64, text: &str, full_to: f64) -> RawLine {
        RawLine {
            text: text.to_string(),
            x0,
            x1: full_to,
            y,
            size,
            angle: 0.0,
            max_gap: 0.4 * size, // ordinary word-space gaps
        }
    }

    /// A TOC-entry line: ends flush (page number) but carries a dot-leader
    /// gap inside.
    fn toc_line(x0: f64, y: f64, size: f64, text: &str, full_to: f64) -> RawLine {
        RawLine {
            max_gap: 20.0 * size,
            ..line(x0, y, size, text, full_to)
        }
    }

    /// A body page's column geometry: margins at 56/539, 15.5pt leading.
    fn body_cols() -> PageCols {
        PageCols {
            x0: 56.0,
            x1: 539.0,
            leading: 15.5,
        }
    }

    #[test]
    fn paras_merge_wrapped_lines() {
        // A full first line at y=700 followed by a flush line one leading
        // below: one paragraph.
        let lines = vec![
            line(
                56.0,
                700.0,
                10.5,
                "The lion attacks the weak stones early",
                539.0,
            ),
            line(
                56.0,
                684.5,
                10.5,
                "in the game, before contact happens.",
                300.0,
            ),
        ];
        let paras = group_lines(lines, &body_cols());
        assert_eq!(paras.len(), 1);
        assert_eq!(
            paras[0].text,
            "The lion attacks the weak stones early in the game, before contact happens."
        );
    }

    #[test]
    fn paras_split_on_short_line_indent_or_gap() {
        // Previous line NOT full → new paragraph.
        let short = vec![
            line(56.0, 700.0, 10.5, "Short line", 200.0),
            line(56.0, 684.5, 10.5, "Follows", 90.0),
        ];
        assert_eq!(group_lines(short, &body_cols()).len(), 2);

        // Indented start → new paragraph (first-line indent style).
        let indented = vec![
            line(
                56.0,
                700.0,
                10.5,
                "Full line reaching the right edge",
                539.0,
            ),
            line(66.5, 684.5, 10.5, "Indented", 100.0),
        ];
        assert_eq!(group_lines(indented, &body_cols()).len(), 2);

        // Vertical gap → new paragraph (block style).
        let gap = vec![
            line(
                56.0,
                700.0,
                10.5,
                "Full line reaching the right edge",
                539.0,
            ),
            line(56.0, 640.0, 10.5, "After a gap", 120.0),
        ];
        assert_eq!(group_lines(gap, &body_cols()).len(), 2);
    }

    #[test]
    fn paras_join_dehyphenates() {
        let lines = vec![
            line(56.0, 700.0, 10.5, "The lit-", 539.0),
            line(56.0, 684.5, 10.5, "tle lion", 100.0),
        ];
        let paras = group_lines(lines, &body_cols());
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text, "The little lion");
    }

    #[test]
    fn paras_split_heading_by_size_and_short_line() {
        let lines = vec![
            line(56.0, 700.0, 18.0, "Chapter 3: The Lion", 250.0),
            line(56.0, 670.0, 10.5, "Body text follows", 200.0),
        ];
        assert_eq!(group_lines(lines, &body_cols()).len(), 2);
    }

    #[test]
    fn page_numbers_stay_passthrough() {
        let lines = vec![
            line(56.0, 700.0, 10.5, "A full body line here", 539.0),
            line(295.0, 40.0, 10.5, "17", 305.0),
        ];
        let paras = group_lines(lines, &body_cols());
        assert_eq!(paras.len(), 2);
        assert!(paras[1].lines[0].x0 > 200.0); // centered page number
        assert!(!paras[1].translatable);
    }

    #[test]
    fn dot_leaders_detected_in_raw_text() {
        assert!(has_dot_leader("1.1. Openings . . . . . . 7"));
        assert!(has_dot_leader("Contents ......... 3"));
        assert!(!has_dot_leader("It ends here. Next"));
        assert!(!has_dot_leader("wait for it..."));
        assert!(!has_dot_leader("e. g. i. e."));
    }

    #[test]
    fn toc_entries_never_merge() {
        // Two TOC lines, both flush-ended (right-aligned page numbers): each
        // stays its own segment because of the leader gap inside.
        let lines = vec![
            toc_line(56.0, 700.0, 10.5, "1.1. Openings 7", 539.0),
            toc_line(56.0, 684.5, 10.5, "1.2. Influence 14", 539.0),
        ];
        assert_eq!(group_lines(lines, &body_cols()).len(), 2);
    }

    #[test]
    fn page_cols_median_start_and_max_edge() {
        let lines = vec![
            line(66.0, 700.0, 10.5, "Indented first line", 500.0),
            line(56.0, 684.5, 10.5, "flush body line", 539.0),
            line(56.0, 669.0, 10.5, "another", 90.0),
        ];
        let cols = page_cols(&lines);
        assert_eq!(cols.x0, 56.0); // median start ignores the indent
        assert_eq!(cols.x1, 539.0); // longest line marks the column edge
    }

    // ── sfnt plumbing ────────────────────────────────────────────────────────

    #[test]
    fn face_extraction_keeps_tables() {
        let font = PdfFont::shared().expect(
            "PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT",
        );
        let f: &[u8] = &font.standalone;
        assert!(&f[..4] == b"OTTO" || &f[..4] == b"\x00\x01\x00\x00" || &f[..4] == b"true");
        let tables = sfnt_tables(f, 0).unwrap();
        assert!(tables.iter().any(|(t, _)| t == b"cmap"));
        assert!(tables.iter().any(|(t, _)| t == b"head"));
        // It must parse as a single face at index 0.
        let face = ttf_parser::Face::parse(f, 0).unwrap();
        assert!(face.glyph_index('中').is_some());
        assert!(face.glyph_index('A').is_some());
    }

    // ── end-to-end with krilla-built fixtures ────────────────────────────────

    /// One fixture line: `(x, y_from_top, size, text)`.
    type FixtureLine = (f64, f64, f64, &'static str);
    /// One fixture page: `(width, height, lines)`.
    type FixturePage = (f64, f64, &'static [FixtureLine]);

    /// Draw `(x, y_from_top, size, text)` lines onto one page (krilla's y
    /// grows downward; the extractor reports user-space y, y-up from the
    /// page's bottom edge).
    fn make_pdf(pages: &[FixturePage]) -> Vec<u8> {
        let font = PdfFont::shared().unwrap().krilla().unwrap();
        let mut doc = krilla::Document::new();
        for &(w, h, lines) in pages {
            let mut page = doc
                .start_page_with(krilla::page::PageSettings::from_wh(w as f32, h as f32).unwrap());
            let mut surface = page.surface();
            for &(x, y_top, size, text) in lines {
                surface.draw_text(
                    krilla::geom::Point::from_xy(x as f32, y_top as f32),
                    font.clone(),
                    size as f32,
                    text,
                    false,
                    krilla::text::TextDirection::Auto,
                );
            }
            surface.finish();
            page.finish();
        }
        doc.finish().unwrap()
    }

    /// Manual probe against a real book:
    /// `FERRYMAN_PDF_PROBE=book.pdf cargo test probe_real_pdf -- --ignored --nocapture`
    #[test]
    #[ignore = "manual probe against FERRYMAN_PDF_PROBE"]
    fn probe_real_pdf() {
        let Some(path) = std::env::var_os("FERRYMAN_PDF_PROBE") else {
            return;
        };
        let pdf = PdfDoc::open(Path::new(&path)).unwrap();
        let segs = pdf.segments();
        eprintln!("== {} segments", segs.len());
        let mut lens: Vec<usize> = segs.iter().map(|s| s.text.len()).collect();
        lens.sort_unstable();
        eprintln!(
            "== len min/median/max: {}/{}/{}",
            lens.first().copied().unwrap_or(0),
            lens.get(lens.len() / 2).copied().unwrap_or(0),
            lens.last().copied().unwrap_or(0)
        );
        for s in &segs {
            let head: String = s.text.chars().take(78).collect();
            eprintln!("-- seg {} ({} ch): {}", s.id, s.text.chars().count(), head);
        }
        // Show paragraph geometry of a body page to eyeball the merge.
        for (pi, page) in pdf.pages.iter().enumerate().skip(8).take(1) {
            eprintln!(
                "== page {} media {:?} cols {:?}",
                pi + 1,
                page.media,
                page.cols
            );
            for (xi, p) in page.paras.iter().enumerate() {
                eprintln!(
                    "   para {xi}: lines={} size={:.1} x0={:.1} x1={:.1} top={:.1} bot={:.1} tr={} merged={} {:?}",
                    p.lines.len(), p.size, p.x0, p.x1, p.top, p.bot, p.translatable, p.merged_up,
                    &p.text[..p.text.len().min(60)]
                );
            }
        }
    }

    #[test]
    fn roundtrip_layout_preserving_bilingual() {
        PdfFont::shared().expect(
            "PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT",
        );
        // Page 1: a paragraph wrapped over two lines (long first line + short
        // flush tail) + a separate paragraph. Page 2: one paragraph.
        let src = make_pdf(&[
            (
                400.0,
                500.0,
                &[
                    (
                        40.0,
                        60.0,
                        11.0,
                        "The lion attacks the weak group early in the opening,",
                    ),
                    (40.0, 77.0, 11.0, "before the tiger reacts."),
                    (40.0, 110.0, 11.0, "Black to play."),
                ],
            ),
            (400.0, 500.0, &[(40.0, 60.0, 11.0, "A second page.")]),
        ]);
        let tmp = std::env::temp_dir().join(format!("ferryman_pdf_lt_{}.pdf", std::process::id()));
        fs::write(&tmp, src).unwrap();

        let mut pdf = PdfDoc::open(&tmp).unwrap();
        let segs = pdf.segments();
        assert_eq!(
            segs.len(),
            3,
            "two paragraphs on page 1 + one on page 2: {:?}",
            segs.iter().map(|s| &s.text).collect::<Vec<_>>()
        );
        assert!(
            segs[0]
                .text
                .starts_with("The lion attacks the weak group early"),
            "wrapped lines merged into one segment: {:?}",
            segs[0].text
        );
        assert_eq!(segs[1].text, "Black to play.");
        assert_eq!(segs[2].text, "A second page.");

        let out = tmp.with_extension("out.pdf");
        pdf.write(
            &[
                (0, "狮子在布局阶段早早攻击薄弱的棋群。".to_string()),
                (1, "轮黑走棋。".to_string()),
                (2, "第二页。".to_string()),
            ],
            &out,
            OutputMode::Bilingual,
        )
        .unwrap();

        let bytes = fs::read(&out).unwrap();
        let reread = lopdf::Document::load_mem(&bytes).unwrap();
        assert_eq!(
            reread.get_pages().len(),
            4,
            "every original page gains a translation page"
        );
        let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
        assert!(text.contains("The lion attacks"), "original kept: {text}");
        assert!(
            text.contains("轮黑走棋"),
            "translation present and extractable: {text}"
        );

        // Re-opening our own output must work too (its /W is single-gid
        // ranges, which normalize_cid_widths repairs before extraction).
        let reopened = PdfDoc::open(&out).unwrap();
        let re_segs = reopened.segments();
        assert!(
            re_segs.iter().any(|s| s.text.contains("狮子在布局")),
            "our own CJK output re-extracts with geometry: {:?}",
            re_segs.iter().map(|s| &s.text).collect::<Vec<_>>()
        );

        // The embedded font must be a subset: with a full CJK face on disk
        // (≥2 MB), embedding it whole would dwarf everything else in the
        // file — a fixture's few dozen glyphs are a tiny fraction of it.
        if pdf.font.standalone.len() > 2_000_000 {
            let mut largest_font = 0usize;
            for obj in reread.objects.values() {
                let Object::Stream(s) = obj else { continue };
                let is_font = s.dict.has(b"Length1")
                    || s.dict
                        .get(b"Subtype")
                        .and_then(Object::as_name)
                        .is_ok_and(|t| t == b"OpenType");
                if is_font {
                    largest_font = largest_font.max(s.content.len());
                }
            }
            assert!(
                largest_font * 4 < pdf.font.standalone.len(),
                "embedded font {largest_font} should be a small subset of {}",
                pdf.font.standalone.len()
            );
        }

        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn unicode_output_embeds_fallback_fonts_and_logical_text() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.pdf");
        fs::write(
            &source,
            make_pdf(&[(
                500.0,
                300.0,
                &[(
                    30.0,
                    40.0,
                    14.0,
                    "Multilingual translation fixture for font fallback and shaping.",
                )],
            )]),
        )
        .unwrap();
        let mut pdf = PdfDoc::open(&source).unwrap();
        let translation = "你好，世界。 العربية سلام ffi a\u{301}";
        let output = root.path().join("output.pdf");
        pdf.write(&[(0, translation.into())], &output, OutputMode::Bilingual)
            .unwrap();
        let doc = lopdf::Document::load(&output).unwrap();
        let page = *doc.get_pages().get(&2).unwrap();
        let content = String::from_utf8(doc.get_page_content(page).unwrap()).unwrap();
        assert!(content.contains("/ActualText <FEFF"));
        let arabic: String = "سلام".encode_utf16().map(|c| format!("{c:04X}")).collect();
        assert!(
            content.contains(&arabic),
            "logical Arabic must remain available to PDF readers"
        );
        let resources = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .as_dict()
            .unwrap();
        let fonts = resources.get(b"Font").unwrap().as_dict().unwrap();
        let drawn = fonts
            .iter()
            .filter(|(name, _)| content.contains(&format!("/{} ", String::from_utf8_lossy(name))))
            .count();
        assert!(drawn >= 2, "CJK and Arabic use separate font coverage");
        if let Some(path) = std::env::var_os("FERRYMAN_PDF_TEST_OUTPUT") {
            fs::copy(output, path).unwrap();
        }
    }

    /// Restructure a flat page tree into root → two intermediate /Pages nodes
    /// → leaves (the shape real books use), re-parenting the leaves. Exercises
    /// the splicer's recursive branch: krilla itself only writes flat trees.
    fn nest_page_tree(src: &[u8]) -> Vec<u8> {
        let mut doc = lopdf::Document::load_mem(src).unwrap();
        let root = doc
            .catalog()
            .unwrap()
            .get(b"Pages")
            .and_then(Object::as_reference)
            .unwrap();
        let kids: Vec<Object> = doc
            .get_dictionary(root)
            .unwrap()
            .get(b"Kids")
            .and_then(Object::as_array)
            .unwrap()
            .clone();
        let make_mid = |doc: &mut lopdf::Document, kids: Vec<Object>| {
            let mut d = Dictionary::new();
            d.set("Type", "Pages");
            d.set("Kids", Object::Array(kids.clone()));
            d.set("Count", kids.len() as i64);
            doc.add_object(Object::Dictionary(d))
        };
        let (a, b) = kids.split_at(kids.len().div_ceil(2));
        let (ida, idb) = (
            make_mid(&mut doc, a.to_vec()),
            make_mid(&mut doc, b.to_vec()),
        );
        for (group, mid) in [(a, ida), (b, idb)] {
            for kid in group {
                let leaf = kid.as_reference().unwrap();
                doc.get_object_mut(leaf)
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .set("Parent", Object::Reference(mid));
            }
        }
        let rd = doc.get_object_mut(root).unwrap().as_dict_mut().unwrap();
        rd.set("Count", kids.len() as i64);
        rd.set(
            "Kids",
            Object::Array(vec![Object::Reference(ida), Object::Reference(idb)]),
        );
        let path =
            std::env::temp_dir().join(format!("ferryman_pdf_nestsrc_{}.pdf", std::process::id()));
        doc.save(&path).unwrap();
        let out = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        out
    }

    /// A nested page tree must survive bilingual splicing: intermediate
    /// /Pages nodes stay in the root's Kids and every page stays reachable
    /// (a regression where the splicer dropped intermediate children made
    /// strict parsers see zero pages).
    #[test]
    fn nested_page_tree_spliced_intact() {
        PdfFont::shared().expect(
            "PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT",
        );
        let src = nest_page_tree(&make_pdf(&[
            (
                400.0,
                500.0,
                &[
                    (40.0, 60.0, 11.0, "First page body line one here long"),
                    (40.0, 77.0, 11.0, "and the wrapped tail."),
                ],
            ),
            (
                400.0,
                500.0,
                &[
                    (40.0, 60.0, 11.0, "Second page with a longer body line"),
                    (40.0, 77.0, 11.0, "short tail."),
                ],
            ),
            (400.0, 500.0, &[(40.0, 60.0, 11.0, "Third page text.")]),
        ]));
        let tmp =
            std::env::temp_dir().join(format!("ferryman_pdf_nest_{}.pdf", std::process::id()));
        fs::write(&tmp, src).unwrap();

        let mut pdf = PdfDoc::open(&tmp).unwrap();
        assert_eq!(pdf.segments().len(), 3);
        let out = tmp.with_extension("out.pdf");
        pdf.write(
            &[
                (0, "第一页的正文。".to_string()),
                (1, "第二页的正文。".to_string()),
                (2, "第三页的正文。".to_string()),
            ],
            &out,
            OutputMode::Bilingual,
        )
        .unwrap();

        let reread = lopdf::Document::load_mem(&fs::read(&out).unwrap()).unwrap();
        assert_eq!(reread.get_pages().len(), 6, "3 originals + 3 translations");
        let root = reread
            .catalog()
            .unwrap()
            .get(b"Pages")
            .and_then(Object::as_reference)
            .unwrap();
        let root_kids = reread
            .get_dictionary(root)
            .unwrap()
            .get(b"Kids")
            .and_then(Object::as_array)
            .unwrap()
            .clone();
        assert_eq!(
            root_kids.len(),
            2,
            "both intermediate nodes survive splicing"
        );
        for kid in root_kids {
            let mid = kid.as_reference().unwrap();
            let mid_dict = reread.get_dictionary(mid).unwrap();
            assert_eq!(mid_dict.get(b"Type").unwrap().as_name().unwrap(), b"Pages");
            let n: usize = mid_dict.get(b"Kids").unwrap().as_array().unwrap().len();
            let count = mid_dict.get(b"Count").unwrap().as_i64().unwrap();
            assert_eq!(n as i64, count, "Count matches Kids after insertion");
        }

        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(&out);
    }

    /// Manual splice probe against a real book (dummy translations):
    /// `FERRYMAN_PDF_PROBE=book.pdf cargo test write_book_probe -- --ignored --nocapture`
    #[test]
    #[ignore = "manual probe against FERRYMAN_PDF_PROBE"]
    fn write_book_probe() {
        let Some(path) = std::env::var_os("FERRYMAN_PDF_PROBE") else {
            return;
        };
        let mut pdf = PdfDoc::open(Path::new(&path)).unwrap();
        let trs: Vec<(SegmentId, String)> = (0..pdf.segments().len())
            .map(|i| (i, "测试译文。".to_string()))
            .collect();
        pdf.write(
            &trs,
            Path::new("/tmp/ferryman_book_probe.pdf"),
            OutputMode::Bilingual,
        )
        .unwrap();
        eprintln!("wrote /tmp/ferryman_book_probe.pdf");
    }

    /// Manual poppler-compat check:
    /// `cargo test write_fixture_output -- --ignored --nocapture`
    #[test]
    #[ignore = "manual: writes /tmp/ferryman_fixture_out.pdf for rendering"]
    fn write_fixture_output() {
        PdfFont::shared().expect(
            "PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT",
        );
        let src = make_pdf(&[
            (
                400.0,
                500.0,
                &[
                    (
                        40.0,
                        60.0,
                        11.0,
                        "The lion attacks the weak group early in the opening,",
                    ),
                    (40.0, 77.0, 11.0, "before the tiger reacts."),
                    (40.0, 110.0, 11.0, "Black to play."),
                ],
            ),
            (400.0, 500.0, &[(40.0, 60.0, 11.0, "A second page.")]),
        ]);
        let tmp = std::env::temp_dir().join("ferryman_fixture_src.pdf");
        fs::write(&tmp, src).unwrap();
        let mut pdf = PdfDoc::open(&tmp).unwrap();
        pdf.write(
            &[
                (0, "狮子在布局阶段早早攻击薄弱的棋群。".to_string()),
                (1, "轮黑走棋。".to_string()),
                (2, "第二页。".to_string()),
            ],
            Path::new("/tmp/ferryman_fixture_out.pdf"),
            OutputMode::Bilingual,
        )
        .unwrap();
        eprintln!("wrote /tmp/ferryman_fixture_out.pdf");
    }

    #[test]
    fn replace_mode_overlays_in_place() {
        PdfFont::shared().expect(
            "PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT",
        );
        let src = make_pdf(&[(
            400.0,
            500.0,
            &[
                (
                    40.0,
                    60.0,
                    11.0,
                    "Original english sentence running to the right margin.",
                ),
                (40.0, 77.0, 11.0, "Tail of the paragraph."),
                (40.0, 110.0, 11.0, "Another line."),
            ],
        )]);
        let tmp =
            std::env::temp_dir().join(format!("ferryman_pdf_repl_{}.pdf", std::process::id()));
        fs::write(&tmp, src).unwrap();

        let mut pdf = PdfDoc::open(&tmp).unwrap();
        let segs = pdf.segments();
        assert_eq!(segs.len(), 2);
        let out = tmp.with_extension("replaced.pdf");
        pdf.write(
            &[(0, "英语原句。".to_string()), (1, "另一行。".to_string())],
            &out,
            OutputMode::Replace,
        )
        .unwrap();

        let bytes = fs::read(&out).unwrap();
        let reread = lopdf::Document::load_mem(&bytes).unwrap();
        assert_eq!(reread.get_pages().len(), 1, "replace adds no pages");
        let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
        assert!(text.contains("英语原句"), "translation drawn: {text}");

        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(&out);
    }
}
