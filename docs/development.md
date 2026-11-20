# Developer & Contributor Guide

## Workspace Layout

Ferryman is a 3-crate Cargo workspace paired with a Vue 3 / TypeScript frontend:

```text
ferryman/
├── crates/
│   ├── ferryman-core/    # Document parsers/writers, batch queue, cache, OpenAI chat client & prompt adapters
│   ├── ferryman-cli/     # `ferryman` CLI binary
│   └── ferryman-web/     # `ferryman-web` Axum server, SQLite store, SSE hub, models, optional `lazycat` discovery
├── web/                  # Vue 3 + TypeScript + Vite + Reka UI + TanStack Vue Query SPA
│   └── src/
│       ├── components/   # Domain panels + shared `components/ui/` primitives
│       ├── composables/  # State owners: `useJobs`, `useSubmission`, `useFolder`, `useModels`
│       └── lib/          # `api.ts`, `query.ts`, `types.ts`, and `generated/` (ts-rs output)
├── docker/               # `Dockerfile` (standalone musl image) and `Dockerfile.lzc` (Lazycat scratch image)
├── scripts/              # Release staging, artifact verification, smoke checks, type & doc & dependency-age checks
└── tests/                # Bun unit tests, Playwright UI regressions, and real-backend smoke tests
```

## Toolchain & Prerequisites

Versions are pinned in repository metadata files—always treat those files as authoritative:

- **Rust**: [`rust-toolchain.toml`](../rust-toolchain.toml) (includes `rustfmt` and `clippy`)
- **Bun**: [`.bun-version`](../.bun-version) (package manager, frontend build, scripts, unit tests)
- **Node.js**: [`.nvmrc`](../.nvmrc) (compatibility pin)
- **PDF test fonts** (required by `cargo test`): on Debian/Ubuntu, install `fonts-noto-cjk` and `fonts-dejavu-core` (used for CJK and Arabic shaping fixtures), or point `FERRYMAN_PDF_FONT` at an installed CJK font.

```bash
sudo apt-get install -y fonts-noto-cjk fonts-dejavu-core
bun install --frozen-lockfile
```

## Local Development Loop

Run the Rust backend and Vite dev server in separate terminals:

```bash
# Terminal 1: Rust Web server (listens on 0.0.0.0:8080, single-user local identity)
FERRYMAN_ALLOW_LOCAL_USER=true cargo run --bin ferryman-web

# Terminal 2: Vite dev server (proxies /api to 127.0.0.1:8080)
bun run dev
```

For production static assets (`dist/web`, served by `ferryman-web` via `FERRYMAN_WEB_DIR`):

```bash
bun run build
```

## Rust ↔ TypeScript Contracts (`ts-rs`)

Wire types under `web/src/lib/generated/` are exported from Rust via `ts-rs`. **Do not edit generated files manually.**

```bash
bun run types:generate  # Regenerate web/src/lib/generated/ after changing Rust DTOs
bun run types:check     # Verify no drift without modifying the working tree
```

Notes:
- Large Rust integers are mapped to TypeScript `number` (`with_large_int("number")`); keep persisted counters and timestamps within JavaScript safe-integer precision.
- Generated types describe wire shapes only and do not perform runtime validation. UI drafts and derived view models belong in [`web/src/lib/types.ts`](../web/src/lib/types.ts).

## Frontend Conventions

- **State & networking**: `useJobs`, `useSubmission`, `useFolder`, and `useModels` own domain state; [`web/src/lib/api.ts`](../web/src/lib/api.ts) centralizes Fetch timeouts and error classification. TanStack Vue Query manages caching, cancellation, and invalidation. `useJobs` subscribes to `/api/jobs/events` (SSE) with adaptive polling fallback that pauses when the page is hidden.
- **Styling**: design tokens live in [`web/src/tokens.css`](../web/src/tokens.css), transitions in [`web/src/motion.css`](../web/src/motion.css) (honoring `prefers-reduced-motion`), and shared UI primitives under `web/src/components/ui/`.
- **Formatting**: run `bun run format` after editing frontend, test, or script files.

## Web HTTP API Overview

All routes are defined in [`crates/ferryman-web/src/main.rs`](../crates/ferryman-web/src/main.rs). Exact request/response schemas live in `web/src/lib/generated/`.

| Group | Method & Path | Purpose |
| --- | --- | --- |
| Health & Config | `GET /healthz`, `GET /api/healthz` | Live SQLite health check |
| | `GET /api/config` | Supported formats, storage kinds, upload & batch limits |
| Documents | `GET /api/documents?storage=&path=` | List files and directories under user's mounted subtree |
| | `POST /api/documents/directories` | Create a directory under user's mounted subtree |
| Jobs | `GET /api/jobs?cursor=&limit=&phase=` | Cursor-paginated job history |
| | `POST /api/jobs` | Multipart upload single-file translation job |
| | `GET /api/jobs/active` | List non-terminal jobs |
| | `GET /api/jobs/events` | Per-user SSE stream (`job` and `resync` events) |
| | `POST /api/jobs/selection/preview` | Advisory preview of mounted file/directory selection |
| | `POST /api/jobs/selection` (alias `/api/jobs/directory`) | Atomically enqueue mounted batch jobs (idempotent by `request_id`) |
| | `GET /api/jobs/{id}`, `DELETE /api/jobs/{id}` | Inspect or delete a job |
| | `POST /api/jobs/{id}/cancel` | Cancel before `writing` (`409` once writing begins) |
| | `POST /api/jobs/{id}/retry` | Retry failed, cancelled, or partially completed job |
| | `GET /api/jobs/{id}/result` | Download translated or partial document |
| Models | `GET /api/models`, `POST /api/models` | List models or create/update a custom OpenAI-compatible API |
| | `POST /api/models/probe` | Query `/models` and optional `/health` metadata |
| | `DELETE /api/models/{id}` | Delete a custom model connection |
| | `POST /api/models/discover` | Trigger Lazycat delegated discovery (`--features lazycat` only) |

## Verification & Test Matrix

Run the same checks enforced by [`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

```bash
# Rust checks (test both default standalone and lazycat features)
cargo fmt --all -- --check
cargo clippy --all-targets --locked --no-default-features -- -D warnings
cargo clippy --all-targets --locked --no-default-features --features lazycat -- -D warnings
cargo test --all-targets --locked --no-default-features
cargo test --all-targets --locked --no-default-features --features lazycat

# Contract, formatting, documentation, and unit checks
bun run types:check
bun run docs:check
bun run format:check
bun run test:unit

# Browser UI regression tests (Playwright mocked APIs) and real-backend smoke test
bun --bun playwright install --with-deps chromium
bun run test
cargo build --locked --bin ferryman-web
bun run test:smoke
```

Inspect Playwright failure traces from `playwright-report/` or `test-results/` with `bun --bun playwright show-trace <trace.zip>`. Set `UI_SCREENSHOT_DIR=/tmp/ferryman-ui` during `bun run test` to save screenshots.

## Dependency Age Policy

[`bunfig.toml`](../bunfig.toml) enforces a **7-day minimum publication age** (`minimumReleaseAge = 604800`) on newly resolved JavaScript packages. Because Cargo does not yet enforce release age natively, [`scripts/check-dependency-age.mjs`](../scripts/check-dependency-age.mjs) audits **both** `Cargo.lock` and `bun.lock` against the configured mirrors:

```bash
bun run deps:check-age
```

- Requires Bun and `curl`. Missing publication dates, yanked crates, releases newer than 7 days, and checksum mismatches fail CI before installation or compilation.
- Always install existing dependencies with `bun install --frozen-lockfile` and commit lockfile updates alongside manifest changes.

## Documentation Maintenance Rules

To keep documentation accurate over time:

1. **Keep evergreen docs evergreen**: never append release notes or `"after this update"` prose to `README.md` or `docs/*.md`—record changes in [`CHANGELOG.md`](../CHANGELOG.md).
2. **Respect single-source boundaries**:
   - CLI flag changes → update [`docs/cli.md`](cli.md)
   - New or changed format behavior → update [`docs/formats.md`](formats.md)
   - Runtime environment variable or operational limit changes → update [`docs/deployment.md`](deployment.md)
   - Lazycat packaging or discovery changes → update [`docs/packaging.md`](packaging.md)
3. **Pass `bun run docs:check`**: CI runs [`scripts/check-docs.mjs`](../scripts/check-docs.mjs) to verify local markdown links, CLI flag coverage, runtime `FERRYMAN_*` environment variable coverage, and absence of removed legacy references.
