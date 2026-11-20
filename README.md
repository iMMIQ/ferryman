# Ferryman

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

**Ferryman** is a multi-format document translator that produces **bilingual** (original + translation) or **translation-only** output through any OpenAI-compatible Chat API. It ships as both a standalone CLI (`ferryman`) and a single-image Web application (`ferryman-web`) for Docker or Lazycat MicroServer.

## Highlights

- **Nine document formats**: translates EPUB, DOCX, PDF, Markdown, plain text, SRT, VTT, ASS/SSA, and LRC while preserving document structure, formatting, and non-text assets.
- **Any OpenAI-compatible API**: connects to remote or self-hosted Chat Completions endpoints with a generic translation adapter and a dedicated `index-translate` adapter. No local GPU or embedded inference runtime is required.
- **Resumable by design**: content-addressed translation caches, per-job checkpoints in the Web queue, automatic context-overflow splitting, and graceful interruption with partial output.
- **Single-image Web workspace**: Vue 3 UI served directly by the Rust backend, backed by SQLite, real-time Server-Sent Events (SSE), per-user storage isolation, and optional at-rest API key encryption.
- **Optional Lazycat MicroServer integration**: compiled via `--features lazycat` to discover deployed models through delegated platform authentication and read/write mounted document and cloud-drive libraries.

## Supported Formats

| Format | Extensions | Bilingual | Replace | Strategy |
| --- | --- | --- | --- | --- |
| EPUB | `.epub` | Styled sibling block (`.hy-zh`) | In-place text replacement preserving markup | Independent |
| DOCX | `.docx` | Appended translated paragraph | In-place run-preserving replacement | Independent |
| PDF | `.pdf` | Interleaved mirrored translation page | Overlay replacement in paragraph box | Independent |
| Markdown | `.md`, `.markdown` | Duplicated translated block | In-place text-node replacement | Batched |
| Plain text | `.txt` | Original line + translated line | Line replacement | Batched |
| SubRip / WebVTT | `.srt`, `.vtt` | Original + translated cue lines | Cue text replacement | Batched |
| ASS / SSA | `.ass`, `.ssa` | Stacked with `\N` inside cue | Cue text replacement | Batched |
| LRC lyrics | `.lrc` | Duplicate timestamped line | Line replacement | Batched |

See [Supported Formats & Behavior](docs/formats.md) for layout rules, PDF font configuration, archive limits, and format-specific caveats.

## Quick Start

### 1. CLI

Build from source (requires Rust toolchain from [`rust-toolchain.toml`](rust-toolchain.toml)):

```bash
cargo build --release --bin ferryman
```

Translate a single file or recursively translate a directory through any OpenAI-compatible API:

```bash
# Generic OpenAI-compatible endpoint
export FERRYMAN_API_KEY="sk-..."
./target/release/ferryman \
  -i book.epub -o book.bilingual.epub \
  --endpoint https://api.openai.com/v1 \
  --model gpt-4o-mini \
  --target 中文

# Index-Translate model on a local OpenAI-compatible server
./target/release/ferryman \
  -i ./books --mode replace --target English \
  --endpoint http://127.0.0.1:8088/v1 \
  --model index-translate-2b \
  --adapter index-translate
```

See the [CLI Guide](docs/cli.md) for all flags, output naming, caching, and error recovery.

### 2. Standalone Docker (Web App)

```bash
cp .env.example .env
docker compose -f docker-compose.standalone.yml up -d --build
```

Open `http://127.0.0.1:8080`, add an OpenAI-compatible model endpoint in **模型与 API** (Models & API), and upload files or pick documents from the mounted directories.

> [!NOTE]
> Standalone Docker runs in single-user mode (`FERRYMAN_ALLOW_LOCAL_USER=true`) by default. Put an authenticating reverse proxy in front that injects `safe_uid` before exposing it to multiple users or a public network. See [Deployment & Operations](docs/deployment.md).

### 3. Lazycat MicroServer

Lazycat MicroServer builds package a single LPK with the `lazycat` feature enabled for delegated model discovery and mounted document/cloud-drive access:

```bash
bun install --frozen-lockfile
bun --bun lzc-cli project lint .
bun --bun lzc-cli project release -o dist/ferryman-web.lpk
```

See [Lazycat Packaging & Integration](docs/packaging.md) for prerequisites, local Buildx assembly, artifact verification, and discovery authorization.

## Documentation

| Document | Scope |
| --- | --- |
| [docs/cli.md](docs/cli.md) | Complete CLI options, prompt adapters, output paths, caching, and interruption handling |
| [docs/formats.md](docs/formats.md) | Format behavior matrix, EPUB/DOCX/PDF/Markdown/subtitle rules, fonts, and limits |
| [docs/deployment.md](docs/deployment.md) | Docker deployment, reverse-proxy auth, full environment variable reference, storage, and operations |
| [docs/packaging.md](docs/packaging.md) | Lazycat MicroServer LPK builds, verification scripts, platform mounts, and model discovery |
| [docs/development.md](docs/development.md) | Workspace architecture, local dev loop, `ts-rs` contracts, HTTP API map, testing, and dependency policy |
| [CHANGELOG.md](CHANGELOG.md) | Version history and migration notes |

## License

Ferryman is licensed under the **LGPL-3.0-or-later**. See [COPYING.LESSER](COPYING.LESSER) for the LGPL-3.0 text and [COPYING](COPYING) for the referenced GPL-3.0 text.
