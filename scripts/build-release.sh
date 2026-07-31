#!/bin/sh
set -eu

if ! command -v x86_64-linux-gnu-gcc >/dev/null 2>&1; then
    echo "missing x86_64-linux-gnu-gcc (install gcc-x86-64-linux-gnu)" >&2
    exit 1
fi

rustup target add x86_64-unknown-linux-gnu >/dev/null
CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    cargo build --release --target x86_64-unknown-linux-gnu --bin ferryman-web
cargo build --release --bin ferryman-agent

mkdir -p build/web-libs
cp target/x86_64-unknown-linux-gnu/release/ferryman-web build/ferryman-web
cp target/release/ferryman-agent build/ferryman-agent
x86_64-linux-gnu-strip build/ferryman-web
strip build/ferryman-agent
cp build/ferryman-agent ai-pod-service/ferryman-agent
cp /etc/ssl/certs/ca-certificates.crt build/ca-certificates.crt

for library in \
    ld-linux-x86-64.so.2 \
    libgcc_s.so.1 \
    libm.so.6 \
    libc.so.6 \
    libresolv.so.2 \
    libnss_dns.so.2 \
    libnss_files.so.2
do
    cp "/usr/x86_64-linux-gnu/lib/$library" "build/web-libs/$library"
done
