//! ferryman — translate documents into bilingual (original + translation)
//! output via a orinfer-served model. EPUB, DOCX, PDF, SRT, VTT, ASS, LRC, TXT
//! and MD ship today — plug a new format into `src/format/` and it just works.
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
    about = "Translate a document into a bilingual side-by-side output via orinfer (EPUB, DOCX, PDF, SRT, VTT, ASS, LRC, TXT, MD)"
)]
struct Cli {
    /// Input file or directory. A file is translated directly (format
    /// auto-detected from the extension). A directory is walked recursively and
    /// every supported file (epub, docx, pdf, srt, vtt, ass, lrc, txt, md) is
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

    /// orinfer OpenAI-compatible endpoint (used when --serve is NOT set).
    #[arg(long, default_value = "http://127.0.0.1:8088")]
    endpoint: String,

    /// Target language (full name, e.g. 中文 / English / 日本語).
    #[arg(long, default_value = "中文")]
    target: String,

    /// Optional cap on total translated blocks across the whole batch (testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Segments per translation request when a format batches (subtitles, txt,
    /// md, pdf). Batching keeps cross-segment context and orders the result strictly
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

    /// Quantized Index translation model.
    #[arg(long, value_enum, default_value_t = Preset::Index2B)]
    preset: Preset,

    /// Served model id (OpenAI `model` field) when NOT --serve. Defaults to the
    /// preset's model (same id the container serves).
    #[arg(long)]
    model: Option<String>,

    // --- container management (self-hosted model deployment) ---
    /// Launch & manage the orinfer container ourselves (shut it down afterwards).
    #[arg(long)]
    serve: bool,

    /// Docker image to run when --serve.
    #[arg(long, default_value = "ferryman-agent:standalone")]
    image: String,

    /// Host directory holding the model files (mounted into the container).
    /// Defaults to the preset's model dir.
    #[arg(long)]
    host_model_dir: Option<String>,

    /// Host directory for runtime caches (defaults to $MODEL_ROOT/orinfer-cache).
    #[arg(long)]
    host_cache_dir: Option<String>,

    /// Model path inside the container.
    /// Defaults to the preset's model.
    #[arg(long)]
    serve_model: Option<String>,

    /// Container name (removed on exit).
    #[arg(long, default_value = "ferryman-orinfer")]
    container_name: String,

    /// Host port to map to the container's 8088.
    #[arg(long, default_value_t = 8088)]
    host_port: u16,

    /// Max concurrent translation requests (default: 32).
    #[arg(long)]
    concurrency: Option<usize>,

    /// Seconds to wait for the container to become healthy.
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
        .unwrap_or_else(|| format!("{}/orinfer-cache", model_root()));
    let serve_model = cli
        .serve_model
        .clone()
        .unwrap_or_else(|| format!("/models/{}", p.model_dir_name));
    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| cli.preset.api_model().to_string());
    let concurrency = cli.concurrency.unwrap_or(p.concurrency);
    let spec = container::ServeSpec {
        image: cli.image.clone(),
        host_model_dir,
        host_cache_dir,
        container_model: serve_model,
        model: model.clone(),
        host_port: cli.host_port,
        container_name: cli.container_name.clone(),
        concurrency,
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
        prompt_char_budget: cli.preset.prompt_char_budget(),
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
    anyhow::ensure!(!summary.cancelled, "translation interrupted");
    anyhow::ensure!(
        summary.failed_files.is_empty() && summary.failed == 0,
        "translation incomplete: {} files and {} segments failed",
        summary.failed_files.len(),
        summary.failed
    );
    Ok(())
}
