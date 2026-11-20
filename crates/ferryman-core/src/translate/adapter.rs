//! Strategy abstraction for model-specific prompt formatting and OpenAI-compatible
//! chat request serialization.

use crate::model_api::{ModelTarget, PromptAdapter};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct Message<'a> {
    pub(super) role: &'a str,
    pub(super) content: String,
}

#[derive(Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

/// Typed request payload for OpenAI-compatible chat endpoints.
#[derive(Serialize)]
pub(super) struct CompatibleChatReq<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
}

/// Strategy encapsulating prompt construction and payload formatting for a model adapter.
pub(super) trait PromptStrategy: Send + Sync {
    fn single_prompt(&self, text: &str, target: &str) -> String;
    fn batch_prompt(&self, cues: &[&str], context: &[&str], target: &str) -> String;
    fn build_request<'a>(
        &self,
        model: &'a str,
        prompt: String,
        max_output_tokens: u32,
    ) -> CompatibleChatReq<'a>;
}

struct GenericAdapter;
struct IndexTranslateAdapter;

const GENERIC_SYSTEM_PROMPT: &str = "You are a professional translator. Follow the user's \
    translation instructions, preserve the requested output structure, and output only the \
    requested translation.";

impl PromptStrategy for GenericAdapter {
    fn single_prompt(&self, text: &str, target: &str) -> String {
        format!(
            "Translate the following text into {target}. Note that you should only output the \
             translated result without any additional explanation:\n\n{text}"
        )
    }

    fn batch_prompt(&self, cues: &[&str], context: &[&str], target: &str) -> String {
        super::build_batch_prompt(cues, context, target)
    }

    fn build_request<'a>(
        &self,
        model: &'a str,
        prompt: String,
        max_output_tokens: u32,
    ) -> CompatibleChatReq<'a> {
        CompatibleChatReq {
            model,
            messages: vec![
                Message {
                    role: "system",
                    content: GENERIC_SYSTEM_PROMPT.to_string(),
                },
                Message {
                    role: "user",
                    content: prompt,
                },
            ],
            temperature: 0.0,
            max_tokens: max_output_tokens,
            chat_template_kwargs: None,
        }
    }
}

impl PromptStrategy for IndexTranslateAdapter {
    fn single_prompt(&self, text: &str, target: &str) -> String {
        // Official Index-Translate model card, source=auto; no system message.
        // https://huggingface.co/IndexTeam/Index-Translate-2B#inference
        format!("请将以下文本翻译为{target}，直接输出翻译结果，不要进行任何解释。\n\n{text}")
    }

    fn batch_prompt(&self, cues: &[&str], context: &[&str], target: &str) -> String {
        let source: Vec<_> = cues
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let id = index + 1;
                format!("<c{id}>{}</c{id}>", super::sanitize_for_model(text))
            })
            .collect();
        // Canonical instTrans template, with Ferryman's alignment constraints.
        let mut prompt = format!(
            "请将以下文本翻译成{target}，并且严格遵循所有约束要求。\n\n〖源文〗\n{}\n\n\
             〖约束要求〗\n1. 〖硬性要求〗完整保留所有 <cN></cN> 标签的数量、编号和顺序，\
             只翻译标签内的正文，不合并、拆分或遗漏条目。",
            source.join("\n")
        );
        if !context.is_empty() {
            prompt.push_str(&format!(
                "\n2. 〖注意〗以下内容仅供理解上下文，不翻译也不输出：\n{}",
                context.join("\n")
            ));
        }
        prompt.push_str("\n\n只输出译文，不要有任何额外说明。");
        prompt
    }

    fn build_request<'a>(
        &self,
        model: &'a str,
        prompt: String,
        max_output_tokens: u32,
    ) -> CompatibleChatReq<'a> {
        CompatibleChatReq {
            model,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
            temperature: 0.0,
            max_tokens: max_output_tokens,
            chat_template_kwargs: Some(ChatTemplateKwargs {
                enable_thinking: false,
            }),
        }
    }
}

pub(super) fn strategy_for(adapter: PromptAdapter) -> &'static dyn PromptStrategy {
    match adapter {
        PromptAdapter::Generic => &GenericAdapter,
        PromptAdapter::IndexTranslate => &IndexTranslateAdapter,
    }
}

pub(super) fn build_compatible_request<'a>(
    target: &'a ModelTarget,
    prompt: String,
) -> CompatibleChatReq<'a> {
    strategy_for(target.adapter).build_request(&target.model_id, prompt, target.max_output_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_adapter_includes_system_message_and_omits_template_kwargs() {
        let target = ModelTarget {
            id: "custom:1".into(),
            name: "Generic".into(),
            base_url: "http://example.com/v1".into(),
            model_id: "gpt-test".into(),
            adapter: PromptAdapter::Generic,
            context_length: 4096,
            max_output_tokens: 1024,
            concurrency: 2,
        };
        let req = build_compatible_request(&target, "hello".into());
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["max_tokens"], 1024);
        assert!(json.get("chat_template_kwargs").is_none());
        assert!(json.get("top_k").is_none());
    }

    #[test]
    fn index_adapter_disables_thinking_without_system_message() {
        let target = ModelTarget {
            id: "custom:2".into(),
            name: "Index".into(),
            base_url: "http://example.com/v1".into(),
            model_id: "index-translate-2b".into(),
            adapter: PromptAdapter::IndexTranslate,
            context_length: 4096,
            max_output_tokens: 768,
            concurrency: 4,
        };
        let req = build_compatible_request(&target, "hello".into());
        let json = serde_json::to_value(req).unwrap();
        assert_eq!(json["messages"].as_array().unwrap().len(), 1);
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["chat_template_kwargs"]["enable_thinking"], false);
        assert!(json.get("top_k").is_none());
    }
}
