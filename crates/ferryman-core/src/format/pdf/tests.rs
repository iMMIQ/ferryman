use super::extract::*;
use super::font::*;
use super::layout::*;
use super::sfnt::*;
use super::*;
use lopdf::Dictionary;

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
    let font = PdfFont::shared()
        .expect("PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT");
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
        let mut page =
            doc.start_page_with(krilla::page::PageSettings::from_wh(w as f32, h as f32).unwrap());
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
    PdfFont::shared()
        .expect("PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT");
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
    if pdf.fonts.fonts[0].standalone.len() > 2_000_000 {
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
            largest_font * 4 < pdf.fonts.fonts[0].standalone.len(),
            "embedded font {largest_font} should be a small subset of {}",
            pdf.fonts.fonts[0].standalone.len()
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
    PdfFont::shared()
        .expect("PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT");
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
    let tmp = std::env::temp_dir().join(format!("ferryman_pdf_nest_{}.pdf", std::process::id()));
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
    PdfFont::shared()
        .expect("PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT");
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
    PdfFont::shared()
        .expect("PDF tests require a CJK font; install fonts-noto-cjk or set FERRYMAN_PDF_FONT");
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
    let tmp = std::env::temp_dir().join(format!("ferryman_pdf_repl_{}.pdf", std::process::id()));
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
