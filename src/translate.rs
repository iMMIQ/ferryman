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
                    if status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        return Err(anyhow!("translation failed (fatal HTTP): {}", last_err));
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

/// Truncate `s` to at most `n` bytes for an error message, never slicing
/// through a multi-byte code point (which would panic — common with CJK
/// content, where byte index `n` routinely lands mid-character). Appends `…`
/// when truncated. Floors `n` back to the nearest char boundary.
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Translate a batch of segments (subtitle cues, prose lines) into
/// `target_lang`, returning one `Option<String>` per segment, aligned by
/// position (`Some` = translated; `None` = the model skipped/failed it and the
/// engine keeps the original). `context` carries a few preceding cues as
/// read-only narrative context (not translated, not counted).
///
/// If the whole batch's prompt is too long for the model's context window (a
/// 25-line prose batch can run to several thousand tokens — see the `max_tokens`
/// note on `ChatReq`), the batch is split in half and each side translated
/// separately; see [`translate_split`]. Retries, partial results, and the
/// fatal-4xx rule live in [`translate_one_batch`].
pub async fn translate_batch(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    cues: &[&str],
    context: &[&str],
    target_lang: &str,
) -> Vec<Option<String>> {
    translate_split(client, endpoint, model, cues, context, target_lang).await
}

/// Translate a slice of cues, halving it whenever the batch comes back missing
/// ≥2 cues. Two failure modes trigger a split, both cured by smaller batches:
/// the prompt overflowing the context window (input too long — every cue lost),
/// or the model's output being truncated before every `<cN>` tag closes (it then
/// drops the trailing cues). A single missing cue is left as-is — that's the
/// by-design "one bad cue costs only itself" case, not worth a split. Each half
/// is a fresh tagged batch with its own `<c1>…` numbering; the results are
/// concatenated in order, so the caller's positional alignment (`trs[idx]` ↔
/// cue `idx`) is preserved.
///
/// A lone cue that still overflows the whole window — one line longer than
/// `max_model_len` — can't be split further and is left untranslated, costing
/// only itself (mirrors the "one bad cue" guarantee).
async fn translate_split(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    cues: &[&str],
    context: &[&str],
    target_lang: &str,
) -> Vec<Option<String>> {
    let SliceOutcome {
        translations,
        overflow,
    } = translate_one_batch(client, endpoint, model, cues, context, target_lang).await;
    let missing = cues.len() - translations.iter().filter(|t| t.is_some()).count();
    if missing >= 2 && cues.len() > 1 {
        // The batch came back missing ≥2 cues. Two causes, both fixed by halving
        // the slice so each side has more room: either the prompt overflowed the
        // context window (`overflow` — input too long, every cue lost), or the
        // model's output was truncated before every `<cN>` tag closed and it
        // dropped the trailing cues. (A single missing cue is left as-is — that's
        // the by-design "one bad cue costs only itself" case, not worth a split.)
        // Each half is a fresh tagged batch with its own `<c1>…` numbering and
        // reuses the same read-only `context` (the right half sees slightly older
        // narrative, fine for fluency).
        let mid = cues.len() / 2;
        let (left, right) = cues.split_at(mid);
        // `Box::pin` the recursive calls: a recursive `async fn`'s future would
        // otherwise be infinitely sized (its size nests its own recursive
        // future). Boxing makes it an opaque heap pointer.
        let mut out = Box::pin(translate_split(
            client,
            endpoint,
            model,
            left,
            context,
            target_lang,
        ))
        .await;
        out.extend(
            Box::pin(translate_split(
                client,
                endpoint,
                model,
                right,
                context,
                target_lang,
            ))
            .await,
        );
        out
    } else if overflow && cues.len() == 1 {
        // A lone cue longer than the whole context window: can't split further.
        // (`translate_one_batch` suppresses its own warn on overflow expecting
        // the caller to split, so name the cause here.)
        eprintln!("warn: 1 cue is longer than the model's context window — left untranslated");
        translations
    } else {
        // Complete, or ≤1 cue missing (a degenerate cue the model refused — kept
        // original; already warned by `translate_one_batch` if it dropped all).
        translations
    }
}

/// One attempt's worth of translations for a fixed cue slice, plus whether the
/// prompt was too long for the model's context window (recoverable by
/// [`translate_split`] halving the slice).
struct SliceOutcome {
    translations: Vec<Option<String>>,
    /// Every attempt failed with a context-length-overflow 4xx — the prompt
    /// itself doesn't fit the window. `false` for any other outcome.
    overflow: bool,
}

/// Did a fatal 4xx come from the prompt not fitting the model's context window?
/// vLLM's body reads "This model's maximum context length is N tokens … reduce
/// the length of the input prompt". Such a batch is recoverable by splitting;
/// other 4xx (bad model id, malformed request) are not.
fn is_context_overflow(err: &str) -> bool {
    err.contains("context length") || err.contains("input prompt")
}

/// Send one cue slice as a tagged batch, retrying transient failures and
/// keeping the attempt that translated the most cues (so a flaky batch still
/// yields what it could). Never aborts on a partial result — a degenerate cue
/// the model refuses costs only itself, not its batch-mates. A fatal 4xx (except
/// 429) stops early. Sets `overflow` on a context-length-overflow 4xx so
/// [`translate_split`] can react by splitting; never warns on overflow (the
/// caller handles it).
async fn translate_one_batch(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    cues: &[&str],
    context: &[&str],
    target_lang: &str,
) -> SliceOutcome {
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
        // Omitted on purpose — same rationale as the Single `translate()` path
        // (see the `ChatReq.max_tokens` note above). A fixed cap is unsafe here:
        // TXT novels batch 25 long paragraphs whose prompt alone can exceed 2048
        // tokens, so a `max_tokens` of 2048 overflows the 30B preset's 4096
        // context and vLLM 400s the whole request, dropping all 25 cues. Letting
        // vLLM derive `max_model_len - prompt_len` fits any input that fits the
        // window at all; a prompt that still overflows the whole window is
        // recovered by `translate_split` halving the batch. The runaway risk a
        // cap once guarded is already handled by `sanitize_for_model` (collapsing
        // repetitive char runs) plus the server-side context limit.
        max_tokens: None,
    };

    let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));

    let mut best: Vec<Option<String>> = (0..cues.len()).map(|_| None).collect();
    let mut best_count = 0;
    let mut last_err = String::new();
    let mut overflow = false;

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
                    // 4xx (except 429) is permanent — retries can't fix it. A
                    // context-length overflow is recoverable by the caller
                    // splitting the batch, so flag it rather than giving up.
                    if status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        if is_context_overflow(&last_err) {
                            overflow = true;
                        }
                        break;
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
                let parsed = parse_tagged(&content, cues.len());
                let count = parsed.iter().filter(|x| x.is_some()).count();
                if count > best_count {
                    best_count = count;
                    best = parsed;
                }
                if best_count == cues.len() {
                    break; // perfect — stop retrying
                }
            }
            Err(e) => {
                last_err = format!("request: {}", e);
                continue;
            }
        }
    }
    // Warn only for a genuinely dead slice (non-overflow fatal 4xx, or transient
    // failures that never parsed). Overflow is handled by `translate_split`, so
    // stay quiet here and let it split.
    if best_count == 0 && !overflow {
        eprintln!(
            "warn: batch of {} cues produced no translations: {}",
            cues.len(),
            last_err
        );
    }
    SliceOutcome {
        translations: best,
        overflow,
    }
}

/// Build a delimiter-tagged prompt for a subtitle batch.
///
/// Wraps each cue in a unique `<cN>…</cN>` tag and instructs the model to
/// preserve the tags exactly — this is Hy-MT2's **trained** "Delimiters" format
/// (see the model card's Delimiters template), so the model keeps a strict 1:1
/// count and placement. An earlier `#N` numbered-list format drifted badly on
/// messy ASR input (the model decorated markers as `#14>` / `#19>content`,
/// breaking alignment 0/3); the tagged format is 3/3 on the same batch.
///
/// Up to `context` preceding cues ride along as an untagged `Context:` section
/// for fluency (not translated, not parsed).
fn build_batch_prompt(cues: &[&str], context: &[&str], target: &str) -> String {
    let mut out = format!(
        "Please accurately translate the following text into {tgt}. You must \
         retain the exact same number of <cN></cN> delimiters in the \
         translation, in the same order. Strictly do not omit, escape, or \
         translate these delimiters. Translate only the text between them; do \
         not merge, split, add, or skip any.\n\n",
        tgt = target
    );
    if !context.is_empty() {
        out.push_str(
            "The lines below under 'Context' are for context only — do NOT \
             translate or wrap them in tags:\n\nContext:\n",
        );
        out.push_str(&context.join("\n"));
        out.push_str("\n\n");
    }
    // Sanitize each cue for the model only (the document keeps the original):
    // ASR often emits absurd repetition (e.g. 177× `あ` from a held vowel) that
    // sends the model into a non-terminating loop. See [`sanitize_for_model`].
    let items: Vec<String> = cues
        .iter()
        .enumerate()
        .map(|(i, c)| format!("<c{i}>{c}</c{i}>", i = i + 1, c = sanitize_for_model(c)))
        .collect();
    out.push_str(&items.join("\n"));
    out
}

/// Collapse any run of more than `CAP` identical characters down to `CAP`.
///
/// ASR artifacts — a sustained vowel transcribed as 100+ copies of one char,
/// or a stutter jam — make Hy-MT2 loop on the same token without emitting EOS
/// (a 177×`あ` cue ran the decoder to the `max_tokens` cap, ~106s, before it
/// stopped). Trimming the run keeps the cue meaningful (a drawn-out sound stays
/// drawn-out, just bounded) and the model terminates normally. Applied only to
/// the text sent to the model; the source file is written back verbatim.
fn sanitize_for_model(s: &str) -> String {
    const CAP: usize = 8;
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        let mut run = 1;
        while run < CAP && chars.peek() == Some(&c) {
            out.push(chars.next().unwrap());
            run += 1;
        }
        // Drop the rest of the run (if any).
        while chars.peek() == Some(&c) {
            chars.next();
        }
    }
    out
}

/// Parse a `<cN>…</cN>`-tagged response into up to `count` aligned
/// translations, keyed by `N`.
///
/// Scans for each `<c<N>>` opener and its matching `</c<N>>` closer; the text
/// between (outer whitespace trimmed, internal newlines preserved) is the
/// translation of cue `N`. Returns a `Vec<Option<String>>` of length `count`:
/// `Some` where cue `N`'s translation was found, `None` where the model skipped
/// or dropped it. Mapping by number (not position) means a skipped cue only
/// ever costs itself — its neighbors still land in the right place — so the
/// caller can translate what the model returned and leave the rest original.
/// That preserves the one-to-one correspondence (every cue is emitted, in
/// order; nothing merged or split) without one degenerate cue sinking its whole
/// batch. Spurious tags outside `1..=count` are ignored.
fn parse_tagged(resp: &str, count: usize) -> Vec<Option<String>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<u32, String> = BTreeMap::new();
    let mut rest = resp;
    while let Some(open) = rest.find("<c") {
        let after = &rest[open + 2..];
        let dend = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if dend == 0 {
            // `<c` not followed by digits — not one of our tags; skip past.
            rest = after;
            continue;
        }
        let Ok(n) = after[..dend].parse::<u32>() else {
            rest = after;
            continue;
        };
        let after_digits = &after[dend..];
        if !after_digits.starts_with('>') {
            rest = after_digits;
            continue;
        }
        let content = &after_digits[1..]; // after '>'
        let close = format!("</c{}>", n);
        match content.find(&close) {
            Some(end) => {
                map.entry(n).or_insert(content[..end].trim().to_string());
                rest = &content[end + close.len()..];
            }
            None => rest = content, // opener with no closer: skip it
        }
    }
    (1..=count as u32).map(|n| map.get(&n).cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tagged_perfect_alignment() {
        let resp = "<c1>你好</c1>\n<c2>世界</c2>\n<c3>再见</c3>";
        let trs = parse_tagged(resp, 3);
        assert_eq!(
            trs,
            vec![
                Some("你好".into()),
                Some("世界".into()),
                Some("再见".into())
            ]
        );
    }

    #[test]
    fn parse_tagged_preserves_multiline_cues() {
        let resp = "<c1>第一行\n第二行</c1>\n<c2>世界</c2>";
        let trs = parse_tagged(resp, 2);
        assert_eq!(
            trs,
            vec![Some("第一行\n第二行".into()), Some("世界".into())]
        );
    }

    #[test]
    fn parse_tagged_maps_by_number_not_position() {
        // Out-of-order tags still map to the right cue (keyed by N).
        let resp = "<c2>世界</c2>\n<c1>你好</c1>";
        let trs = parse_tagged(resp, 2);
        assert_eq!(trs, vec![Some("你好".into()), Some("世界".into())]);
    }

    #[test]
    fn parse_tagged_recovers_partial_when_one_skipped() {
        // The model dropped cue 2: cue 1 and 3 still map correctly, cue 2 is
        // None (kept original by the caller). One bad cue costs only itself.
        let resp = "<c1>你好</c1>\n<c3>再见</c3>";
        let trs = parse_tagged(resp, 3);
        assert_eq!(trs, vec![Some("你好".into()), None, Some("再见".into())]);
    }

    #[test]
    fn parse_tagged_ignores_spurious_extra_tag() {
        // A spurious c4 (outside 1..=count) is ignored, not a hard failure.
        let resp = "<c1>你好</c1>\n<c2>世界</c2>\n<c3>再见</c3>\n<c4>多余</c4>";
        let trs = parse_tagged(resp, 3);
        assert_eq!(
            trs,
            vec![
                Some("你好".into()),
                Some("世界".into()),
                Some("再见".into())
            ]
        );
    }

    #[test]
    fn parse_tagged_shifted_keys_yield_partial() {
        // Keys 2..=4 (none for cue 1): cue 1 is None, 2 and 3 land.
        let resp = "<c2>你好</c2>\n<c3>世界</c3>\n<c4>再见</c4>";
        let trs = parse_tagged(resp, 3);
        assert_eq!(trs, vec![None, Some("你好".into()), Some("世界".into())]);
    }

    #[test]
    fn parse_tagged_ignores_untagged_context() {
        // Un-tagged lines (a Context section, prose, anything) must not leak in.
        let resp = "Context:\nFoo\nBar\n\n<c1>你好</c1>\n<c2>世界</c2>";
        let trs = parse_tagged(resp, 2);
        assert_eq!(trs, vec![Some("你好".into()), Some("世界".into())]);
    }

    #[test]
    fn parse_tagged_handles_double_digit_tags() {
        // `</c1>` is not a substring of `</c10>`, so a 10-cue response parses
        // without cue 1 bleeding into cue 10's region.
        let items: Vec<String> = (1..=10)
            .map(|i| format!("<c{i}>t{i}</c{i}>", i = i))
            .collect();
        let resp = items.join("\n");
        let trs = parse_tagged(&resp, 10);
        assert_eq!(trs.len(), 10);
        assert_eq!(trs[0], Some("t1".into()));
        assert_eq!(trs[9], Some("t10".into()));
    }

    #[test]
    fn build_batch_prompt_wraps_each_cue_in_unique_tag() {
        let p = build_batch_prompt(&["a", "b"], &["ctx1", "ctx2"], "中文");
        // Official Delimiters instruction present.
        assert!(p.contains("retain the exact same number of <cN></cN> delimiters"));
        // Context block present and un-tagged.
        assert!(p.contains("Context:\nctx1\nctx2"));
        // Each cue wrapped in its own numbered tag, one per line.
        assert!(p.contains("<c1>a</c1>"));
        assert!(p.contains("<c2>b</c2>"));
        // Context lines are NOT tagged.
        assert!(!p.contains("<c0>"));
    }

    #[test]
    fn build_batch_prompt_omits_context_section_when_empty() {
        let p = build_batch_prompt(&["a"], &[], "中文");
        assert!(!p.contains("Context:"));
        assert!(p.contains("<c1>a</c1>"));
    }

    #[test]
    fn sanitize_collapses_long_runs_keeps_short_ones() {
        // A 177×`あ` ASR jam collapses to the cap; a normal line is untouched.
        assert_eq!(sanitize_for_model(&"あ".repeat(177)), "あ".repeat(8));
        assert_eq!(sanitize_for_model("あああ"), "あああ");
        assert_eq!(sanitize_for_model("hello world"), "hello world");
        // Runs of different chars are independent.
        assert_eq!(sanitize_for_model("あああいいい"), "あああいいい");
        assert_eq!(
            sanitize_for_model(&format!("{}{}", "あ".repeat(50), "い".repeat(50))),
            format!("{}{}", "あ".repeat(8), "い".repeat(8))
        );
        // Punctuation runs (ellipses) past the cap also collapse.
        assert_eq!(sanitize_for_model(&".".repeat(20)), ".".repeat(8));
    }

    #[test]
    fn is_context_overflow_matches_vllm_context_error() {
        // The exact body vLLM returns when the prompt exceeds the context window.
        let vllm = "HTTP 400 Bad Request: {\"error\":{\"message\":\"This model's \
            maximum context length is 4096 tokens. However, you requested 0 output \
            tokens and your prompt contains at least 4097 input tokens, for a total \
            of at least 4097 tokens. Please reduce the length of the input prompt or \
            the number of requested output tokens.\"}}";
        assert!(is_context_overflow(vllm));
        // The shorter "input prompt" phrasing alone also matches.
        assert!(is_context_overflow(
            "HTTP 400: Please reduce the length of the input prompt"
        ));
        // Other 4xx must NOT match — those aren't recoverable by splitting.
        assert!(!is_context_overflow("HTTP 404 Not Found: model not found"));
        assert!(!is_context_overflow(
            "HTTP 400: invalid sampling temperature"
        ));
    }

    #[test]
    fn truncate_never_splits_multibyte_char() {
        // '。' is 3 bytes (E3 80 82). A cut at byte 200 inside a run of them
        // must floor to the nearest boundary, not panic. Build a string whose
        // byte 200 is mid-character.
        let s = "あ".repeat(200); // each 'あ' is 3 bytes; byte 200 == char #66 boundary
        let t = truncate(&s, 200);
        assert!(t.ends_with('…'));
        // The truncated body must be valid UTF-8 (no panic) and shorter than s.
        assert!(t.len() <= 201);

        // A cut landing mid-character: byte 200 of "。" * 100 is inside char #67.
        let u = "。".repeat(100); // bytes: char ends at 3,6,...,201; 200 is mid-char #67
        let v = truncate(&u, 200);
        assert!(v.ends_with('…'));
        assert_eq!(v.len(), 3 * 66 + 3); // 66 full chars (198 bytes) + '…' (3 bytes)

        // Short string returned unchanged.
        assert_eq!(truncate("hello", 10), "hello");
        // ASCII boundary exact.
        assert_eq!(truncate("hello world", 5), "hello…");
    }
}
