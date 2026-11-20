//! ferryman — translate documents into bilingual (original + translation) or
//! translated-only output via an OpenAI-compatible HTTP API.

use anyhow::Result;
use clap::Parser;
use ferryman::batch::{collect_inputs, run_batch_controlled, BatchOpts, BatchProgress};
use ferryman::cache::Cache;
use ferryman::engine::{build_translation_client, Engine};
use ferryman::format::{Format, OutputMode};
use ferryman::model_api::{ModelTarget, PromptAdapter};
use ferryman::settings::{
    TranslationSettings, DEFAULT_BATCH_SIZE, DEFAULT_CONTEXT_SEGMENTS,
    DEFAULT_REQUEST_TIMEOUT_SECONDS,
};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "ferryman",
    about = "Translate documents into bilingual or translated-only output via an OpenAI-compatible API (EPUB, DOCX, PDF, SRT, VTT, ASS, LRC, TXT, MD)"
)]
struct Cli {
    /// Input file or directory (walked recursively for supported formats).
    #[arg(long, short = 'i')]
    input: PathBuf,

    /// Output path (single-file mode only; rejected with a directory input).
    #[arg(long, short = 'o', conflicts_with = "in_place")]
    output: Option<PathBuf>,

    /// Overwrite each input file in place (single file or directory).
    #[arg(long)]
    in_place: bool,

    /// Output mode: `bilingual` (default) or `replace`.
    #[arg(long, value_enum, default_value_t = OutputMode::Bilingual)]
    mode: OutputMode,

    /// OpenAI-compatible API base URL.
    #[arg(long, default_value = "http://127.0.0.1:8088/v1")]
    endpoint: String,

    /// Served model id (OpenAI `model` field).
    #[arg(long, default_value = "index-translate-2b")]
    model: String,

    /// Prompt adapter (`generic` or `index-translate`).
    #[arg(long, value_enum, default_value_t = PromptAdapter::Generic)]
    adapter: PromptAdapter,

    /// Optional API key (falls back to FERRYMAN_API_KEY).
    #[arg(long)]
    api_key: Option<String>,

    /// Target language (full name, e.g. 中文 / English / 日本語).
    #[arg(long, default_value = "中文")]
    target: String,

    /// Optional cap on total translated blocks across the whole batch (testing).
    #[arg(long)]
    limit: Option<usize>,

    /// Segments per translation request when a format batches.
    #[arg(long, default_value_t = DEFAULT_BATCH_SIZE)]
    subtitle_batch_size: usize,

    /// Number of preceding segments sent as read-only context with each batch.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_SEGMENTS)]
    subtitle_context: usize,

    /// Disable the on-disk translation cache (retranslate everything).
    #[arg(long)]
    no_cache: bool,

    /// Cache directory (default: $XDG_CACHE_HOME/ferryman or ~/.cache/ferryman).
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_REQUEST_TIMEOUT_SECONDS)]
    timeout: u64,

    /// Model context window in tokens.
    #[arg(long, default_value_t = 8192)]
    context_length: u32,

    /// Max output tokens per request.
    #[arg(long, default_value_t = 4096)]
    max_output_tokens: u32,

    /// Max concurrent translation requests.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
}

fn default_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("ferryman");
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache").join("ferryman")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .without_time()
        .init();
    let cli = Cli::parse();
    let settings = TranslationSettings {
        batch_size: cli.subtitle_batch_size,
        context_segments: cli.subtitle_context,
        cache_enabled: !cli.no_cache,
    };

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
        Format::from_path(&cli.input)?;
        vec![cli.input.clone()]
    };

    let mut target = ModelTarget::from_endpoint(&cli.endpoint, &cli.model, cli.concurrency);
    target.adapter = cli.adapter;
    target.context_length = cli.context_length;
    target.max_output_tokens = cli.max_output_tokens;
    target.validate()?;

    let api_key = cli
        .api_key
        .or_else(|| std::env::var("FERRYMAN_API_KEY").ok())
        .filter(|key| !key.is_empty());
    let client = build_translation_client(Duration::from_secs(cli.timeout), None)?;
    let cache = if settings.cache_enabled {
        let dir = cli.cache_dir.clone().unwrap_or_else(default_cache_dir);
        Cache::open(Some(dir))
    } else {
        None
    };
    let prompt_char_budget = target.prompt_char_budget();
    let engine = Engine::new(
        client,
        target.base_url.clone(),
        target.model_id.clone(),
        cli.target.clone(),
        cli.concurrency,
        cache,
    )
    .with_model_target(target)
    .with_api_key(api_key);

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::with_template("  [{bar:20.cyan/blue}] {pos}/{len} ({elapsed}) {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb.set_message("translating");

    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let bar = pb.clone();
    let summary = run_batch_controlled(
        &engine,
        inputs,
        BatchOpts {
            mode: cli.mode,
            in_place: cli.in_place,
            output: cli.output.clone(),
            batch_size: settings.batch_size,
            context: settings.context_segments,
            limit: cli.limit,
            prompt_char_budget,
        },
        cancel,
        Arc::new(move |progress: BatchProgress| {
            bar.set_length(progress.total as u64);
            bar.set_position(progress.completed as u64);
        }),
    )
    .await;
    signal_task.abort();
    pb.finish_and_clear();

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
