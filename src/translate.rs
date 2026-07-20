//! vLLM (OpenAI-compatible) translation client.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize)]
struct ChatReq<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    top_p: f32,
    top_k: i32,
    repetition_penalty: f32,
    // Omitted → vLLM defaults to `max_model_len - prompt_len` (computed server-side
    // with its tokenizer). A fixed value can't be safe across presets: the 30B
    // preset's context is 4096, so a fixed max_tokens of 4096 leaves zero room for
    // the prompt and every request fails with HTTP 400. Letting vLLM derive it
    // fits any context length / input language without truncating short of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMsg,
}

#[derive(Deserialize)]
struct RespMsg {
    content: String,
}

/// Translate `text` into `target_lang` using the official Hy-MT2 "Default
/// Translation" prompt and recommended sampling params (for the 7B model).
///
/// Retries a few times on transient HTTP / parse errors.
pub async fn translate(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    text: &str,
    target_lang: &str,
) -> Result<String> {
    // Trim leading/trailing whitespace; keep internal structure as-is.
    let trimmed = text.trim();
    let prompt = format!(
        "Translate the following text into {tgt}. Note that you should only \
         output the translated result without any additional explanation:\n\n{text}",
        tgt = target_lang,
        text = trimmed
    );

    let body = ChatReq {
        model,
        messages: vec![Message {
            role: "user",
            content: prompt,
        }],
        temperature: 0.7,
        top_p: 0.6,
        top_k: 20,
        repetition_penalty: 1.05,
        max_tokens: None,
    };

    let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));

    let mut last_err = String::new();
    for attempt in 0..4u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500u64 * 2u64.pow(attempt))).await;
        }
        let send = client.post(&url).json(&body).send().await;
        match send {
            Ok(resp) => {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    last_err = format!("HTTP {}: {}", status, truncate(&txt, 300));
                    // 4xx (except 429 Too Many Requests) are permanent — bad model
                    // id, malformed request, or a block over the context window.
                    // Retrying just burns a concurrency slot for several seconds
                    // and can never succeed, so fail this block immediately.
                    if status.is_client_error()
                        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        return Err(anyhow!(
                            "translation failed (fatal HTTP): {}",
                            last_err
                        ));
                    }
                    continue;
                }
                match serde_json::from_str::<ChatResp>(&txt) {
                    Ok(parsed) => match parsed.choices.into_iter().next() {
                        Some(c) => return Ok(c.message.content.trim().to_string()),
                        None => {
                            last_err = "empty choices".into();
                            continue;
                        }
                    },
                    Err(e) => {
                        last_err = format!("parse: {} | {}", e, truncate(&txt, 200));
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = format!("request: {}", e);
                continue;
            }
        }
    }
    Err(anyhow!("translation failed after retries: {}", last_err))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Translate a batch of subtitle cues into `target_lang`, returning exactly
/// `cues.len()` translations in order (one per cue).
///
/// This is the batched counterpart to [`translate`]: instead of one text per
/// request (which starves the model of cross-cue context for short subtitle
/// lines), all cues go out behind a single prompt and the response is aligned
/// by `#N` numbered blocks. `context` carries a few preceding cues as
/// read-only narrative context (not translated, not counted in the output).
///
/// **Alignment guarantee**: the model must return one `#1..=#N` block per cue,
/// in order. [`parse_numbered`] enforces this strictly — any missing / extra /
/// reordered entry fails the parse and triggers a retry; a batch that still
/// won't align after the retries fails wholesale (the engine then emits those
/// cues unchanged, never silently merging or splitting them).
pub async fn translate_batch(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    cues: &[&str],
    context: &[&str],
    target_lang: &str,
) -> Result<Vec<String>> {
    let prompt = build_batch_prompt(cues, context, target_lang);
    let body = ChatReq {
        model,
        messages: vec![Message {
            role: "user",
            content: prompt,
        }],
        temperature: 0.7,
        top_p: 0.6,
        top_k: 20,
        repetition_penalty: 1.05,
        max_tokens: None,
    };

    let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));

    let mut last_err = String::new();
    for attempt in 0..4u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500u64 * 2u64.pow(attempt))).await;
        }
        match client.post(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    last_err = format!("HTTP {}: {}", status, truncate(&txt, 300));
                    // 4xx (except 429) is permanent — a whole bad batch shouldn't
                    // burn 4 concurrency slots. Fail it now.
                    if status.is_client_error()
                        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        return Err(anyhow!(
                            "batch translation failed (fatal HTTP): {}",
                            last_err
                        ));
                    }
                    continue;
                }
                let content = match serde_json::from_str::<ChatResp>(&txt) {
                    Ok(parsed) => match parsed.choices.into_iter().next() {
                        Some(c) => c.message.content,
                        None => {
                            last_err = "empty choices".into();
                            continue;
                        }
                    },
                    Err(e) => {
                        last_err = format!("parse json: {} | {}", e, truncate(&txt, 200));
                        continue;
                    }
                };
                match parse_numbered(&content, cues.len()) {
                    Some(trs) => return Ok(trs),
                    // Misalignment is transient (the model usually self-corrects
                    // on retry); keep last_err informative for the final bail.
                    None => {
                        last_err = format!(
                            "alignment: expected {} entries | response: {}",
                            cues.len(),
                            truncate(&content, 200)
                        );
                        continue;
                    }
                }
            }
            Err(e) => {
                last_err = format!("request: {}", e);
                continue;
            }
        }
    }
    Err(anyhow!(
        "batch translation failed after retries: {}",
        last_err
    ))
}

/// Build the validated `#N` numbered-block prompt for a subtitle batch.
///
/// Format (empirically tuned on Hy-MT2 — 25/25 perfect alignment at N=25 with
/// a 5-line context block, rep_pen 1.05 vs 1.0 identical): instructions, an
/// optional unnumbered `Context:` section (excluded from parsing), then one
/// `#i\n<cue>` block per cue joined by blank lines.
fn build_batch_prompt(cues: &[&str], context: &[&str], target: &str) -> String {
    let n = cues.len();
    let mut out = format!(
        "Translate the following {n} subtitle lines into {tgt}. Output each \
         translation on the line right after its number, keeping the number \
         prefix. Output exactly {n} numbered entries (#{a} to #{b}), in the \
         same order. Do NOT merge, split, add, or skip any line. No \
         explanations.\n\n",
        n = n,
        tgt = target,
        a = 1,
        b = n
    );
    if !context.is_empty() {
        out.push_str(
            "The lines below marked 'Context' are for context only — do NOT \
             translate or number them.\n\nContext:\n",
        );
        out.push_str(&context.join("\n"));
        out.push_str("\n\n");
    }
    let blocks: Vec<String> = cues
        .iter()
        .enumerate()
        .map(|(i, c)| format!("#{i}\n{c}", i = i + 1, c = c))
        .collect();
    out.push_str(&blocks.join("\n\n"));
    out
}

/// Parse a `#N`-numbered response into exactly `count` aligned translations.
///
/// A number marker is a line that is *exactly* `#` followed by digits (trimmed
/// of surrounding space), matching what the model emits. The text of entry `i`
/// is everything between `#i` and `#i+1`, with only outer whitespace trimmed —
/// internal newlines (multi-line cues) are preserved.
///
/// Returns `Some` only when the markers are *exactly* `{1..=count}` (no
/// missing, no extra, no duplicates); otherwise `None`. Mapping by number (not
/// position) makes the parse robust to mild reordering while still rejecting a
/// wrong count — so the one-to-one correspondence is enforced structurally.
fn parse_numbered(resp: &str, count: usize) -> Option<Vec<String>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<u32, String> = BTreeMap::new();
    let mut cur: Option<u32> = None;
    let mut buf: Vec<&str> = Vec::new();
    // Flush the buffered text under `cur` into the map.
    let flush = |cur: &mut Option<u32>, buf: &mut Vec<&str>, map: &mut BTreeMap<u32, String>| {
        if let Some(n) = cur.take() {
            map.insert(n, buf.join("\n").trim().to_string());
        }
        buf.clear();
    };
    for line in resp.split('\n') {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim();
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                flush(&mut cur, &mut buf, &mut map);
                cur = rest.parse::<u32>().ok();
                continue;
            }
        }
        if cur.is_some() {
            buf.push(line);
        }
    }
    flush(&mut cur, &mut buf, &mut map);

    // Exactly the keys {1..=count} required.
    if map.len() != count {
        return None;
    }
    (1..=count as u32).map(|n| map.get(&n).cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numbered_perfect_alignment() {
        let resp = "#1\n你好\n\n#2\n世界\n\n#3\n再见";
        let trs = parse_numbered(resp, 3).unwrap();
        assert_eq!(trs, vec!["你好", "世界", "再见"]);
    }

    #[test]
    fn parse_numbered_preserves_multiline_cues() {
        let resp = "#1\n第一行\n第二行\n\n#2\n世界";
        let trs = parse_numbered(resp, 2).unwrap();
        assert_eq!(trs, vec!["第一行\n第二行", "世界"]);
    }

    #[test]
    fn parse_numbered_robust_to_reordering() {
        // Numbered (not positional): #2 before #1 still maps correctly.
        let resp = "#2\n世界\n\n#1\n你好";
        let trs = parse_numbered(resp, 2).unwrap();
        assert_eq!(trs, vec!["你好", "世界"]);
    }

    #[test]
    fn parse_numbered_rejects_missing_entry() {
        // Model merged two cues → only 2 entries for 3 cues.
        let resp = "#1\n你好\n\n#2\n世界再见";
        assert!(parse_numbered(resp, 3).is_none());
    }

    #[test]
    fn parse_numbered_rejects_extra_entry() {
        let resp = "#1\n你好\n\n#2\n世界\n\n#3\n再见\n\n#4\n多余";
        assert!(parse_numbered(resp, 3).is_none());
    }

    #[test]
    fn parse_numbered_rejects_shifted_keys() {
        // Right count but wrong keys (2..=4 instead of 1..=3).
        let resp = "#2\n你好\n\n#3\n世界\n\n#4\n再见";
        assert!(parse_numbered(resp, 3).is_none());
    }

    #[test]
    fn parse_numbered_ignores_context_section() {
        // Unnumbered context lines before the first marker must not leak in.
        let resp = "Context:\nFoo\nBar\n\n#1\n你好\n\n#2\n世界";
        let trs = parse_numbered(resp, 2).unwrap();
        assert_eq!(trs, vec!["你好", "世界"]);
    }

    #[test]
    fn parse_numbered_tolerates_space_after_hash() {
        let resp = "# 1\n你好\n\n# 2\n世界";
        let trs = parse_numbered(resp, 2).unwrap();
        assert_eq!(trs, vec!["你好", "世界"]);
    }

    #[test]
    fn build_batch_prompt_shape_with_context() {
        let p = build_batch_prompt(&["a", "b"], &["ctx1", "ctx2"], "中文");
        // Instructions name the count and range.
        assert!(p.contains("2 subtitle lines"));
        assert!(p.contains("#1 to #2"));
        // Context block present and unnumbered.
        assert!(p.contains("Context:\nctx1\nctx2"));
        // Numbered cue blocks.
        assert!(p.contains("#1\na"));
        assert!(p.contains("#2\nb"));
        // The context lines are NOT numbered.
        assert!(!p.contains("#ctx"));
    }

    #[test]
    fn build_batch_prompt_omits_context_section_when_empty() {
        let p = build_batch_prompt(&["a"], &[], "中文");
        assert!(!p.contains("Context:"));
        assert!(p.contains("#1\na"));
    }
}
