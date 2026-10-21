use serde::{Deserialize, Serialize};
use std::fmt;

/// Index translation models quantized for orinfer 0.2.0 on SM87.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize, ts_rs::TS,
)]
pub enum Preset {
    #[value(name = "index-translate-2b")]
    #[serde(rename = "index-translate-2b")]
    #[serde(alias = "7b-fp8")]
    Index2B,
    #[value(name = "index-translate-9b")]
    #[serde(rename = "index-translate-9b")]
    Index9B,
    #[value(name = "index-translate-35b")]
    #[serde(rename = "index-translate-35b")]
    #[serde(alias = "30b-fp8")]
    Index35B,
}

#[derive(Clone, Copy, Debug)]
pub struct PresetConfig {
    pub model_dir_name: &'static str,
    pub huggingface_repo: &'static str,
    pub huggingface_revision: &'static str,
    pub modelscope_repo: &'static str,
    pub download_bytes: u64,
    pub execution_package: &'static str,
    pub benchmark_shard: &'static str,
    pub max_model_len: u32,
    pub concurrency: usize,
}

impl Preset {
    pub const ALL: [Self; 3] = [Self::Index2B, Self::Index9B, Self::Index35B];

    pub fn config(self) -> PresetConfig {
        let (name, revision, bytes, digest, shard) = match self {
            Self::Index2B => (
                "Index-Translate-2B-Orinfer-E8P-Q4A8",
                "bf66bfdf94f23dbb2875bfb2759a62728de7b58b",
                1_295_590_980,
                "f5a1375161070837e64b9346394b040070ca3a98c133bb15b754077cbc0bf4ff",
                "cache/weights/model.safetensors",
            ),
            Self::Index9B => (
                "Index-Translate-9B-Orinfer-E8P-Q4A8",
                "ac90447ba241715774f90742928abcd05875c4f5",
                5_787_251_225,
                "821345ac103f4aa09ba98d2a09bd8d489d560f1a5ea09f73b396e1f102431efa",
                "cache/weights/model-00001-of-00004.safetensors",
            ),
            // Both hub repositories use the prepared Orinfer 0.2.0 model layout.
            Self::Index35B => (
                "Index-Translate-35B-A3B-preview-Orinfer-E8P-Q4A8",
                "main",
                18_936_482_464,
                "9007d7466049d3fb3b52e4864e57ff259677d1bd65a45a4826d814cfd1cd5521",
                "cache/weights/model-00001-of-00015.safetensors",
            ),
        };
        let (huggingface_repo, modelscope_repo) = match self {
            Self::Index2B => (
                "immiq/Index-Translate-2B-Orinfer-E8P-Q4A8",
                "iammiq/Index-Translate-2B-Orinfer-E8P-Q4A8",
            ),
            Self::Index9B => (
                "immiq/Index-Translate-9B-Orinfer-E8P-Q4A8",
                "iammiq/Index-Translate-9B-Orinfer-E8P-Q4A8",
            ),
            Self::Index35B => (
                "immiq/Index-Translate-35B-A3B-preview-Orinfer-E8P-Q4A8",
                "iammiq/Index-Translate-35B-A3B-preview-Orinfer-E8P-Q4A8",
            ),
        };
        PresetConfig {
            model_dir_name: name,
            huggingface_repo,
            huggingface_revision: revision,
            modelscope_repo,
            download_bytes: bytes,
            execution_package: digest,
            benchmark_shard: shard,
            max_model_len: 8192,
            concurrency: 32,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Index2B => "index-translate-2b",
            Self::Index9B => "index-translate-9b",
            Self::Index35B => "index-translate-35b",
        }
    }

    pub fn api_model(self) -> &'static str {
        self.as_str()
    }

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
