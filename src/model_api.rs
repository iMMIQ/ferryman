//! OpenAI-compatible endpoint normalization and versioned translation adapters.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum PromptAdapter {
    #[default]
    Generic,
    IndexTranslate,
}

#[derive(Clone, Debug, Deserialize, Serialize, ts_rs::TS)]
pub struct ModelTarget {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    pub adapter: PromptAdapter,
    pub context_length: u32,
    pub max_output_tokens: u32,
    pub concurrency: usize,
}

impl ModelTarget {
    pub fn validate(&mut self) -> Result<()> {
        self.base_url = normalize_base_url(&self.base_url)?;
        self.model_id = self.model_id.trim().to_string();
        self.name = self.name.trim().to_string();
        if self.model_id.is_empty() || self.model_id.len() > 512 {
            bail!("请填写有效的模型 ID");
        }
        if self.name.is_empty() || self.name.len() > 512 {
            bail!("请填写模型名称");
        }
        if !(512..=2_000_000).contains(&self.context_length)
            || self.max_output_tokens == 0
            || self.max_output_tokens > self.context_length / 2
        {
            bail!("上下文至少为 512 tokens，输出上限须大于 0 且不超过上下文的一半");
        }
        if !(1..=32).contains(&self.concurrency) {
            bail!("并发数须在 1 到 32 之间");
        }
        Ok(())
    }

    pub fn prompt_char_budget(&self) -> usize {
        // Conservative for mixed scripts: reserve output, framing and batch tags.
        (self.context_length.saturating_sub(self.max_output_tokens) as usize / 2)
            .saturating_sub(256)
            .max(128)
    }

    pub fn cache_identity(&self) -> String {
        format!(
            "chat-adapter-v1:{:?}:{}:{}",
            self.adapter, self.context_length, self.max_output_tokens
        )
    }
}

pub fn normalize_base_url(value: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(value.trim())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("API 地址须为 HTTP(S) URL，不能包含账号、密码、查询参数或片段");
    }
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/chat/completions").unwrap_or(path);
    let path = if path.is_empty() { "/v1" } else { path };
    let path = path.to_string();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn chat_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if endpoint.ends_with("/v1") {
        format!("{endpoint}/chat/completions")
    } else {
        format!("{endpoint}/v1/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_preserve_provider_paths_without_duplicate_versions() {
        assert_eq!(
            normalize_base_url("http://localhost:18156").unwrap(),
            "http://localhost:18156/v1"
        );
        assert_eq!(
            normalize_base_url("https://example.com/api/v1/chat/completions/").unwrap(),
            "https://example.com/api/v1"
        );
        assert_eq!(
            chat_url("http://localhost:18156/v1"),
            "http://localhost:18156/v1/chat/completions"
        );
        assert!(normalize_base_url("https://user:secret@example.com/v1").is_err());
        assert!(normalize_base_url("file:///etc/passwd").is_err());
    }
}
