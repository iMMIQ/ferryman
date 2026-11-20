use super::{ASCENT, DESCENT};
use pdf_extract::OutputDev;

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
pub(super) fn normalize_line(raw: &str) -> String {
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
pub(super) fn is_translatable(line: &str) -> bool {
    line.chars().any(char::is_alphabetic)
}

// ── geometric extraction (custom OutputDev) ─────────────────────────────────

/// Rotate `(x, y)` by `angle` radians (about the origin).
pub(super) fn rot(angle: f64, x: f64, y: f64) -> (f64, f64) {
    let (s, c) = angle.sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// One positioned run of characters — a "visual line" piece, already rotated
/// into a left-to-right reading frame (`y` still points up). All geometry a
/// paragraph or a redraw needs lives here.
#[derive(Debug, Clone)]
pub(super) struct RawLine {
    pub(super) text: String,
    /// Left edge of the first character.
    pub(super) x0: f64,
    /// Right edge of the last character (origin + advance).
    pub(super) x1: f64,
    /// Baseline `y`.
    pub(super) y: f64,
    /// Font size (CTM-scaled).
    pub(super) size: f64,
    /// Text direction in PDF user space, radians (0 = upright, left→right).
    pub(super) angle: f64,
    /// Largest gap between consecutive characters (a dot-leader or column
    /// split inside the line; wrapped body text never exceeds a word space).
    pub(super) max_gap: f64,
}

impl RawLine {
    /// Estimated top of the line's ink.
    pub(super) fn top(&self) -> f64 {
        self.y + self.size * ASCENT
    }
    /// Estimated bottom of the line's ink.
    pub(super) fn bot(&self) -> f64 {
        self.y - self.size * DESCENT
    }
}

/// An [`OutputDev`] that keeps character geometry instead of flattening to
/// text: characters accumulate into [`RawLine`]s while the baseline, angle
/// and gaps are consistent; pages collect sorted (top→bottom, left→right).
#[derive(Default)]
pub(super) struct GeoCollector {
    pub(super) pages: Vec<(MediaBoxGeo, Vec<RawLine>)>,
    pub(super) media: Option<MediaBoxGeo>,
    pub(super) lines: Vec<RawLine>,
    pub(super) buf: String,
    pub(super) buf_x0: f64,
    pub(super) buf_y: f64,
    pub(super) buf_size: f64,
    pub(super) buf_angle: f64,
    pub(super) buf_max_gap: f64,
    /// Width of the whitespace run currently in progress (spaces and
    /// positioning jumps), committed to `buf_max_gap when ink resumes.
    pub(super) run_ws: f64,
    pub(super) last_x1: f64,
}

pub(super) type MediaBoxGeo = (f64, f64, f64, f64); // llx lly urx ury

/// Whether a raw line contains a dot-leader run — four or more dots with
/// only spaces between them (`. . . .`, `....`). The dots are ink, so the
/// whitespace-run tracker can't see the leader as one gap; the text is the
/// only witness. Three or fewer dots stay ordinary punctuation.
pub(super) fn has_dot_leader(raw: &str) -> bool {
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
