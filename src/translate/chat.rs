//! Chat transport and completion validation shared by HTTP and JSONL.
use crate::http_client::{is_transient_status, parse_retry_after};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub(crate) enum ChatClient<'a> {
    Http(&'a reqwest::Client),
    Compatible(&'a reqwest::Client, &'a crate::model_api::ModelTarget),
    Jsonl(&'a crate::jsonl::JsonlSender),
}

#[derive(Serialize)]
pub(super) struct ChatReq<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    top_p: f32,
    top_k: i32,
    repetition_penalty: f32,
    enable_thinking: bool,
    // Orinfer derives the output budget from the remaining context (up to 8192 tokens).
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
    pub(super) message: RespMsg,
    pub(super) finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct RespMsg {
    pub(super) content: Option<String>,
}

impl<'a> ChatReq<'a> {
    pub(super) fn translation(model: &'a str, prompt: String) -> Self {
        Self {
            model,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
            temperature: 0.0,
            top_p: 1.0,
            top_k: 0,
            repetition_penalty: 1.0,
            enable_thinking: false,
            max_tokens: None,
        }
    }
}

pub(super) enum ChatAttempt {
    Success(String),
    Transient {
        error: String,
        retry_after: Option<Duration>,
    },
    Fatal {
        error: String,
        overflow: bool,
    },
}

impl ChatAttempt {
    fn transient(error: impl ToString) -> Self {
        Self::Transient {
            error: error.to_string(),
            retry_after: None,
        }
    }
}

impl ChatClient<'_> {
    pub(super) fn single_prompt(&self, text: &str, target: &str) -> String {
        if matches!(self, Self::Compatible(_, model) if model.adapter == crate::model_api::PromptAdapter::IndexTranslate)
        {
            // Official Index-Translate model card, source=auto; no system message.
            // https://huggingface.co/IndexTeam/Index-Translate-2B#inference
            format!("请将以下文本翻译为{target}，直接输出翻译结果，不要进行任何解释。\n\n{text}")
        } else {
            format!("Translate the following text into {target}. Note that you should only output the translated result without any additional explanation:\n\n{text}")
        }
    }

    pub(super) fn batch_prompt(&self, cues: &[&str], context: &[&str], target: &str) -> String {
        if matches!(self, Self::Compatible(_, model) if model.adapter == crate::model_api::PromptAdapter::IndexTranslate)
        {
            let source: Vec<_> = cues
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    let id = index + 1;
                    format!("<c{id}>{}</c{id}>", super::sanitize_for_model(text))
                })
                .collect();
            // Canonical instTrans template, with Ferryman's alignment constraints.
            let mut prompt = format!("请将以下文本翻译成{target}，并且严格遵循所有约束要求。\n\n〖源文〗\n{}\n\n〖约束要求〗\n1. 〖硬性要求〗完整保留所有 <cN></cN> 标签的数量、编号和顺序，只翻译标签内的正文，不合并、拆分或遗漏条目。", source.join("\n"));
            if !context.is_empty() {
                prompt.push_str(&format!(
                    "\n2. 〖注意〗以下内容仅供理解上下文，不翻译也不输出：\n{}",
                    context.join("\n")
                ));
            }
            prompt.push_str("\n\n只输出译文，不要有任何额外说明。");
            prompt
        } else {
            super::build_batch_prompt(cues, context, target)
        }
    }

    pub(super) async fn post(&self, url: &str, body: &ChatReq<'_>) -> ChatAttempt {
        match self {
            Self::Http(client) | Self::Compatible(client, _) => {
                let mut value = serde_json::to_value(body).expect("serialize chat request");
                let request_url;
                let url = if let Self::Compatible(_, target) = self {
                    value["max_tokens"] = target.max_output_tokens.into();
                    let object = value.as_object_mut().expect("chat object");
                    for key in ["top_k", "repetition_penalty", "enable_thinking", "top_p"] {
                        object.remove(key);
                    }
                    if target.adapter == crate::model_api::PromptAdapter::Generic {
                        value["messages"].as_array_mut().expect("messages").insert(0, serde_json::json!({
                            "role":"system", "content":"You are a professional translator. Follow the user's translation instructions, preserve the requested output structure, and output only the requested translation."
                        }));
                    } else {
                        value["chat_template_kwargs"] =
                            serde_json::json!({"enable_thinking":false});
                    }
                    request_url = format!("{}/chat/completions", target.base_url);
                    &request_url
                } else {
                    url
                };
                let response = match client.post(url).json(&value).send().await {
                    Ok(response) => response,
                    Err(error) => return ChatAttempt::transient(format!("request: {error}")),
                };
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| parse_retry_after(value, std::time::SystemTime::now()));
                let text = match response.text().await {
                    Ok(text) => text,
                    Err(error) => return ChatAttempt::transient(format!("read response: {error}")),
                };
                if let Some(outcome) = classify_status(status, &text, retry_after) {
                    return outcome;
                }
                match serde_json::from_str::<Value>(&text) {
                    Ok(value) => decode(value),
                    Err(error) => ChatAttempt::transient(format!(
                        "parse json: {error} | {}",
                        truncate(&text, 200)
                    )),
                }
            }
            Self::Jsonl(sender) => {
                let reply = match sender
                    .chat(serde_json::to_value(body).expect("serialize chat request"))
                    .await
                {
                    Ok(reply) => reply,
                    Err(error) => return ChatAttempt::transient(error),
                };
                if let Some(error) = reply.error {
                    let message = format!("{}: {}", error.code, error.message);
                    return if is_context_overflow(&message) {
                        ChatAttempt::Fatal {
                            error: message,
                            overflow: true,
                        }
                    } else {
                        ChatAttempt::transient(message)
                    };
                }
                let Some(response) = reply.response else {
                    return ChatAttempt::transient("missing batch response");
                };
                let status = match reqwest::StatusCode::from_u16(response.status_code) {
                    Ok(status) => status,
                    Err(error) => return ChatAttempt::transient(error),
                };
                if let Some(outcome) = classify_status(status, &response.body, None) {
                    return outcome;
                }
                decode(response.body)
            }
        }
    }
}

fn classify_status(
    status: reqwest::StatusCode,
    body: impl std::fmt::Display,
    retry_after: Option<Duration>,
) -> Option<ChatAttempt> {
    if status.is_success() {
        return None;
    }
    let error = format!("HTTP {status}: {}", truncate(&body.to_string(), 300));
    Some(
        if status.is_client_error() && !is_transient_status(status) {
            ChatAttempt::Fatal {
                overflow: is_context_overflow(&error),
                error,
            }
        } else {
            ChatAttempt::Transient { error, retry_after }
        },
    )
}

fn decode(value: Value) -> ChatAttempt {
    match serde_json::from_value::<ChatResp>(value) {
        Ok(parsed) => match parsed.choices.into_iter().next() {
            Some(choice) => validate_choice(choice),
            None => ChatAttempt::transient("empty choices"),
        },
        Err(error) => ChatAttempt::transient(format!("parse json: {error}")),
    }
}

fn validate_choice(choice: Choice) -> ChatAttempt {
    match choice.finish_reason.as_deref() {
        Some("length") => {
            return ChatAttempt::Fatal {
                error: "model output was truncated".into(),
                overflow: true,
            }
        }
        // Some compatible servers omit finish_reason. Still reject empty text.
        None | Some("stop") => {}
        Some(reason) => {
            return ChatAttempt::Fatal {
                error: format!("model did not complete translation: {reason}"),
                overflow: false,
            }
        }
    }
    match choice.message.content {
        Some(content) if !content.trim().is_empty() => ChatAttempt::Success(content),
        _ => ChatAttempt::Transient {
            error: "empty translation".into(),
            retry_after: None,
        },
    }
}

/// Did a fatal 4xx come from the prompt not fitting the model's context window?
/// vLLM's body reads "This model's maximum context length is N tokens … reduce
/// the length of the input prompt"; other compatible servers say "context
/// window" or "token limit". Such a batch is recoverable by splitting; other
/// 4xx (bad model id, malformed request) are not. Matched case-insensitively
/// against a small pattern set rather than one exact phrasing, so a server
/// wording change doesn't silently turn a recoverable overflow into a
/// permanent failure.
fn is_context_overflow(err: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "context length",
        "context_length_exceeded",
        "input prompt",
        "context window",
        "token limit",
        "maximum context",
    ];
    let lower = err.to_ascii_lowercase();
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty_truncated_and_non_text_completions() {
        let classify = |reason: Option<&str>, content: Option<&str>| {
            validate_choice(Choice {
                finish_reason: reason.map(str::to_owned),
                message: RespMsg {
                    content: content.map(str::to_owned),
                },
            })
        };
        assert!(matches!(
            classify(Some("length"), Some("partial")),
            ChatAttempt::Fatal { overflow: true, .. }
        ));
        for content in [None, Some(""), Some(" \n ")] {
            assert!(matches!(
                classify(Some("stop"), content),
                ChatAttempt::Transient { .. }
            ));
        }
        assert!(matches!(
            classify(Some("content_filter"), Some("partial")),
            ChatAttempt::Fatal {
                overflow: false,
                ..
            }
        ));
        assert!(matches!(
            classify(Some("stop"), Some("complete")),
            ChatAttempt::Success(_)
        ));
        assert!(matches!(
            classify(None, Some("legacy server")),
            ChatAttempt::Success(_)
        ));
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
    #[test]
    fn http_status_policy_distinguishes_retries_from_bad_requests() {
        use reqwest::StatusCode;
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(matches!(
                classify_status(status, "temporary failure", None),
                Some(ChatAttempt::Transient { .. })
            ));
        }
        assert!(matches!(
            classify_status(StatusCode::BAD_REQUEST, "context window exceeded", None),
            Some(ChatAttempt::Fatal { overflow: true, .. })
        ));
        assert!(matches!(
            classify_status(StatusCode::NOT_FOUND, "model not found", None),
            Some(ChatAttempt::Fatal {
                overflow: false,
                ..
            })
        ));
        assert!(classify_status(StatusCode::OK, "", None).is_none());
    }
}
