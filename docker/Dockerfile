# syntax=docker/dockerfile:1
# Standalone only. Lazycat keeps its native buildscript / embedded-image flow.
# Model inference runs on a configured external HTTP API.

FROM oven/bun:1.4.4-slim@sha256:1846adf22496bd711036aae8be147e57bdebbaea2e5264e3e0b5af1db187d9a4 AS frontend
WORKDIR /frontend
COPY package.json bun.lock bunfig.toml ./
RUN --mount=type=cache,target=/root/.bun/install/cache bun install --frozen-lockfile
COPY tsconfig.json vite.config.ts ./
COPY web ./web
RUN bun run build

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
    cargo build --release --locked --no-default-features --bin ferryman-web \
    && mkdir /out && cp target/release/ferryman-web /out/ && strip /out/ferryman-web

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
