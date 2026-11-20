//! LRC lyrics grammar (`.lrc`).
//!
//! LRC is line-oriented like ASS, not block-oriented like SRT: each line is
//! `[mm:ss.xx]lyric`, optionally with several timestamps sharing one lyric
//! (`[t1][t2]text`). Everything else — `[ti:…]`/`[ar:…]` ID tags, blank lines —
//! is passed through verbatim. The leading timestamp token(s) are the cue's
//! framing (emitted unchanged so timing survives byte-exact); the text after
//! them is what the model translates.
//!
//! Rides the subtitle batched path — short lyric lines need cross-line context
//! — so [`crate::format::SubtitleDoc`]`<`[`Lrc`]`>` inherits
//! [`crate::format::Strategy::Batched`].

use super::{Cue, SubFormat};
use crate::format::OutputMode;
use anyhow::Result;
use std::collections::HashMap;

/// Marker type for the LRC grammar.
pub struct Lrc;

impl SubFormat for Lrc {
    const NAME: &'static str = "lrc";
    fn parse(src: &str) -> Result<Vec<Cue>> {
        parse_lrc(src)
    }
    fn render(cues: &[Cue], tr: &HashMap<usize, String>, mode: OutputMode) -> String {
        render_lrc(cues, tr, mode)
    }
}

fn parse_lrc(src: &str) -> Result<Vec<Cue>> {
    let norm = src.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues: Vec<Cue> = Vec::new();
    for line in norm.split('\n') {
        let (prefix, rest) = peel_leading_time_tags(line);
        if prefix.is_empty() {
            // No timestamp token → blank line, `[ti:…]` metadata, or untimed
            // text: pass through unchanged.
            cues.push(Cue::passthrough(line));
        } else {
            cues.push(Cue {
                text: rest.to_string(),
                framing: prefix,
                timed: true,
            });
        }
    }
    Ok(cues)
}

fn render_lrc(cues: &[Cue], tr: &HashMap<usize, String>, mode: OutputMode) -> String {
    let mut out: Vec<String> = Vec::with_capacity(cues.len());
    for (i, c) in cues.iter().enumerate() {
        if !c.timed {
            out.push(c.framing.clone());
            continue;
        }
        match tr.get(&i) {
            None => out.push(format!("{}{}", c.framing, c.text)),
            Some(t) => match mode {
                OutputMode::Replace => out.push(format!("{}{}", c.framing, t)),
                // Bilingual: the original lyric, then the translation on a new
                // line sharing the SAME timestamp prefix (a player shows both).
                OutputMode::Bilingual => {
                    if c.text.is_empty() {
                        out.push(format!("{}{}", c.framing, t));
                    } else {
                        out.push(format!("{}{}", c.framing, c.text));
                        out.push(format!("{}{}", c.framing, t));
                    }
                }
            },
        }
    }
    // No forced trailing newline: split('\n') of a file ending in '\n' yields a
    // trailing "" passthrough cue, so join("\n") round-trips the file exactly.
    out.join("\n")
}

/// Peel every leading `[…]` *timestamp* token off `line`, returning
/// `(prefix, rest)`. A token is a timestamp iff its first `:`-separated field
/// is all digits (covers `mm:ss`, `mm:ss.xx`, `mm:ss.xxx`, enhanced `mm:ss:ff`).
/// Non-timestamp tokens (`[ti:…]`, `[ar:…]`, `[offset:…]`, unbalanced `[`) stop
/// peeling, so metadata lines fall through to passthrough with an empty prefix.
fn peel_leading_time_tags(line: &str) -> (String, &str) {
    let mut prefix = String::new();
    let mut rest = line;
    while let Some(after) = rest.strip_prefix('[') {
        let Some(close) = after.find(']') else {
            break; // unbalanced '['
        };
        let tag = &after[..close];
        if is_lrc_timestamp(tag) {
            prefix.push('[');
            prefix.push_str(tag);
            prefix.push(']');
            rest = &after[close + 1..];
        } else {
            break; // metadata tag — leave it (and the rest) for passthrough
        }
    }
    (prefix, rest)
}

fn is_lrc_timestamp(tag: &str) -> bool {
    let mut parts = tag.split(':');
    match parts.next() {
        Some(first) if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) => {
            parts.next().is_some() // need at least mm:ss
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LRC_SAMPLE: &str = "\
[ti:Song Title]
[ar:Artist]
[00:01.01]轻声耳语
[00:06.51]杂鱼魔族的谄媚
[00:10.00][00:20.00]repeated line
";

    #[test]
    fn parse_separates_timestamps_from_text() {
        let cues = parse_lrc(LRC_SAMPLE).unwrap();
        let timed: Vec<&Cue> = cues.iter().filter(|c| c.timed).collect();
        assert_eq!(timed.len(), 3);
        assert_eq!(timed[0].framing, "[00:01.01]");
        assert_eq!(timed[0].text, "轻声耳语");
        // multi-timestamp line: both prefixes peeled into framing.
        assert_eq!(timed[2].framing, "[00:10.00][00:20.00]");
        assert_eq!(timed[2].text, "repeated line");
    }

    #[test]
    fn parse_passes_through_metadata_and_blanks() {
        let cues = parse_lrc(LRC_SAMPLE).unwrap();
        let pass: Vec<&Cue> = cues.iter().filter(|c| !c.timed).collect();
        // [ti:…], [ar:…], and the trailing "" from the final \n.
        assert_eq!(pass.len(), 3);
        assert_eq!(pass[0].framing, "[ti:Song Title]");
        assert_eq!(pass[1].framing, "[ar:Artist]");
        assert!(pass[2].framing.is_empty());
    }

    #[test]
    fn render_replace_swaps_only_text() {
        let cues = parse_lrc(LRC_SAMPLE).unwrap();
        // timed cues sit at indices 2,3,4 (after [ti], [ar]).
        let mut tr = HashMap::new();
        tr.insert(2, "whisper".to_string());
        tr.insert(4, "again".to_string());
        let out = render_lrc(&cues, &tr, OutputMode::Replace);
        assert!(out.contains("[00:01.01]whisper"));
        assert!(out.contains("[00:10.00][00:20.00]again"));
        // untranslated cue keeps original.
        assert!(out.contains("[00:06.51]杂鱼魔族的谄媚"));
    }

    #[test]
    fn render_bilingual_dups_timestamp_for_translation() {
        let cues = parse_lrc(LRC_SAMPLE).unwrap();
        let mut tr = HashMap::new();
        tr.insert(2, "whisper".to_string());
        let out = render_lrc(&cues, &tr, OutputMode::Bilingual);
        assert!(out.contains("[00:01.01]轻声耳语"));
        assert!(out.contains("[00:01.01]whisper"));
    }

    #[test]
    fn roundtrip_unchanged_with_no_translations() {
        let cues = parse_lrc(LRC_SAMPLE).unwrap();
        let out = render_lrc(&cues, &HashMap::new(), OutputMode::Replace);
        assert_eq!(out, LRC_SAMPLE);
    }

    #[test]
    fn is_lrc_timestamp_classifies_correctly() {
        assert!(is_lrc_timestamp("00:01.01"));
        assert!(is_lrc_timestamp("1:23"));
        assert!(is_lrc_timestamp("00:01:20")); // enhanced mm:ss:ff
        assert!(!is_lrc_timestamp("ti:Song"));
        assert!(!is_lrc_timestamp("ar:Artist"));
        assert!(!is_lrc_timestamp("offset:500"));
        assert!(!is_lrc_timestamp("00")); // no colon
    }
}
