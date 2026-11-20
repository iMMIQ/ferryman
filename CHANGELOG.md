# Changelog

All notable changes to Ferryman are documented in this file.

## Unreleased

### Added

- Real-time per-user job updates via Server-Sent Events (`/api/jobs/events`) with adaptive polling fallback in the Web UI.
- Optional at-rest AEAD encryption (`ChaCha20-Poly1305`) for stored model API keys via `FERRYMAN_SECRET_KEY`.
- SQLite health endpoints (`/healthz` and `/api/healthz`), graceful shutdown handling (`SIGINT`/`SIGTERM`), submission receipt retention cleanup, and bounded per-user cache eviction (`FERRYMAN_CACHE_MAX_BYTES`, `FERRYMAN_CACHE_TTL_SECONDS`).
- EPUB `--mode replace` (translation-only output) preserving chapter markup, links, images, and inline styling.
- Automated documentation drift checks (`bun run docs:check`) and trilingual README entry points (`README.md`, `README.zh-CN.md`, `README.ja.md`).

### Changed

- Split the repository into a 3-crate Cargo workspace (`ferryman-core`, `ferryman-cli`, `ferryman-web`) and consolidated Dockerfiles under `docker/`.
- Unified CLI and Web translation clients on OpenAI-compatible Chat Completions with versioned `PromptAdapter` strategies (`generic` and `index-translate`).
- Migrated frontend tooling, scripts, and lockfile management to Bun with a 7-day minimum release age check (`bun run deps:check-age`) covering both `Cargo.lock` and `bun.lock`.
- Documented `LGPL-3.0-or-later` licensing (`COPYING.LESSER` and `COPYING`).

### Removed

- Removed legacy `ferryman-agent`, AI Pod resource packaging, hardcoded CLI `--preset` options, `--serve` container management, and Orinfer JSONL execution paths.

### Migration Notes

- **CLI users**: replace `--preset <name>` with explicit `--endpoint`, `--model`, `--adapter`, `--context-length`, `--max-output-tokens`, and `--concurrency` options.
- **Web upgrades from legacy self-deployed versions**: existing job history is preserved; unfinished jobs without a saved external model selection cannot resume automatically and must be resubmitted against a configured model API.
- **API key encryption**: enabling `FERRYMAN_SECRET_KEY` encrypts newly saved keys while remaining able to read legacy plaintext keys. Keep the secret backed up—removing or changing it prevents decrypting already-encrypted keys.

## 0.4.2

- Switched Web model execution to external OpenAI-compatible Chat APIs with optional Lazycat MicroServer delegated discovery (`lzcapp.user_delegate`, requiring LZCOS 1.5.2+).
- Added per-model shared concurrency enforcement driven by `/health.scheduler.max_active` for discovered models and explicit limits for custom APIs.
- Simplified the translation workspace UI with reduced-motion-aware transitions and shared mobile/desktop job controls.

## 0.4.0 – 0.4.1

- Adopted Index-Translate prompt support and shared translation contracts across CLI and Web.
- Hardened translation retries, cancellation boundaries, per-job checkpoints, and archive decompression limits.

## 0.3.0 – 0.3.1

- Added layout-preserving PDF translation with CJK/fallback OpenType font subsetting and bidirectional text shaping.
- Migrated the Web frontend to Vue 3, TypeScript, Reka UI, and TanStack Vue Query with `ts-rs` generated wire types.
- Reworked Web job persistence onto SQLite (`jobs.sqlite3`) with retention sweeping and standalone Docker support.
- Added DOCX, Markdown, plain text, and subtitle/lyrics (SRT, VTT, ASS/SSA, LRC) format backends.
