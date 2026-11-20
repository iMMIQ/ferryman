# Web Deployment & Operations

`ferryman-web` is a single self-contained HTTP service that embeds the static Vue 3 UI, manages per-user model API connections, runs a persistent SQLite job queue, streams real-time job updates over Server-Sent Events (SSE), and translates uploaded or mounted documents through external OpenAI-compatible Chat APIs.

No local GPU or embedded inference runtime is required.

> [!NOTE]
> The Web UI is currently localized in Simplified Chinese (for example, **新建翻译** = New Translation, **翻译任务** = Tasks, **模型与 API** = Models & API).

---

## Deployment Options

### 1. Standalone Docker (Recommended for Self-Hosting)

The standalone image ([`docker/Dockerfile`](../docker/Dockerfile), target `web`) builds `ferryman-web` with `--no-default-features` (no Lazycat discovery or ticket middleware) and installs `ca-certificates`, `curl`, `fonts-noto-cjk`, `fonts-noto-core`, and `fonts-dejavu-core`.

```bash
cp .env.example .env
docker compose -f docker-compose.standalone.yml up -d --build
```

The Compose file mounts three host directories configured via `.env`:

| `.env` Variable | Default | Container Mount | Purpose |
| --- | --- | --- | --- |
| `FERRYMAN_WEB_PORT` | `8080` | `8080` | Host port mapped to the Web UI/API |
| `FERRYMAN_DATA_DIR` | `./data` | `/data` | SQLite database (`jobs.sqlite3`), job artifacts, checkpoints, and per-user caches |
| `FERRYMAN_DOCS_DIR` | `./documents` | `/documents` | Mounted user documents library (`FERRYMAN_USER_DOCUMENTS_DIR`) |
| `FERRYMAN_REMOTEFS_DIR` | `./remotefs` | `/remotefs` | Mounted remote/cloud-drive library (`FERRYMAN_REMOTE_FS_DIR`) |

Open `http://127.0.0.1:8080` and add an OpenAI-compatible model service in **模型与 API**.

### 2. Running from Source

```bash
bun install --frozen-lockfile
bun run build
FERRYMAN_ALLOW_LOCAL_USER=true cargo run --bin ferryman-web
```

When `FERRYMAN_ALLOW_LOCAL_USER=true`, `ferryman-web` automatically creates `./ferryman-documents/local-development-user` and `./ferryman-remotefs/local-development-user` on startup.

### 3. Lazycat MicroServer

For Lazycat MicroServer LPK builds (`--features lazycat`) and delegated model discovery, see [docs/packaging.md](packaging.md).

---

## Authentication & Multi-User Isolation

Every API request requires a resolved user identity:

- **Authenticated proxy mode (`FERRYMAN_ALLOW_LOCAL_USER=false`, default)**: the backend reads the user ID from the `safe_uid` header (falling back to `x-hc-user-id`). Missing headers return HTTP `401`; unsafe UIDs containing `/`, `\`, `\0`, `.`, or `..` return HTTP `400`.
- **Single-user / local mode (`FERRYMAN_ALLOW_LOCAL_USER=true`)**: requests without identity headers fall back to `local-development-user`. Do **not** expose single-user mode directly to the public internet without an authenticating reverse proxy.

**Storage confinement**:

- Mounted storage roots (`FERRYMAN_USER_DOCUMENTS_DIR` and `FERRYMAN_REMOTE_FS_DIR`) expect one subdirectory per UID (`<root>/<uid>`).
- All directory listings, previews, reads, and writes are canonicalized and confined to the authenticated user's subtree; symbolic links are skipped or rejected.
- Database records (jobs, model configs, API keys, submission receipts) and shared translation caches are isolated per user (keyed by a SHA-256 owner hash of the UID).

---

## Environment Variables

### Server & Storage Paths

| Variable | Default | Description |
| --- | --- | --- |
| `FERRYMAN_WEB_LISTEN` | `0.0.0.0:8080` | TCP bind address for HTTP server. |
| `FERRYMAN_WEB_DIR` | `./dist/web` | Directory containing built frontend assets (`/app/web` in Docker/LPK images). |
| `FERRYMAN_DATA_DIR` | `./ferryman-data` | Persistent data directory storing `jobs.sqlite3`, job workspaces, and `users/<owner>/cache`. |
| `FERRYMAN_USER_DOCUMENTS_DIR` | `./ferryman-documents` | Root directory for user document libraries (`<dir>/<uid>`). Falls back to `FERRYMAN_DOCUMENTS_DIR` if unset. |
| `FERRYMAN_DOCUMENTS_DIR` | *(unset)* | Legacy alias read only when `FERRYMAN_USER_DOCUMENTS_DIR` is unset. |
| `FERRYMAN_REMOTE_FS_DIR` | `./ferryman-remotefs` | Root directory for mounted cloud-drive / remote libraries (`<dir>/<uid>`). |

### Security & Identity

| Variable | Default | Description |
| --- | --- | --- |
| `FERRYMAN_ALLOW_LOCAL_USER` | `false` | Set to `true` or `1` to fall back to `local-development-user` when proxy identity headers are absent. |
| `FERRYMAN_SECRET_KEY` | *(unset)* | Optional passphrase enabling application-layer `ChaCha20-Poly1305` encryption (`enc:v1:...`) for model API keys stored in SQLite. Legacy plaintext keys remain readable on load. **Back up this key**: changing or unsetting it makes previously encrypted API keys unreadable. |

### Retention & Cache Limits

| Variable | Default | Description |
| --- | --- | --- |
| `FERRYMAN_JOB_RETENTION_SECONDS` | `604800` (7 days) | Retention window for terminal jobs (`completed`, `failed`, `cancelled`) and idempotency receipts. Swept hourly; set to `0` to disable deletion. |
| `FERRYMAN_CACHE_MAX_BYTES` | `2147483648` (2 GiB) | Per-user shared translation cache size cap (evicts oldest modified entries first during hourly sweeps). Set to `0` to disable size eviction. |
| `FERRYMAN_CACHE_TTL_SECONDS` | `2592000` (30 days) | Maximum age of per-user shared translation cache files before eviction. Set to `0` to disable TTL eviction. |

### Platform Discovery, Fonts & Logging

| Variable | Default | Description |
| --- | --- | --- |
| `FERRYMAN_MODEL_DISCOVERY_URL` | `http://app.cloud.lazycat.app.ai.lzcx/api/v1/model-deployments` | Only effective when built with `--features lazycat`; ignored in standalone builds. Set empty (`""`) to disable discovery. |
| `FERRYMAN_PDF_FONT` | *(auto-discovered)* | Path to primary CJK OpenType font (`.ttc`/`.otf`/`.ttf`) for PDF rendering. |
| `FERRYMAN_PDF_FALLBACK_FONTS` | *(auto-discovered)* | OS path-list (colon-separated on Linux) of additional fallback fonts for PDF rendering. |
| `FERRYMAN_PDF_DEBUG` | *(unset)* | Enables verbose PDF extraction debug logging when set. |
| `RUST_LOG` | `info` | Standard `tracing` log filter (e.g., `info`, `ferryman_web=debug`). |

---

## Model Configuration & Concurrency

Users configure custom OpenAI-compatible Chat APIs in **模型与 API**:

- **URL normalization**: accepts bare origins (`http://host:8088`), `/v1`, provider-specific base paths, or full `/chat/completions` URLs. Embedded credentials, query strings, and URL fragments are rejected.
- **Model probing**: clicking **读取模型列表** calls `GET <base_url>/models` and optionally queries `/health` for `max_context`. Models without `/health` metadata default conservatively to a `2048` context window.
- **Secret isolation**: API keys are stored per owner in `jobs.sqlite3` (file mode `0600`) and are never returned by catalogue or job APIs. Editing a model's `base_url` never inherits a saved key from the previous destination.
- **Shared concurrency enforcement**: all jobs targeting the same `(base_url, model_id)` share a single in-flight request limiter (including retries). Job records pin the model ID, base URL, prompt adapter, and token budgets; deleting a connection or changing its URL requires resubmitting affected jobs.

---

## Job Lifecycle, Checkpoints & Limits

### Operational Quotas

| Constraint | Value |
| --- | --- |
| Max concurrent active jobs (global) | `8` |
| Max concurrent document parsers (global) | `2` |
| Max single upload file size | `512 MiB` |
| Max files per mounted directory/selection batch | `1,000` |
| Max non-terminal jobs per user | `2,000` |
| Batch size range (`batch_size`) | `1`–`100` (default `25`) |
| Context segments range (`context_segments`) | `0`–`50` (default `5`) |
| Per-request HTTP timeout | `180 s` |
| Job list page size | default `50`, max `100` |

### Queue & Save Semantics

- **States**: `queued` → `starting_model` → `translating` → `writing` → terminal (`completed`, `failed`, or `cancelled`). Jobs that finish with `failed_segments > 0` are surfaced in the UI as **部分完成** (Partially completed) with a retry action.
- **Output serialization**: jobs targeting the same output file path are serialized automatically; jobs writing to different paths run concurrently up to the global cap.
- **Cancellation boundary**: cancellation is accepted before `writing`; once final file writing starts, `POST /api/jobs/{id}/cancel` returns HTTP `409`. Accepted cancellations keep partial downloadable results inside the job directory and never overwrite a mounted source file.
- **Atomic mounted submissions**: directory/selection batches copy all inputs and enqueue every job in a single SQLite transaction. Clients pass a `request_id` UUID so retrying a submission is idempotent even if source directories changed afterward.
- **Checkpoints & retry safety**: every job keeps its successful segment translations in a private `<job-dir>/checkpoint` directory independent of the optional shared user cache. Retrying reuses those checkpoints and refuses to overwrite a previously saved mounted output file if the user edited it since the earlier attempt (the new output remains downloadable from the task).

---

## Storage Layout, Health Checks & Shutdown

Under `FERRYMAN_DATA_DIR`:

```text
<FERRYMAN_DATA_DIR>/
├── jobs.sqlite3              # Mode 0600, WAL mode (jobs, model_sources, submissions)
├── jobs/<job-uuid>/          # Input copy, partial/final output, private checkpoint/
└── users/<owner-hash>/cache/ # Sharded per-user shared translation cache
```

- **Health checks**: `GET /healthz` and `GET /api/healthz` execute a live SQLite ping (`200 {"status":"ok"}` or `503`). `GET /api/config` returns supported formats, storage kinds, upload caps, and default translation limits.
- **Graceful shutdown**: on `SIGINT` or `SIGTERM`, `ferryman-web` stops accepting new HTTP requests, closes SSE streams, cancels active translation jobs, waits up to 10 seconds for worker drain, and flushes pending SQLite updates. Interrupted non-terminal jobs are automatically recovered and re-queued on restart.
