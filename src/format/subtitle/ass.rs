//! Advanced SubStation Alpha (`.ass` / `.ssa`) grammar.
//!
//! ASS is line-structured, not block-structured like SRT/VTT: under `[Events]`,
//! each `Dialogue:` line is one cue and its translatable text is the last field
//! (per the section's `Format:` line). Everything else — `[Script Info]`,
//! `[V4+ Styles]`, `Format:`, `Comment:`, blank lines — is passed through
//! verbatim, so styles/timing/metadata survive byte-exact.
//!
//! A leading run of `{...}` override blocks on a line (positioning, fades,
//! karaoke timing) is peeled into the framing so the model only sees the
//! visible text; interspersed per-character tags (rare, e.g. per-glyph colour)
//! stay in the text and are translated best-effort.

use super::{Cue, SubFormat};
use crate::format::OutputMode;
use anyhow::Result;
use std::collections::HashMap;

/// Marker type for the ASS/SSA grammar.
pub struct Ass;

impl SubFormat for Ass {
    const NAME: &'static str = "ass";
    fn parse(src: &str) -> Result<Vec<Cue>> {
        parse_ass(src)
    }
    fn render(cues: &[Cue], tr: &HashMap<usize, String>, mode: OutputMode) -> String {
        render_ass(cues, tr, mode)
    }
}

/// Default number of comma-separated fields before `Text` in an ASS Events
/// `Dialogue:` line (`Layer, Start, End, Style, Name, MarginL, MarginR,
/// MarginV, Effect, Text` → 9). Overridden at runtime by the file's own
/// `Format:` line if present.
const DEFAULT_TEXT_COMMAS: usize = 9;

fn parse_ass(src: &str) -> Result<Vec<Cue>> {
    let norm = src.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues: Vec<Cue> = Vec::new();
    let mut section = "";
    let mut text_commas = DEFAULT_TEXT_COMMAS;

    for line in norm.split('\n') {
        let trimmed = line.trim();
        // Section header — track it (the Styles section has its own Format:
        // line that must NOT be read as the Events field list).
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed;
            cues.push(Cue::passthrough(line));
            continue;
        }
        if section.eq_ignore_ascii_case("[events]") {
            if let Some(fmt) = trimmed.strip_prefix("Format:") {
                text_commas = fields_before_text(fmt);
                cues.push(Cue::passthrough(line));
                continue;
            }
            if let Some(rest) = line.strip_prefix("Dialogue:") {
                let (head, text_field) = split_at_comma(rest, text_commas);
                let (lead, visible) = split_leading_tags(text_field);
                // framing = "Dialogue:" + head (fields+commas) + leading tags
                cues.push(Cue {
                    text: visible.to_string(),
                    framing: format!("Dialogue:{}{}", head, lead),
                    timed: true,
                });
                continue;
            }
        }
        cues.push(Cue::passthrough(line));
    }
    Ok(cues)
}

fn render_ass(cues: &[Cue], tr: &HashMap<usize, String>, mode: OutputMode) -> String {
    let mut out: Vec<String> = Vec::with_capacity(cues.len());
    for (i, c) in cues.iter().enumerate() {
        if !c.timed {
            out.push(c.framing.clone());
            continue;
        }
        let body = match tr.get(&i) {
            None => c.text.clone(),
            Some(t) => match mode {
                OutputMode::Replace => t.clone(),
                // ASS bilingual: stack the translation under the original with a
                // hard line break (\N) inside the same Dialogue cue.
                OutputMode::Bilingual => {
                    if c.text.is_empty() {
                        t.clone()
                    } else {
                        format!("{}\\N{}", c.text, t)
                    }
                }
            },
        };
        out.push(format!("{}{}", c.framing, body));
    }
    out.join("\n") + "\n"
}

/// Number of comma-separated fields preceding `Text` in an Events `Format:`
/// line — i.e. how many commas to skip to reach the Text field on each
/// `Dialogue:` line.
fn fields_before_text(fmt: &str) -> usize {
    let fields: Vec<&str> = fmt.split(',').map(|f| f.trim()).collect();
    fields
        .iter()
        .rposition(|f| f.eq_ignore_ascii_case("Text"))
        .unwrap_or(fields.len().saturating_sub(1))
}

/// Split `s` into `(head, tail)` where `head` runs through the `n`-th comma
/// (inclusive) and `tail` is everything after — the Text field, which may
/// itself contain commas. If `s` has fewer than `n` commas, the whole string is
/// the head and the tail is empty.
fn split_at_comma(s: &str, n: usize) -> (&str, &str) {
    let mut pos = 0;
    let mut count = 0;
    while count < n {
        match s[pos..].find(',') {
            Some(idx) => {
                pos += idx + 1;
                count += 1;
            }
            None => return (s, ""),
        }
    }
    (&s[..pos], &s[pos..])
}

/// Peel a leading run of `{...}` override blocks off `text`, returning
/// `(leading_tags, rest)`. The rest is what the model should translate.
fn split_leading_tags(text: &str) -> (&str, &str) {
    let mut end = 0;
    loop {
        let rest = &text[end..];
        if rest.starts_with('{') {
            match rest.find('}') {
                Some(close) => end += close + 1,
                None => break, // unbalanced '{' — stop, leave the rest intact
            }
        } else {
            break;
        }
    }
    (&text[..end], &text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASS_SAMPLE: &str = "\
[Script Info]
Title: Demo
ScriptType: v4.00+

[V4+ Styles]
Format: Name, Fontname, Fontsize
Style: Default,Arial,20

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:12.36,0:00:13.88,Default,,0,0,0,,还差一点点
Dialogue: 0,0:00:14.86,0:00:16.11,Default,,0,0,0,,{\\an5\\fad(500,300)}魔穗字幕组
Comment: 0,0:00:20.00,0:00:21.00,Default,,0,0,0,,a hidden note
";

    #[test]
    fn parse_finds_dialogue_text_and_uses_events_format() {
        let cues = parse_ass(ASS_SAMPLE).unwrap();
        // timed = the two Dialogue lines; everything else is passthrough.
        let timed: Vec<&Cue> = cues.iter().filter(|c| c.timed).collect();
        assert_eq!(timed.len(), 2);
        assert_eq!(timed[0].text, "还差一点点");
        assert_eq!(timed[0].framing, "Dialogue: 0,0:00:12.36,0:00:13.88,Default,,0,0,0,,");
        // Leading override tags peeled into framing; visible text only.
        assert_eq!(timed[1].text, "魔穗字幕组");
        assert_eq!(
            timed[1].framing,
            "Dialogue: 0,0:00:14.86,0:00:16.11,Default,,0,0,0,,{\\an5\\fad(500,300)}"
        );
    }

    #[test]
    fn parse_passes_through_non_dialogue_lines() {
        let cues = parse_ass(ASS_SAMPLE).unwrap();
        // Styles Format: line must NOT be read as the Events field list — the
        // Events text_commas stays 9 even though the Styles Format lists 3 fields.
        let framing_set: Vec<&str> = cues.iter().map(|c| c.framing.as_str()).collect();
        assert!(framing_set.iter().any(|&f| f == "[Script Info]"));
        assert!(framing_set.iter().any(|&f| f.starts_with("Format: Layer")));
        assert!(framing_set.iter().any(|&f| f.starts_with("Comment:")));
        // The Styles Format line is preserved verbatim too.
        assert!(framing_set.iter().any(|&f| f == "Format: Name, Fontname, Fontsize"));
    }

    #[test]
    fn render_roundtrips_unchanged_lines() {
        let cues = parse_ass(ASS_SAMPLE).unwrap();
        let out = render_ass(&cues, &HashMap::new(), OutputMode::Replace);
        // Every original line survives (translate-mode irrelevant with no tr).
        for line in ASS_SAMPLE.split('\n') {
            if !line.is_empty() {
                assert!(out.contains(line), "missing original line: {line:?}");
            }
        }
    }

    #[test]
    fn render_replace_and_bilingual() {
        let cues = parse_ass(ASS_SAMPLE).unwrap();
        let timed_idx: Vec<usize> = cues
            .iter()
            .enumerate()
            .filter(|(_, c)| c.timed)
            .map(|(i, _)| i)
            .collect();
        let mut tr = HashMap::new();
        tr.insert(timed_idx[0], "almost there".to_string());
        tr.insert(timed_idx[1], "MoSui Fansub".to_string());

        let repl = render_ass(&cues, &tr, OutputMode::Replace);
        assert!(repl.contains(",almost there\n") || repl.contains(",almost there\r"));
        assert!(repl.contains("}MoSui Fansub\n") || repl.contains("}MoSui Fansub\r"));

        let bi = render_ass(&cues, &tr, OutputMode::Bilingual);
        assert!(bi.contains("还差一点点\\Nalmost there"));
        assert!(bi.contains("魔穗字幕组\\NMoSui Fansub"));
    }

    #[test]
    fn fields_before_text_handles_custom_format() {
        assert_eq!(fields_before_text("Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text"), 9);
        // A format without Text falls back to the last field.
        assert_eq!(fields_before_text("A, B, C"), 2);
    }

    #[test]
    fn split_at_comma_keeps_tail_commas_intact() {
        // Text field itself contains commas — only the first n are split on.
        let (head, tail) = split_at_comma("a,b,c,d,e", 3);
        assert_eq!(head, "a,b,c,");
        assert_eq!(tail, "d,e");
        // Fewer than n commas: whole string is head, tail empty.
        let (head, tail) = split_at_comma("a,b", 5);
        assert_eq!(head, "a,b");
        assert_eq!(tail, "");
    }

    #[test]
    fn split_leading_tags_peels_only_leading_blocks() {
        assert_eq!(split_leading_tags("{\\an5}hello"), ("{\\an5}", "hello"));
        assert_eq!(split_leading_tags("{\\a}{\\b}hi"), ("{\\a}{\\b}", "hi"));
        assert_eq!(split_leading_tags("plain"), ("", "plain"));
        // Interspersed tags after visible text are NOT peeled (best-effort).
        assert_eq!(split_leading_tags("a{\\b}c"), ("", "a{\\b}c"));
        // Unbalanced brace: stop, leave intact.
        assert_eq!(split_leading_tags("{unbalanced"), ("", "{unbalanced"));
    }
}
