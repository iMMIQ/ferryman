#!/bin/sh
set -eu
. "$(dirname -- "$0")/build-common.sh"
PRODUCT=web
BINARY=ferryman-web
RUST_TARGET=x86_64-unknown-linux-gnu
CROSS_PREFIX=x86_64-linux-gnu
init_build
require_command npm
require_command node
test "$(node --version)" = "v$(cat .nvmrc)" || {
    echo "use the Node version in .nvmrc (nvm install && nvm use)" >&2
    exit 1
}

# Validate runtime inputs before compiling. Never reuse stale fonts or libraries.
# Resolve libraries through the selected compiler on both ARM64 and x86 hosts.
mkdir -p "$STAGE/web-libs" "$STAGE/fonts"
for library in ld-linux-x86-64.so.2 libgcc_s.so.1 libm.so.6 libc.so.6 libresolv.so.2 libnss_dns.so.2 libnss_files.so.2; do
    library_path=$("$CROSS_PREFIX-gcc" -print-file-name="$library")
    test "$library_path" != "$library" && test -r "$library_path" || {
        echo "compiler runtime library not found: $library" >&2
        exit 1
    }
    cp "$library_path" "$STAGE/web-libs/$library"
done
cp /etc/ssl/certs/ca-certificates.crt "$STAGE/ca-certificates.crt"
cp runtime/nsswitch.conf "$STAGE/nsswitch.conf"
cp /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc "$STAGE/fonts/"
for name in NotoSans NotoSansArabic NotoSansHebrew NotoSansDevanagari NotoSansThai; do
    cp "/usr/share/fonts/truetype/noto/$name-Regular.ttf" "$STAGE/fonts/"
done
cp /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf "$STAGE/fonts/"
for package in fonts-noto-cjk fonts-noto-core fonts-dejavu-core; do
    cp "/usr/share/doc/$package/copyright" "$STAGE/fonts/$package.copyright"
done
npm ci --prefer-offline --no-audit --no-fund
npm run build
cp -R dist/web "$STAGE/assets"
build_binary "$STAGE/ferryman-web"
python3 scripts/check-artifacts.py write web "$STAGE"
publish_stage
