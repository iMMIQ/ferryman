//! ferryman — translate a document into bilingual (original + translation)
//! output via a vLLM-served model. EPUB, SRT, VTT, ASS, LRC and TXT ship today;
//! docx / md are planned — plug a new format into `src/format/` and it just works.
//!
//! The original formatting is preserved byte-for-byte (via lol_html for EPUB;
//! cue timing/structure is preserved verbatim for subtitles); after each
//! translated block a styled sibling carrying the translation is inserted.

mod archive;
mod cache;
mod container;
mod engine;
mod format;
mod html;
mod translate;

use crate::cache::Cache;
use crate::engine::Engine;
use crate::format::{OutputMode, Strategy};
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "ferryman",
    about = "Translate a document into a bilingual side-by-side output via vLLM (EPUB, SRT, VTT, ASS, LRC, TXT)"
)]
struct Cli {
    /// Input path (format auto-detected from extension: epub, srt, vtt, ass, lrc, txt).
    #[arg(long, short = 'i')]
    input: PathBuf,

    /// Output path (bilingual output, same format as the input).
    #[arg(long, short = 'o')]
    output: PathBuf,

    /// Output mode: `bilingual` (default) keeps the original and appends the
    /// translation; `replace` writes only the translation.
    #[arg(long, value_enum, default_value_t = OutputMode::Bilingual)]
    mode: OutputMode,

    /// vLLM OpenAI-compatible endpoint (used when --serve is NOT set).
    #[arg(long, default_value = "http://localhost:8001")]
    endpoint: String,

    /// Target language (full name, e.g. 中文 / English / 日本語).
    #[arg(long, default_value = "中文")]
    target: String,

    /// Optional cap on total translated blocks (for quick testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Subtitle cues per translation request (subtitle inputs only). Batching
    /// keeps cross-cue context and orders the result strictly one-to-one; the
    /// model returns one translation per cue, no merge/split. (default: 25)
    #[arg(long, default_value_t = 25)]
    subtitle_batch_size: usize,

    /// Number of preceding cues sent as read-only context with each subtitle
    /// batch (not translated, not emitted) — keeps the translation fluent
    /// across cue boundaries. (default: 5)
    #[arg(long, default_value_t = 5)]
    subtitle_context: usize,

    /// Disable the on-disk translation cache (retranslate everything). By
    /// default completed translations are cached so re-runs skip them and a
    /// Ctrl-C'd run keeps what finished.
    #[arg(long)]
    no_cache: bool,

    /// Cache directory (default: $XDG_CACHE_HOME/ferryman or ~/.cache/ferryman).
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 180)]
    timeout: u64,

    /// Model preset: bundles the model path + the optimal vLLM serve config for
    /// this Jetson. `7b-fp8` = Hy-MT2-7B-FP8 (light/fast, the original default);
    /// `30b-fp8` = Hy-MT2-30B-A3B-FP8 (higher quality; CUDA graphs on,
    /// max-num-seqs 512, util 0.55 — measured ~1222 tok/s peak). Every flag below
    /// overrides the preset, so you can still tweak any single knob.
    #[arg(long, value_enum, default_value_t = Preset::SevenBFp8)]
    preset: Preset,

    /// Served model id (OpenAI `model` field) when NOT --serve. Defaults to the
    /// preset's model (same id the container serves).
    #[arg(long)]
    model: Option<String>,

    // --- container management (self-hosted model deployment) ---
    /// Launch & manage the vLLM container ourselves (shut it down afterwards).
    #[arg(long)]
    serve: bool,

    /// Docker image to run when --serve.
    #[arg(long, default_value = "docker.io/catdogai/lzc-aipod-vllm:agxorin-cu126-src-18f658bb3185-20260703")]
    image: String,

    /// Host directory holding the model files (mounted into the container).
    /// Defaults to the preset's model dir.
    #[arg(long)]
    host_model_dir: Option<String>,

    /// Host directory persisted as the container's JIT/compile cache. The cu126
    /// image's v1 engine JIT-compiles FlashInfer/Triton kernels on first launch
    /// (~2.5-5 min); persisting them reuses the compiled kernels on later launches.
    /// Defaults to `$MODEL_ROOT/vllm-cache` (same root as the presets).
    #[arg(long)]
    host_cache_dir: Option<String>,

    /// Model path inside the container; also the served model id.
    /// Defaults to the preset's model.
    #[arg(long)]
    serve_model: Option<String>,

    /// Container name (removed on exit).
    #[arg(long, default_value = "ferryman-vllm")]
    container_name: String,

    /// Host port to map to the container's 8000.
    #[arg(long, default_value_t = 8001)]
    host_port: u16,

    /// Quantization method, e.g. `awq_marlin`. Omit to let vLLM auto-detect (FP8).
    #[arg(long)]
    quantization: Option<String>,

    /// Compute dtype (default: float16 for 7b, auto→bf16 for 30b).
    #[arg(long)]
    vllm_dtype: Option<String>,

    /// KV cache dtype. `fp8` halves KV-cache memory and boosts decode throughput;
    /// `auto` uses the model's native dtype. (default: fp8)
    #[arg(long)]
    kv_cache_dtype: Option<String>,

    /// gpu-memory-utilization (default: 0.30 for 7b, 0.55 for 30b).
    #[arg(long)]
    gpu_memory_utilization: Option<f32>,

    /// max-model-len (default: 8192 for 7b, 4096 for 30b).
    #[arg(long)]
    max_model_len: Option<u32>,

    /// max-num-seqs, vLLM's admission cap (default: 512 for both presets).
    #[arg(long)]
    max_num_seqs: Option<u32>,

    /// Force eager mode (disable torch.compile + CUDA graphs). Both presets
    /// leave this off — graphs are faster on this Jetson. Set only to A/B test
    /// eager. (README documents this as "omit --enforce-eager" for graphs-on.)
    #[arg(long)]
    enforce_eager: bool,

    /// Max concurrent translation requests (default: 256 for 7b, 128 for 30b).
    #[arg(long)]
    concurrency: Option<usize>,

    /// Seconds to wait for the container to become healthy (cold start ~2.5-5 min).
    #[arg(long, default_value_t = 600)]
    health_timeout: u64,
}

/// Bundled model + optimal serve config per preset.
#[derive(Clone, Copy, clap::ValueEnum, PartialEq, Debug)]
pub enum Preset {
    /// Hy-MT2-7B-FP8 — light & fast, the original default.
    #[value(name = "7b-fp8")]
    SevenBFp8,
    /// Hy-MT2-30B-A3B-FP8 — higher quality; optimal serve config measured on
    /// this Jetson (CUDA graphs on, max-num-seqs 512 → ~1222 tok/s peak).
    #[value(name = "30b-fp8")]
    ThirtyBFp8,
}

struct PresetCfg {
    host_model_dir: String,
    serve_model: &'static str,
    dtype: &'static str,
    kv_cache_dtype: &'static str,
    gpu_memory_utilization: f32,
    max_model_len: u32,
    max_num_seqs: Option<u32>,
    /// false = CUDA graphs ON (omit --enforce-eager).
    enforce_eager: bool,
    concurrency: usize,
}

impl Preset {
    fn cfg(self) -> PresetCfg {
        match self {
            Preset::SevenBFp8 => PresetCfg {
                host_model_dir: format!("{}/Hy-MT2-7B-FP8", model_root()),
                serve_model: "/models/Hy-MT2-7B-FP8",
                dtype: "float16",
                kv_cache_dtype: "fp8",
                // 7B weights are only ~7 GiB; 0.30 already yields ~258k fp8 KV
                // tokens, far past what compute can keep busy.
                gpu_memory_utilization: 0.30,
                max_model_len: 8192,
                // 7B saturates on COMPUTE ~c256-512; 512 lets short blocks reach
                // the ceiling (default 256 leaves throughput on the table).
                max_num_seqs: Some(512),
                // CUDA graphs ON: measured +8% throughput ceiling (868→938 tok/s)
                // and +15% at low concurrency. Smaller win than the 30B (7B is
                // dense → fewer kernels/step → less CPU launch overhead to remove)
                // but effectively free: capture is 1.7 GiB and the 7B footprint is
                // tiny. (The old "graphs hurt 2x" note was AWQ-specific, not FP8.)
                enforce_eager: false,
                // Near the compute saturation point: ~895 tok/s @ ~11s/block.
                // (Old default 96 topped out at ~637 tok/s.)
                concurrency: 256,
            },
            Preset::ThirtyBFp8 => PresetCfg {
                host_model_dir: format!("{}/Hy-MT2-30B-A3B-FP8", model_root()),
                serve_model: "/models/Hy-MT2-30B-A3B-FP8",
                dtype: "auto",
                kv_cache_dtype: "fp8",
                // Reliable minimum: weights are 28.6G = 47% of 61G, so util must
                // be ≥~0.52 for positive KV (0.45 went negative-KV and failed).
                gpu_memory_utilization: 0.55,
                max_model_len: 4096,
                // Unlocks the throughput ceiling (default 256 caps at ~878 tok/s;
                // 512 reaches 1222). KV (~66-108k, varies) binds real paragraphs
                // ~c150-240, so concurrency 128 stays safely under that.
                max_num_seqs: Some(512),
                // CUDA graphs ON: 2.9x faster single-stream, +9% @c256, peak 1222
                // vs 878 eager. Capture uses only ~1 GiB. (The 7B-AWQ "graphs hurt"
                // finding does NOT apply to this 30B-FP8 + vLLM-main build.)
                enforce_eager: false,
                concurrency: 128,
            },
        }
    }
}

/// Default on-disk cache dir: `$XDG_CACHE_HOME/ferryman`, else
/// `$HOME/.cache/ferryman`. Avoids pulling a `dirs`-style crate for one lookup.
fn default_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("ferryman");
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache").join("ferryman")
}

/// Default model root: `$HOME/model`. Resolved at runtime so the binary never
/// bakes in a specific user's home path — the presets join a model subpath onto
/// this. If `$HOME` is unset, falls back to a relative `model/` (will then fail
/// clearly at the docker mount, which is the right place to surface it).
fn model_root() -> String {
    std::env::var("HOME")
        .map(|h| format!("{h}/model"))
        .unwrap_or_else(|_| "model".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve the preset, then let any explicit --flag override it.
    let p = cli.preset.cfg();
    let host_model_dir = cli
        .host_model_dir
        .clone()
        .unwrap_or_else(|| p.host_model_dir.clone());
    let host_cache_dir = cli
        .host_cache_dir
        .clone()
        .unwrap_or_else(|| format!("{}/vllm-cache", model_root()));
    let serve_model = cli
        .serve_model
        .clone()
        .unwrap_or_else(|| p.serve_model.to_string());
    let dtype = cli.vllm_dtype.clone().unwrap_or_else(|| p.dtype.to_string());
    let kv_cache_dtype = cli
        .kv_cache_dtype
        .clone()
        .unwrap_or_else(|| p.kv_cache_dtype.to_string());
    let gpu_memory_utilization = cli.gpu_memory_utilization.unwrap_or(p.gpu_memory_utilization);
    let max_model_len = cli.max_model_len.unwrap_or(p.max_model_len);
    let max_num_seqs = cli.max_num_seqs.or(p.max_num_seqs);
    let enforce_eager = cli.enforce_eager || p.enforce_eager;
    let concurrency = cli.concurrency.unwrap_or(p.concurrency);

    eprintln!(
        "preset: {:?} | model {} | concurrency {} | {} KV | util {} | max-model-len {} | graphs {}",
        cli.preset,
        serve_model,
        concurrency,
        kv_cache_dtype,
        gpu_memory_utilization,
        max_model_len,
        if enforce_eager { "off (eager)" } else { "on" }
    );

    // Optionally launch (and on exit tear down) the vLLM container ourselves.
    let spec = container::ServeSpec {
        image: cli.image.clone(),
        host_model_dir: host_model_dir.clone(),
        host_cache_dir: host_cache_dir.clone(),
        container_model: serve_model.clone(),
        host_port: cli.host_port,
        container_name: cli.container_name.clone(),
        quantization: cli.quantization.clone(),
        dtype: dtype.clone(),
        kv_cache_dtype: kv_cache_dtype.clone(),
        gpu_memory_utilization,
        max_model_len,
        max_num_seqs,
        enforce_eager,
        health_timeout: cli.health_timeout,
    };
    // Guard stays alive until the end of main → container removed after translation
    // (and on any error via `?`, since Drop runs on unwind/return).
    let _guard = if cli.serve {
        Some(container::ContainerGuard::launch(&spec).await?)
    } else {
        None
    };
    let endpoint = _guard
        .as_ref()
        .map(|g| g.endpoint().to_string())
        .unwrap_or_else(|| cli.endpoint.clone());
    let model = if cli.serve {
        serve_model.clone()
    } else {
        cli.model
            .clone()
            .unwrap_or_else(|| serve_model.clone())
    };

    // --- format-agnostic translation pipeline ---
    // Open the input (dispatches on extension), pull its segments, translate
    // them all in one concurrent batch, write the result back in the input's
    // own format. Adding a format never touches anything below this line.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cli.timeout))
        .build()?;
    let cache = if cli.no_cache {
        None
    } else {
        let dir = cli.cache_dir.clone().unwrap_or_else(default_cache_dir);
        Cache::open(Some(dir))
    };
    let engine = Engine::new(client, endpoint, model, cli.target.clone(), concurrency, cache);

    let mut doc = format::open(&cli.input, None)
        .with_context(|| format!("open input {}", cli.input.display()))?;
    let segments = doc.segments();
    eprintln!(
        "{}: {} block(s) -> translating into {:?}",
        doc.format_name(),
        segments.len(),
        cli.target
    );

    // The format picks Independent vs Batched via its strategy(); the CLI
    // supplies the batch parameters (size / context window) when batching is
    // requested. Formats that stay Independent ignore them.
    let strategy = match doc.strategy() {
        Strategy::Independent => Strategy::Independent,
        Strategy::Batched { .. } => Strategy::Batched {
            batch_size: cli.subtitle_batch_size,
            context: cli.subtitle_context,
        },
    };
    let out = engine.translate(&segments, strategy, cli.limit).await;
    if out.cancelled {
        eprintln!("interrupted (Ctrl-C): writing the partial output gathered so far");
    }

    doc.write(&out.translations, &cli.output, cli.mode)
        .with_context(|| format!("write output {}", cli.output.display()))?;

    eprintln!(
        "\ndone: {} block(s) translated, {} failed{} -> {}",
        out.translated,
        out.failed,
        if out.cancelled { " (partial)" } else { "" },
        cli.output.display()
    );
    Ok(())
}
