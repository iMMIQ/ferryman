use super::extract::{is_translatable, MediaBoxGeo, RawLine};
#[cfg(test)]
use super::text;

// ── paragraph reconstruction ────────────────────────────────────────────────

/// Column geometry a page's lines imply (medians are robust to headings and
/// strays).
#[derive(Debug, Clone, Copy)]
pub(super) struct PageCols {
    pub(super) x0: f64,
    pub(super) x1: f64,
    pub(super) leading: f64,
}

pub(super) fn page_cols(lines: &[RawLine]) -> PageCols {
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
pub(super) fn should_merge(l: &RawLine, b: &RawLine, cols: &PageCols) -> bool {
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
pub(super) fn is_page_number(s: &str) -> bool {
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
pub(super) fn join_lines(a: &str, b: &str) -> String {
    if let Some(head) = a.strip_suffix('-') {
        if b.chars().next().is_some_and(|c| c.is_lowercase()) {
            return format!("{head}{b}");
        }
    }
    format!("{a} {b}")
}

/// A reconstructed paragraph: its original lines with geometry, the joined
/// segment text, and the box it occupies on the page.
pub(super) struct Para {
    pub(super) lines: Vec<RawLine>,
    pub(super) text: String,
    pub(super) x0: f64,
    pub(super) x1: f64,
    pub(super) top: f64,
    pub(super) bot: f64,
    pub(super) size: f64,
    pub(super) translatable: bool,
    /// Continuation of an earlier page's paragraph. It shares that segment
    /// and receives a proportion of its translation when writing.
    pub(super) merged_up: bool,
    pub(super) merged_into: Option<(usize, usize)>,
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

    pub(super) fn first(&self) -> &RawLine {
        &self.lines[0]
    }
    fn last(&self) -> &RawLine {
        &self.lines[self.lines.len() - 1]
    }
}

/// Group one page's sorted lines into paragraphs.
pub(super) fn build_paragraphs(lines: Vec<RawLine>) -> Vec<Para> {
    let cols = page_cols(&lines);
    group_lines(lines, &cols)
}

/// [`build_paragraphs`] with the column geometry supplied, so unit tests can
/// feed a realistic body column without building a full page of full lines.
pub(super) fn group_lines(lines: Vec<RawLine>, cols: &PageCols) -> Vec<Para> {
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
pub(super) fn should_merge_across(
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
pub(super) fn merge_across_pages(pages: &mut [(MediaBoxGeo, Vec<Para>)]) {
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
pub(super) fn page_cols_if(paras: &[Para]) -> Option<PageCols> {
    let lines: Vec<&RawLine> = paras.iter().flat_map(|p| p.lines.iter()).collect();
    if lines.is_empty() {
        return None;
    }
    let owned: Vec<RawLine> = lines.into_iter().cloned().collect();
    Some(page_cols(&owned))
}

// ── line wrapping (for fitting translations into slots) ─────────────────────

#[cfg(test)]
pub(super) fn wrap_text(text: &str, max_w: f32, char_width: impl Fn(char) -> f32) -> Vec<String> {
    self::text::wrap(text, max_w as f64, |s| {
        Ok(s.chars().map(&char_width).sum::<f32>() as f64)
    })
    .unwrap()
}
