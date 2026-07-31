//! ferryman — translate documents into bilingual (original + translation)
//! output via a vLLM-served model. EPUB, DOCX, SRT, VTT, ASS, LRC, TXT and MD
//! ship today — plug a new format into `src/format/` and it just works.
//!
//! The original formatting is preserved byte-for-byte (via lol_html for EPUB;
//! surgical paragraph splice for DOCX; cue timing/structure is preserved
//! verbatim for subtitles); after each translated block a styled sibling
//! carrying the translation is inserted.
//!
//! A single file or a whole directory goes through [`batch::run_batch`]: one
//! shared concurrency pool, files opened lazily and written the moment they
//! finish — so memory tracks the files *in flight*, not the size of the input.

use anyhow::Result;
use clap::Parser;
use ferryman::batch::{collect_inputs, run_batch, BatchOpts};
use ferryman::cache::Cache;
use ferryman::container;
use ferryman::engine::{build_translation_client, Engine};
use ferryman::format::OutputMode;
use ferryman::preset::Preset;
use ferryman::settings::{
    TranslationSettings, DEFAULT_BATCH_SIZE, DEFAULT_CONTEXT_SEGMENTS,
    DEFAULT_REQUEST_TIMEOUT_SECONDS,
};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "ferryman",
    about = "Translate a document into a bilingual side-by-side output via vLLM (EPUB, DOCX, SRT, VTT, ASS, LRC, TXT, MD)"
)]
struct Cli {
    /// Input file or directory. A file is translated directly (format
    /// auto-detected from the extension). A directory is walked recursively and
    /// every supported file (epub, docx, srt, vtt, ass, lrc, txt, md) is
    /// translated; unsupported files and ferryman's own suffixed outputs are
    /// skipped. Files are opened lazily and written as they finish, so memory
    /// stays bounded by the concurrency window — for a very large EPUB/DOCX
    /// library, run it in subdirectory batches.
    #[arg(long, short = 'i')]
    input: PathBuf,

    /// Output path (single-file mode only; rejected with a directory input).
    /// If neither --output nor --in-place is given, each file is written to a
    /// sibling named `<name>.bilingual.<ext>` next to the original.
    #[arg(long, short = 'o', conflicts_with = "in_place")]
    output: Option<PathBuf>,

    /// Overwrite each input file in place (single file or directory). Mutually
    /// exclusive with --output. Each file is written to a sibling temp file
    /// first, then atomically renamed over the original, so a crash mid-write
    /// can't truncate the source.
    #[arg(long)]
    in_place: bool,

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

    /// Optional cap on total translated blocks across the whole batch (testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Segments per translation request when a format batches (subtitles, txt,
    /// md). Batching keeps cross-segment context and orders the result strictly
    /// one-to-one; the model returns one translation per segment, no merge/split.
    /// (default: 25)
    #[arg(long, default_value_t = DEFAULT_BATCH_SIZE)]
    subtitle_batch_size: usize,

    /// Number of preceding segments sent as read-only context with each batch
    /// (not translated, not emitted) — keeps the translation fluent across
    /// boundaries. (default: 5)
    #[arg(long, default_value_t = DEFAULT_CONTEXT_SEGMENTS)]
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
    #[arg(long, default_value_t = DEFAULT_REQUEST_TIMEOUT_SECONDS)]
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
    #[arg(
        long,
        default_value = "docker.io/catdogai/lzc-aipod-vllm:agxorin-cu126-src-18f658bb3185-20260703"
    )]
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
    let settings = TranslationSettings {
        batch_size: cli.subtitle_batch_size,
        context_segments: cli.subtitle_context,
        cache_enabled: !cli.no_cache,
    };

    // Resolve the preset, then let any explicit --flag override it.
    let p = cli.preset.config();
    let host_model_dir = cli
        .host_model_dir
        .clone()
        .unwrap_or_else(|| format!("{}/{}", model_root(), p.model_dir_name));
    let host_cache_dir = cli
        .host_cache_dir
        .clone()
        .unwrap_or_else(|| format!("{}/vllm-cache", model_root()));
    let serve_model = cli
        .serve_model
        .clone()
        .unwrap_or_else(|| p.serve_model.to_string());
    let dtype = cli
        .vllm_dtype
        .clone()
        .unwrap_or_else(|| p.dtype.to_string());
    let kv_cache_dtype = cli
        .kv_cache_dtype
        .clone()
        .unwrap_or_else(|| p.kv_cache_dtype.to_string());
    let gpu_memory_utilization = cli
        .gpu_memory_utilization
        .unwrap_or(p.gpu_memory_utilization);
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
        cli.model.clone().unwrap_or_else(|| serve_model.clone())
    };

    let client = build_translation_client(Duration::from_secs(cli.timeout), None)?;
    let cache = if settings.cache_enabled {
        let dir = cli.cache_dir.clone().unwrap_or_else(default_cache_dir);
        Cache::open(Some(dir))
    } else {
        None
    };
    let engine = Engine::new(
        client,
        endpoint,
        model,
        cli.target.clone(),
        concurrency,
        cache,
    );

    // --- input enumeration ---
    // A directory is walked recursively for supported files; a single file is
    // processed as-is. --output (one path) can't be paired with a directory.
    if cli.input.is_dir() && cli.output.is_some() {
        anyhow::bail!(
            "--output cannot be combined with a directory input; use --in-place, \
             or drop both to write a mode-specific suffixed sibling next to each file"
        );
    }
    let inputs = if cli.input.is_dir() {
        let files = collect_inputs(&cli.input)?;
        eprintln!(
            "input dir {}: {} supported file(s) (recursed)",
            cli.input.display(),
            files.len()
        );
        files
    } else {
        vec![cli.input.clone()]
    };

    // --- run the queue: lazy open + shared pool + progressive write ---
    // batch.rs opens files only as pool slots free up, and writes each file the
    // instant it finishes (releasing its parsed IR). Memory therefore tracks the
    // files in flight (~the concurrency window), not the whole directory.
    let opts = BatchOpts {
        mode: cli.mode,
        in_place: cli.in_place,
        output: cli.output.clone(),
        batch_size: settings.batch_size,
        context: settings.context_segments,
        limit: cli.limit,
    };
    let summary = run_batch(&engine, inputs, opts).await;

    eprintln!(
        "\nbatch: {} file(s) ok, {} failed; {} segment(s) translated, {} failed{}",
        summary.ok_files,
        summary.failed_files.len(),
        summary.translated,
        summary.failed,
        if summary.cancelled {
            " (interrupted)"
        } else {
            ""
        }
    );
    for (p, m) in &summary.failed_files {
        eprintln!("  failed: {} ({})", p.display(), m);
    }
    Ok(())
}
