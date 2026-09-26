//! Unicode line breaking and shaping; PDF geometry and writing stay in pdf.rs.
use super::PdfFont;
use anyhow::{anyhow, bail, Result};
use std::{collections::BTreeMap, ops::Range, path::PathBuf};
use unicode_bidi::{BidiInfo, Level};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

pub(super) struct Fonts {
    pub fonts: Vec<PdfFont>,
}
#[derive(Debug)]
pub(super) struct Glyph {
    pub font: usize,
    pub id: u16,
    pub advance: f64,
    pub x_offset: f64,
    pub y_offset: f64,
    pub text: String,
}
#[derive(Debug)]
pub(super) struct Line {
    pub text: String,
    pub glyphs: Vec<Glyph>,
    pub width: f64,
}
pub(super) struct Layout {
    pub size: f64,
    pub lines: Vec<Line>,
}
fn ignorable(ch: char) -> bool {
    use unicode_bidi::BidiClass::*;
    ch.is_control()
        || matches!(
            unicode_bidi::bidi_class(ch),
            BN | LRE | RLE | PDF | LRO | RLO | LRI | RLI | FSI | PDI
        )
        || matches!(ch, '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}
impl Fonts {
    pub fn new(primary: PdfFont) -> Result<Self> {
        let mut fonts = vec![primary];
        if let Some(paths) = std::env::var_os("FERRYMAN_PDF_FALLBACK_FONTS") {
            for path in std::env::split_paths(&paths) {
                fonts.push(PdfFont::load(&path)?);
            }
        }
        // Ordered, deterministic fallbacks. Explicit configuration takes precedence.
        for name in [
            "NotoSans-Regular.ttf",
            "NotoSansArabic-Regular.ttf",
            "NotoSansHebrew-Regular.ttf",
            "NotoSansDevanagari-Regular.ttf",
            "NotoSansThai-Regular.ttf",
            "DejaVuSans.ttf",
        ] {
            if let Some(path) = [
                "/app/fonts",
                "/usr/share/fonts/truetype/noto",
                "/usr/share/fonts/truetype/dejavu",
            ]
            .iter()
            .map(|root| PathBuf::from(root).join(name))
            .find(|path| path.is_file())
            {
                fonts.push(PdfFont::load(&path)?);
            }
        }
        Ok(Self { fonts })
    }

    pub fn shape(&self, text: &str) -> Result<Line> {
        self.shape_direction(text, None)
    }

    fn shape_direction(&self, text: &str, base: Option<Level>) -> Result<Line> {
        let faces: Vec<_> = self
            .fonts
            .iter()
            .map(|font| {
                rustybuzz::Face::from_slice(&font.standalone, 0)
                    .ok_or_else(|| anyhow!("invalid PDF shaping font"))
            })
            .collect::<Result<_>>()?;
        let bidi = BidiInfo::new(text, base);
        let mut glyphs = Vec::new();
        for paragraph in &bidi.paragraphs {
            let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
            for run in runs {
                let rtl = levels[run.start].is_rtl();
                let mut script = text[run.clone()]
                    .chars()
                    .map(|ch| ch.script())
                    .find(|s| !matches!(s, Script::Common | Script::Inherited | Script::Unknown))
                    .unwrap_or(Script::Latin);
                let mut pieces: Vec<(Range<usize>, usize, Script)> = Vec::new();
                for (start, grapheme) in text[run.clone()].grapheme_indices(true) {
                    let range = start + run.start..start + run.start + grapheme.len();
                    if let Some(strong) = grapheme.chars().map(|ch| ch.script()).find(|s| {
                        !matches!(s, Script::Common | Script::Inherited | Script::Unknown)
                    }) {
                        script = strong;
                    }
                    let font = faces.iter().position(|face| grapheme.chars().all(|ch| ignorable(ch) || face.glyph_index(ch).is_some()))
                        .ok_or_else(|| anyhow!("PDF font cannot render {grapheme:?}; install an appropriate Noto font or set FERRYMAN_PDF_FALLBACK_FONTS"))?;
                    if let Some((previous, previous_font, previous_script)) = pieces.last_mut() {
                        if *previous_font == font && *previous_script == script {
                            previous.end = range.end;
                            continue;
                        }
                    }
                    pieces.push((range, font, script));
                }
                if rtl {
                    pieces.reverse();
                }
                for (range, font, script) in pieces {
                    let part = &text[range];
                    let face = &faces[font];
                    let mut buffer = rustybuzz::UnicodeBuffer::new();
                    buffer.push_str(part);
                    buffer.set_direction(if rtl {
                        rustybuzz::Direction::RightToLeft
                    } else {
                        rustybuzz::Direction::LeftToRight
                    });
                    if let Ok(script) = script.short_name().parse() {
                        buffer.set_script(script);
                    }
                    buffer.guess_segment_properties();
                    let shaped = rustybuzz::shape(face, &[], buffer);
                    let mut clusters: Vec<usize> = shaped
                        .glyph_infos()
                        .iter()
                        .map(|info| info.cluster as usize)
                        .collect();
                    clusters.push(part.len());
                    clusters.sort_unstable();
                    clusters.dedup();
                    let upem = face.units_per_em() as f64;
                    for (info, position) in
                        shaped.glyph_infos().iter().zip(shaped.glyph_positions())
                    {
                        let start = info.cluster as usize;
                        let end = clusters
                            .iter()
                            .copied()
                            .find(|end| *end > start)
                            .unwrap_or(part.len());
                        let cluster = &part[start..end];
                        if info.glyph_id == 0 && !cluster.chars().all(ignorable) {
                            bail!("PDF font lacks a shaped glyph for {cluster:?}; set FERRYMAN_PDF_FALLBACK_FONTS");
                        }
                        glyphs.push(Glyph {
                            font,
                            id: info.glyph_id as u16,
                            advance: position.x_advance as f64 / upem,
                            x_offset: position.x_offset as f64 / upem,
                            y_offset: position.y_offset as f64 / upem,
                            text: cluster.to_owned(),
                        });
                    }
                }
            }
        }
        Ok(Line {
            text: text.to_owned(),
            width: glyphs.iter().map(|g| g.advance).sum(),
            glyphs,
        })
    }

    pub fn layout(&self, text: &str, width: f64, height: f64, initial_size: f64) -> Result<Layout> {
        let mut size = initial_size.max(super::TR_MIN_SIZE);
        loop {
            let mut lines = Vec::new();
            // Carry each paragraph's base direction across its wrapped lines.
            let bidi = BidiInfo::new(text, None);
            for paragraph in &bidi.paragraphs {
                for line in wrap(&text[paragraph.range.clone()], width / size, |part| {
                    Ok(self.shape_direction(part, Some(paragraph.level))?.width)
                })? {
                    lines.push(self.shape_direction(&line, Some(paragraph.level))?);
                }
            }
            if lines.len() as f64 * size * super::TR_LEADING <= height || size <= super::TR_MIN_SIZE
            {
                return Ok(Layout { size, lines });
            }
            size = (size * 0.94).max(super::TR_MIN_SIZE);
        }
    }
}

pub(super) fn collect_used<'a>(
    lines: impl Iterator<Item = &'a Line>,
    count: usize,
) -> Vec<BTreeMap<u16, String>> {
    let mut used = vec![BTreeMap::new(); count];
    for line in lines {
        for glyph in &line.glyphs {
            used[glyph.font]
                .entry(glyph.id)
                .or_insert_with(|| glyph.text.clone());
        }
    }
    used
}

/// Standard UAX #14 opportunities first; emergency wraps only at grapheme
/// boundaries, so combining marks and emoji sequences remain intact.
pub(super) fn wrap(
    text: &str,
    max_width: f64,
    measure: impl Fn(&str) -> Result<f64>,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut last_fit = 0;
    for (end, kind) in unicode_linebreak::linebreaks(text) {
        while start < end && measure(text[start..end].trim_end())? > max_width {
            let split = if last_fit > start {
                last_fit
            } else {
                let mut fit = start;
                for (offset, grapheme) in text[start..end].grapheme_indices(true) {
                    let next = start + offset + grapheme.len();
                    if fit > start && measure(text[start..next].trim_end())? > max_width {
                        break;
                    }
                    fit = next;
                }
                fit
            };
            lines.push(text[start..split].trim_end().to_owned());
            start = split;
            while start < end && text[start..].starts_with([' ', '\t']) {
                start += 1;
            }
            last_fit = start;
        }
        last_fit = end;
        if kind == unicode_linebreak::BreakOpportunity::Mandatory && start < end {
            lines.push(text[start..end].trim_end().to_owned());
            start = end;
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_breaks_keep_graphemes_and_cjk_punctuation() {
        let measure = |s: &str| Ok(s.graphemes(true).count() as f64);
        assert_eq!(
            wrap("a\u{301}a\u{301}a\u{301}", 1.0, measure).unwrap(),
            ["a\u{301}", "a\u{301}", "a\u{301}"]
        );
        assert_eq!(
            wrap("你好，世界。", 3.0, measure).unwrap(),
            ["你好，", "世界。"]
        );
        assert_eq!(wrap("a\nb", 3.0, measure).unwrap(), ["a", "b"]);
    }
    #[test]
    fn shaping_handles_ligatures_arabic_and_missing_coverage() {
        let path = std::path::Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
        if !path.is_file() {
            panic!("install fonts-dejavu-core for PDF shaping tests");
        }
        let fonts = Fonts {
            fonts: vec![PdfFont::load(path).unwrap()],
        };
        let ffi = fonts.shape("ffi").unwrap();
        assert!(ffi.glyphs.len() < 3, "font ligatures must be shaped");
        let arabic = fonts.shape("سلام").unwrap();
        assert!(arabic.glyphs.len() < 4, "lam-alef must be shaped");
        assert!(arabic.glyphs.iter().all(|glyph| glyph.id != 0));
        assert!(arabic.width > 0.0);
        assert_eq!(arabic.glyphs.first().unwrap().text, "م");
        let combining = fonts.shape("a\u{301}").unwrap();
        assert_eq!(combining.glyphs.len(), 1);
        assert!(fonts
            .shape("\u{10ffff}")
            .unwrap_err()
            .to_string()
            .contains("FERRYMAN_PDF_FALLBACK_FONTS"));
    }
}
