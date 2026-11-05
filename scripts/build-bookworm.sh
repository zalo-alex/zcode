#!/usr/bin/env bash
# Build a release binary against Debian bookworm's glibc (2.36) so it runs on Debian 12 (bookworm) and anything newer.
# Output: dist/zcode-linux-<arch>.tar.gz + .sha256   (same layout the release workflow publishes)
set -euo pipefail
cd "$(dirname "$0")/.."
ARCH=$(uname -m)
mkdir -p dist
docker run --rm -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/build -e ZCODE_COMMIT="${ZCODE_COMMIT:-$(git rev-parse --short HEAD 2>/dev/null || true)}" \
  -e ZCODE_DEFAULT_REPO="${ZCODE_DEFAULT_REPO:-zalo-alex/zcode}" \
  -v zcode-cargo-home:/cargo -v zcode-build-target:/build -v "$PWD":/src -w /src rust:bookworm \
  sh -c "cargo build --release --locked && cp /build/release/zcode /src/dist/zcode && chown $(id -u):$(id -g) /src/dist/zcode"
tar -C dist -czf "dist/zcode-linux-$ARCH.tar.gz" zcode
( cd dist && sha256sum "zcode-linux-$ARCH.tar.gz" > "zcode-linux-$ARCH.tar.gz.sha256" )
ls -la dist
