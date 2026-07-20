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
