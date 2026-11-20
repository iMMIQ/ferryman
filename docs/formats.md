# Supported Formats & Behavior

Ferryman translates nine document formats through a unified format-agnostic engine. Every format supports both **`bilingual`** (keep original + translation) and **`replace`** (translation only) output modes, and leaves untranslated segments in their original text if a request fails or hits `--limit`.

## Format Matrix

| Format | Extensions | Translation Unit | Strategy | Bilingual Output | Replace Output | Untouched / Skipped |
| --- | --- | --- | --- | --- | --- | --- |
| EPUB | `.epub` | Leaf HTML blocks | Independent | Inserts `.hy-zh` sibling block after original | Replaces text in place, distributing across inline markup | EPUB3 `nav`, `head`, `title`, `script`, `style`, `pre`, `code`, `textarea`, `svg`, `td`/`th`, `div` |
| DOCX | `.docx` | Body paragraphs (`<w:p>`) | Independent | Appends translated paragraph after original | Replaces text across existing runs, preserving styling/links | Headers, footers, footnotes, endnotes; legacy binary `.doc` is unsupported |
| PDF | `.pdf` | Reconstructed visual paragraphs | Independent | Inserts mirrored translation page after each original page | Covers original paragraph box and draws translation in place | TOC leader lines, page numbers, letterless lines, figures/vectors; scanned/encrypted PDFs fail |
| Markdown | `.md`, `.markdown` | CommonMark text nodes | Batched | Duplicates whole block (footnotes append inline ` / <translation>`) | Replaces text nodes in place with Markdown escaping | Leading `---`/`+++` frontmatter, code blocks, inline code, HTML, link URLs, autolinks, reference definitions |
| Plain text | `.txt` | Non-blank lines | Batched | Emits original line followed by translated line | Replaces line text | Blank lines and structural lines starting with `[` |
| SubRip / WebVTT | `.srt`, `.vtt` | Timed cues (`-->`) | Batched | Appends translation lines beneath original inside the cue | Replaces cue text lines | Cue indexes/timestamps, `WEBVTT` headers, `NOTE`/`STYLE`/`REGION` blocks |
| ASS / SSA | `.ass`, `.ssa` | `[Events]` `Dialogue:` lines | Batched | Stacks original and translation with `\N` inside the cue | Replaces visible dialogue text | `[Script Info]`, `[V4+ Styles]`, `Format:`, `Comment:`, leading `{...}` override tags |
| LRC | `.lrc` | Timestamped lyric lines | Batched | Emits original line then translated line with the same timestamp prefix | Replaces lyric text after timestamp prefix | `[ti:]`/`[ar:]` metadata tags, untimed lines, blanks |

### Translation Strategies

- **Independent** (`EPUB`, `DOCX`, `PDF`): sends one block/paragraph per request. An oversized segment is split automatically at a language boundary and recombined if it exceeds the model's context window.
- **Batched** (`Markdown`, `TXT`, `SRT`, `VTT`, `ASS/SSA`, `LRC`): sends consecutive segments wrapped in `<cN>...</cN>` tags together with preceding read-only context lines (defaults: 25 segments per batch, 5 context segments). Tags are parsed by number so one dropped cue never shifts its neighbors; missing/truncated batches halve automatically. Long runs of identical characters (e.g., ASR stutter artifacts) are capped to 8 characters before sending to the model while keeping the original source untouched.

---

## EPUB

Ferryman reads `META-INF/container.xml` and the OPF spine (skipping the EPUB3 `nav` document) and rewrites each content XHTML document while preserving non-text assets and valid ZIP layout (`mimetype` stored uncompressed first).

- **Leaf block selection**: tracks `p`, `h1`–`h6`, `li`, `blockquote`, `figcaption`, `dt`, and `dd`. Container blocks that hold another tracked block are skipped to prevent double-translation. Table cells (`td`/`th`) and generic `div` wrappers are intentionally excluded to avoid nested duplication and invalid XHTML.
- **Suppressed regions**: content inside `head`, `title`, `script`, `style`, `pre`, `code`, `nav`, `textarea`, and `svg` is never sent for translation.
- **`bilingual` mode**: injects a `.hy-zh` stylesheet into `<head>` and appends an HTML-escaped sibling element (`<p class="hy-zh">`, or matching `<li>`/`<dt>`/`<dd>` to keep lists valid). Inline markup (such as `<strong>` or `<a>`) stays intact in the original block, while the appended translation is plain text.
- **`replace` mode**: replaces leaf text in place while preserving chapter markup, links, images, styles, and archive assets. Inline styling spans are preserved by distributing translated graphemes proportionally across the original text runs.

## DOCX

DOCX files are processed as OOXML ZIP archives targeting the main document body (`word/document.xml`).

- **Paragraph assembly**: consecutive `<w:t>` runs within each `<w:p>` (including nested text boxes) are concatenated into one segment so Word's run fragmentation does not corrupt sentences. DrawingML text (`a:p`/`a:t`) is excluded.
- **`bilingual` mode**: inserts a translated paragraph immediately after the original paragraph.
- **`replace` mode**: distributes translated text across existing text runs, retaining paragraph/run styling, hyperlinks, bookmarks, images, tables, and text boxes; tabs and line breaks in translations become Word control elements.
- **Scope**: headers, footers, footnotes, and endnotes (`word/header*.xml`, `word/footnotes.xml`, etc.) pass through unchanged. Legacy binary `.doc` files are not supported.

## PDF

Because PDF stores positioned glyph runs rather than logical paragraphs, Ferryman reconstructs paragraph layout from geometry and edits the original PDF bytes in place with `lopdf` so diagrams, images, and vector graphics remain intact.

- **Extraction**: groups positioned characters into visual lines and lines into paragraphs across wraps and page breaks (including dehyphenation, ligature normalization, and repair of broken `/W` glyph-width arrays). Table-of-contents lines (dot leaders or right-aligned page numbers), standalone page numbers, and letterless lines pass through untouched.
- **`bilingual` mode**: inserts a same-sized translation page after every original page and draws each translated paragraph at its mirrored coordinates (shrinking font size down to fit its slot). Original pages are untouched.
- **`replace` mode**: paints a background rectangle over each paragraph bounding box on the original page and draws the translated text inside that box.
- **Fonts & scripts**:
  - Rendering requires one CJK-capable OpenType font (`.ttc`/`.otf`/`.ttf`). Discovery checks `FERRYMAN_PDF_FONT` first, then `/app/fonts/NotoSansCJK-Regular.ttc` (bundled in Lazycat packages), then standard system paths for Noto Sans/Serif CJK, WenQuanYi, and Droid Sans Fallback.
  - Fallback fonts for additional scripts are discovered in `/app/fonts` and standard Noto/DejaVu directories (`NotoSans`, `NotoSansArabic`, `NotoSansHebrew`, `NotoSansDevanagari`, `NotoSansThai`, `DejaVuSans`), or supplied explicitly via `FERRYMAN_PDF_FALLBACK_FONTS` (OS path-list, colon-separated on Linux).
  - Fonts are validated on document open and embedded as **subset CID/Type0 fonts** containing only the glyphs actually drawn, together with `ToUnicode` maps and logical `ActualText`. Shaping (`rustybuzz`), bidirectional ordering, and Unicode grapheme line breaking preserve combining marks, ligatures, and RTL text.
  - Missing glyph coverage fails explicitly with a hint rather than silently dropping characters.
- **Limitations**: scanned PDFs without an embedded text layer and encrypted PDFs are rejected with an error. Original text color is not exposed by the extractor and backgrounds are assumed white in `replace` mode.

## Markdown

Markdown files (`.md`, `.markdown`) are parsed with CommonMark extensions (tables, strikethrough, task lists, footnotes) and updated via original byte ranges:

- Leading YAML (`---`) or TOML (`+++`) frontmatter, fenced/indented code blocks, inline code, HTML blocks, link URLs, autolinks, and reference definitions are never translated.
- Translated text nodes are escaped so model output cannot inject unintended Markdown syntax.
- `bilingual` mode duplicates each translated top-level block (preserving inline structure); footnote definitions append ` / <translation>` inline so footnote anchors remain unique.
- Source line endings (`LF` / `CRLF`) and whitespace round-trip verbatim when no translation is applied.

## Plain Text

Plain text (`.txt`) treats each non-blank line as one segment, except structural lines whose first non-whitespace character is `[` (e.g., `[chapter:...]` markers or catalog headers), which pass through verbatim. Blank lines and trailing-newline conventions are preserved.

## Subtitles & Lyrics

- **SRT / VTT**: blank-line-separated blocks containing `-->` are translated; cue numbers, timestamps, `WEBVTT` headers, and `NOTE`/`STYLE`/`REGION` blocks pass through unchanged. `bilingual` stacks translation lines after the original text inside each cue.
- **ASS / SSA**: under `[Events]`, each `Dialogue:` line's `Text` field (resolved dynamically from the section's `Format:` header) is translated; leading `{...}` override blocks stay in the framing so positioning/fade tags survive untouched. `bilingual` joins original and translation with `\N`. Everything outside `Dialogue:` lines passes through verbatim.
- **LRC**: leading timestamp tokens (`[mm:ss.xx]`, including multi-timestamp lines) are preserved as framing; metadata tags (`[ti:...]`, `[ar:...]`) pass through verbatim. `bilingual` emits the original line followed by the translation line carrying the same timestamp prefix.

## Resource & Archive Safety Limits

- **ZIP archives (`EPUB` / `DOCX`)**: capped at **10,000 entries**, **128 MiB** per decompressed entry, and **512 MiB** total decompressed size (enforced on both advertised headers and actual decompressed byte reads).
- **Parser concurrency**: document parsing runs on blocking threads capped at **2 concurrent parsers per process**. If a run is cancelled while parsing, callers stop waiting immediately while the running parser retains its concurrency slot until it exits.
