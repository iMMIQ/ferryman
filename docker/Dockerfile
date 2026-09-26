# syntax=docker/dockerfile:1
# Standalone only. Lazycat keeps its native buildscript / embedded-image flow.
ARG VLLM_BASE_IMAGE=docker.io/catdogai/lzc-aipod-vllm:agxorin-cu126-src-18f658bb3185-20260703@sha256:079f2ab5a3f19da5fbdf930476fed73625c3d47085407280fbee48c4293279be

FROM node:22.16.0-bookworm-slim@sha256:048ed02c5fd52e86fda6fbd2f6a76cf0d4492fd6c6fee9e2c463ed5108da0e34 AS frontend
WORKDIR /frontend
COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --prefer-offline --no-audit --no-fund
COPY tsconfig.json vite.config.ts ./
COPY web ./web
RUN npm run build

# Both targets use musl binaries, independent of the runtime's glibc version.
# Cache compilation separately by architecture; unrelated frontend/docs/data
# changes never enter this stage. BuildKit prunes the unused binary target.
FROM rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f AS rust-source
RUN apk add --no-cache musl-dev gcc binutils
WORKDIR /app
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock ./
COPY src ./src

FROM rust-source AS web-binary
ARG TARGETARCH
RUN --mount=type=cache,id=ferryman-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ferryman-target-${TARGETARCH},target=/app/target,sharing=locked \
    cargo build --release --locked --bin ferryman-web \
    && mkdir /out && cp target/release/ferryman-web /out/ && strip /out/ferryman-web

FROM rust-source AS agent-binary
ARG TARGETARCH
RUN --mount=type=cache,id=ferryman-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ferryman-target-${TARGETARCH},target=/app/target,sharing=locked \
    cargo build --release --locked --bin ferryman-agent \
    && mkdir /out && cp target/release/ferryman-agent /out/ && strip /out/ferryman-agent

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS web
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl fonts-noto-cjk fonts-noto-core fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
COPY --from=web-binary /out/ferryman-web /usr/local/bin/ferryman-web
COPY --from=frontend /frontend/dist/web /app/web
ENV FERRYMAN_WEB_LISTEN=0.0.0.0:8080 \
    FERRYMAN_WEB_DIR=/app/web \
    RUST_LOG=info
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/ferryman-web"]

FROM ${VLLM_BASE_IMAGE} AS agent
COPY --from=agent-binary /out/ferryman-agent /usr/local/bin/ferryman-agent
ENV FERRYMAN_AGENT_LISTEN=0.0.0.0:8090 \
    FERRYMAN_MODEL_ROOT=/models \
    FERRYMAN_IDLE_TIMEOUT_SECONDS=600 \
    RUST_LOG=info \
    LD_PRELOAD= \
    VLLM_CONFIGURE_LOGGING=0 \
    VLLM_DO_NOT_TRACK=1 \
    VLLM_NO_USAGE_STATS=1
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/ferryman-agent"]
