# syntax=docker/dockerfile:1
# Standalone only. Lazycat keeps its native buildscript / embedded-image flow.
# Runtime uses the CUDA driver supplied by the NVIDIA container runtime.

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

# Orinfer publishes aarch64 binaries; execution packages are installed at build time.
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS orinfer-bundle
RUN apt-get update && apt-get install -y --no-install-recommends python3 ca-certificates qemu-user libc6-arm64-cross libgcc-s1-arm64-cross && rm -rf /var/lib/apt/lists/*
COPY scripts/bundle-orinfer.py scripts/orinfer-release.json /build/
RUN python3 /build/bundle-orinfer.py /opt/orinfer

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS agent
COPY --from=agent-binary /out/ferryman-agent /usr/local/bin/ferryman-agent
COPY --from=orinfer-bundle /opt/orinfer /opt/orinfer
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
ENV PATH=/opt/orinfer/bin:$PATH \
    ORINFER_EXECUTION_CACHE=/opt/orinfer/packages \
    FERRYMAN_AGENT_LISTEN=0.0.0.0:8090 \
    FERRYMAN_MODEL_ROOT=/models \
    FERRYMAN_ORINFER_ENDPOINT=http://127.0.0.1:18088 \
    FERRYMAN_IDLE_TIMEOUT_SECONDS=600 \
    NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility \
    RUST_LOG=info
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/ferryman-agent"]
