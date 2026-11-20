# CLI Guide

The `ferryman` command-line binary translates a single document or an entire directory tree into bilingual or translation-only output through any OpenAI-compatible Chat Completions API.

## Build

Requires the Rust toolchain pinned in [`rust-toolchain.toml`](../rust-toolchain.toml):

```bash
cargo build --release --bin ferryman
```

The compiled binary is written to `./target/release/ferryman`.

## Examples

```bash
# 1. Translate a single EPUB to bilingual Chinese via an external OpenAI-compatible API
export FERRYMAN_API_KEY="sk-..."
./target/release/ferryman \
  -i book.epub -o book.bilingual.epub \
  --endpoint https://api.openai.com/v1 \
  --model gpt-4o-mini \
  --target 中文

# 2. Translate a directory recursively in translation-only mode (writes *.translated.<ext> siblings)
./target/release/ferryman \
  -i ./library --mode replace --target English \
  --endpoint http://127.0.0.1:8088/v1 \
  --model my-chat-model

# 3. Overwrite files in place with the Index-Translate prompt adapter
./target/release/ferryman \
  -i ./subtitles --in-place --target 中文 \
  --endpoint http://127.0.0.1:8088/v1 \
  --model index-translate-2b \
  --adapter index-translate \
  --concurrency 16

# 4. Smoke-test the first 20 segments without touching the cache
./target/release/ferryman \
  -i draft.pdf -o sample.pdf \
  --limit 20 --no-cache
```

## Options Reference

Run `ferryman --help` for the built-in summary. Every flag below matches [`crates/ferryman-cli/src/main.rs`](../crates/ferryman-cli/src/main.rs).

### Input & Output

| Flag | Default | Description |
| --- | --- | --- |
| `-i`, `--input <PATH>` | *(required)* | Input file or directory. Directories are walked recursively for supported formats. |
| `-o`, `--output <PATH>` | *(none)* | Explicit output path for single-file mode. Rejected with directory input or `--in-place`. |
| `--in-place` | `false` | Atomically overwrite each input file in place (single file or directory). |
| `--mode <MODE>` | `bilingual` | Output mode: `bilingual` (keep original + translation) or `replace` (translation only). |
| `--target <LANG>` | `中文` | Free-form target language name passed into prompts (e.g., `中文`, `English`, `日本語`). |
| `--limit <N>` | *(unlimited)* | Global cap on total segments translated across the run (useful for quick tests). |

### Model & API Connection

| Flag | Default | Description |
| --- | --- | --- |
| `--endpoint <URL>` | `http://127.0.0.1:8088/v1` | OpenAI-compatible API base URL. Accepts bare host, `/v1`, provider prefix, or full `/chat/completions` suffix. |
| `--model <ID>` | `index-translate-2b` | Model identifier sent in the OpenAI `model` request field. |
| `--adapter <ADAPTER>` | `generic` | Prompt formatting strategy: `generic` or `index-translate`. |
| `--api-key <KEY>` | `$FERRYMAN_API_KEY` | Bearer API key. Falls back to the `FERRYMAN_API_KEY` environment variable if omitted. |
| `--context-length <TOKENS>` | `8192` | Served context window in tokens (valid range: `512`–`2000000`). Used to size batched prompts. |
| `--max-output-tokens <TOKENS>` | `4096` | Max output tokens per request (`> 0` and `<= context_length / 2`). |
| `--concurrency <N>` | `32` | Maximum concurrent translation requests shared across all files in the batch. |
| `--timeout <SECONDS>` | `180` | Per-HTTP-request timeout in seconds. |

### Batching & Caching

| Flag | Default | Description |
| --- | --- | --- |
| `--subtitle-batch-size <N>` | `25` | Segments per request for batched formats (Markdown, TXT, SRT, VTT, ASS/SSA, LRC). |
| `--subtitle-context <N>` | `5` | Read-only preceding segments sent as context with each batch. |
| `--no-cache` | `false` | Disable the on-disk translation cache and retranslate every segment. |
| `--cache-dir <PATH>` | `$XDG_CACHE_HOME/ferryman` or `~/.cache/ferryman` | Custom directory for the sharded translation cache. |

## Environment Variables

| Variable | Purpose |
| --- | --- |
| `FERRYMAN_API_KEY` | Bearer token fallback when `--api-key` is not supplied. |
| `XDG_CACHE_HOME` | Base cache directory used when `--cache-dir` is omitted (`$XDG_CACHE_HOME/ferryman`). |
| `FERRYMAN_PDF_FONT` | Primary CJK OpenType font path for PDF rendering (see [docs/formats.md](formats.md#pdf)). |
| `FERRYMAN_PDF_FALLBACK_FONTS` | OS path-list of additional fallback fonts for PDF rendering. |
| `FERRYMAN_PDF_DEBUG` | Set to any value to emit verbose PDF geometry extraction logs. |
| `RUST_LOG` | Standard `tracing` log filter (CLI defaults to `warn`). |

## Prompt Adapters

- **`generic`** *(default)*: sends a translator `system` message and a `user` message with `temperature: 0.0`. Compatible with standard OpenAI Chat Completions providers.
- **`index-translate`**: sends a single `user` message using the official Index-Translate Chinese prompt (and `instTrans` constraints for batched segments) with `temperature: 0.0` and `chat_template_kwargs: { "enable_thinking": false }`.

## Output Naming & Directory Traversal

- **Explicit output (`-o`)**: valid only when `--input` is a single file.
- **Default sibling suffix** (when neither `-o` nor `--in-place` is given): writes `<stem>.bilingual.<ext>` for `--mode bilingual` or `<stem>.translated.<ext>` for `--mode replace` next to each source file.
- **In-place (`--in-place`)**: writes to a temporary sibling file (`<name>.ferryman-tmp`) and atomically renames it over the source once complete.
- **Recursive directory mode**: walks subdirectories in deterministic sorted order, skips symbolic links to avoid escapes and cycles, and automatically ignores already-generated `*.bilingual.<ext>` and `*.translated.<ext>` files.

## Resumability, Retries & Exit Codes

- **Content-addressed cache**: each successful segment translation is stored atomically under `<cache-dir>/<2-hex-shard>/<hash>` keyed by cache revision, model, target language, text, endpoint, adapter/budget identity, and (for batched formats) neighboring cues and context. Empty or truncated model outputs are never cached.
- **Safe interruption (`Ctrl-C`)**: pressing `Ctrl-C` stops dispatching new requests, cancels in-flight HTTP calls, and writes partial output for any currently open file. Re-running with the same arguments resumes from the on-disk cache.
- **Per-segment fault isolation & overflow recovery**:
  - Transient errors (`408`, `429`, `5xx`, transport/parse errors, empty responses) retry up to 4 attempts with exponential jitter honoring `Retry-After` up to a 30-second cap.
  - Context-length `4xx` errors and `finish_reason: "length"` truncations automatically halve the batch (or split a single oversized segment at a language boundary) and retry.
  - Non-recoverable `4xx` errors (e.g., unknown model ID, auth failure) fail that segment immediately without aborting other segments; untranslated segments remain in their original text.
- **Exit status**: returns `0` only if every file and segment succeeded without cancellation; returns a non-zero exit status if the run was interrupted or any file/segment failed.
