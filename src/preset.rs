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
    pub dtype: &'static str,
    pub kv_cache_dtype: &'static str,
    pub gpu_memory_utilization: f32,
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
                dtype: "float16",
                kv_cache_dtype: "fp8",
                gpu_memory_utilization: 0.30,
                max_model_len: 8192,
                max_num_seqs: Some(512),
                enforce_eager: false,
                concurrency: 256,
            },
            Preset::ThirtyBFp8 => PresetConfig {
                model_dir_name: "Hy-MT2-30B-A3B-FP8",
                serve_model: "/models/Hy-MT2-30B-A3B-FP8",
                dtype: "auto",
                kv_cache_dtype: "fp8",
                gpu_memory_utilization: 0.55,
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
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
