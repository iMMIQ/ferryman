use serde::{Deserialize, Serialize};
use std::fmt;

/// The two Hy-MT2 deployments supported by Ferryman.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
pub enum Preset {
    #[value(name = "7b-fp8")]
    #[serde(rename = "7b-fp8")]
    SevenBFp8,
    #[value(name = "30b-fp8")]
    #[serde(rename = "30b-fp8")]
    ThirtyBFp8,
}

#[derive(Clone, Copy, Debug)]
pub struct PresetConfig {
    pub model_dir_name: &'static str,
    pub serve_model: &'static str,
    pub huggingface_repo: &'static str,
    pub huggingface_revision: &'static str,
    pub modelscope_repo: &'static str,
    pub download_bytes: u64,
    pub dtype: &'static str,
    pub kv_cache_dtype: &'static str,
    pub gpu_memory_utilization: f32,
    pub kv_cache_memory_bytes: u64,
    pub max_model_len: u32,
    pub max_num_seqs: Option<u32>,
    pub enforce_eager: bool,
    pub concurrency: usize,
}

impl Preset {
    pub fn config(self) -> PresetConfig {
        match self {
            Preset::SevenBFp8 => PresetConfig {
                model_dir_name: "Hy-MT2-7B-FP8",
                serve_model: "/models/Hy-MT2-7B-FP8",
                huggingface_repo: "tencent/Hy-MT2-7B-FP8",
                huggingface_revision: "883d09eb21d9be92058556cd0a4016d8a648c7db",
                modelscope_repo: "Tencent-Hunyuan/Hy-MT2-7B-FP8",
                download_bytes: 8_046_402_613,
                dtype: "float16",
                kv_cache_dtype: "fp8",
                gpu_memory_utilization: 0.30,
                kv_cache_memory_bytes: 8 * 1024 * 1024 * 1024,
                max_model_len: 8192,
                max_num_seqs: Some(512),
                enforce_eager: false,
                concurrency: 256,
            },
            Preset::ThirtyBFp8 => PresetConfig {
                model_dir_name: "Hy-MT2-30B-A3B-FP8",
                serve_model: "/models/Hy-MT2-30B-A3B-FP8",
                huggingface_repo: "tencent/Hy-MT2-30B-A3B-FP8",
                huggingface_revision: "b69671c83c2137c6982209715030df82f0093ee1",
                modelscope_repo: "Tencent-Hunyuan/Hy-MT2-30B-A3B-FP8",
                download_bytes: 30_593_608_342,
                dtype: "auto",
                kv_cache_dtype: "fp8",
                gpu_memory_utilization: 0.55,
                kv_cache_memory_bytes: 3 * 1024 * 1024 * 1024,
                max_model_len: 4096,
                max_num_seqs: Some(512),
                enforce_eager: false,
                concurrency: 128,
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Preset::SevenBFp8 => "7b-fp8",
            Preset::ThirtyBFp8 => "30b-fp8",
        }
    }

    pub fn api_model(self) -> &'static str {
        self.as_str()
    }

    /// Estimated per-request prompt ceiling (in characters) for client-side
    /// batch pre-splitting: roughly half the context window, because the
    /// translation output needs about as much room as the input again.
    /// CJK counts ≈1 token per char, so chars over-estimate latin text — the
    /// safe direction. Floored so even a small window still admits a cue.
    pub fn prompt_char_budget(self) -> usize {
        (self.config().max_model_len as usize / 2)
            .saturating_sub(256)
            .max(512)
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Preset;

    #[test]
    fn presets_use_fixed_kv_cache_budgets() {
        assert_eq!(
            Preset::SevenBFp8.config().kv_cache_memory_bytes,
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(
            Preset::ThirtyBFp8.config().kv_cache_memory_bytes,
            3 * 1024 * 1024 * 1024
        );
    }
}
