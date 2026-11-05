#!/usr/bin/env bash
# zcode installer (same idea as opencode's):
#   curl -fsSL https://raw.githubusercontent.com/zalo-alex/zcode/main/install.sh | bash
#
# env: ZCODE_REPO          github "owner/name" (default: zalo-alex/zcode)
#      ZCODE_VERSION       release tag (default: latest)
#      ZCODE_INSTALL       install root (default: ~/.zcode, binary in <root>/bin)
#      ZCODE_DOWNLOAD_BASE override release download base URL (tests / mirrors)
set -euo pipefail

REPO=${ZCODE_REPO:-zalo-alex/zcode}
VERSION=${ZCODE_VERSION:-latest}
ROOT=${ZCODE_INSTALL:-$HOME/.zcode}

die() { echo "zcode install: $*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "only Linux builds are published"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac
command -v curl >/dev/null || die "curl is required"
command -v tar >/dev/null || die "tar is required"

if [ -n "${ZCODE_DOWNLOAD_BASE:-}" ]; then
  BASE=$ZCODE_DOWNLOAD_BASE
else
  if [ "$VERSION" = latest ]; then BASE="https://github.com/$REPO/releases/latest/download"
  else BASE="https://github.com/$REPO/releases/download/$VERSION"; fi
fi

ASSET="zcode-linux-$ARCH.tar.gz"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
echo "downloading $BASE/$ASSET"
curl -fsSL -o "$TMP/$ASSET" "$BASE/$ASSET" || die "download failed"
if curl -fsSL -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256"; then
  ( cd "$TMP" && sha256sum -c "$ASSET.sha256" >/dev/null ) || die "checksum mismatch"
else
  echo "warning: no checksum published, skipping verification" >&2
fi
tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -f "$TMP/zcode" ] || die "archive does not contain a zcode binary"

mkdir -p "$ROOT/bin"
install -m 0755 "$TMP/zcode" "$ROOT/bin/zcode.new"
mv -f "$ROOT/bin/zcode.new" "$ROOT/bin/zcode"   # atomic; safe while an old zcode is running

LINK=""
if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then LINK=/usr/local/bin/zcode; fi
if [ -n "$LINK" ]; then ln -sf "$ROOT/bin/zcode" "$LINK"; fi

echo "installed: $("$ROOT/bin/zcode" --version) -> $ROOT/bin/zcode${LINK:+ (linked at $LINK)}"
if ! command -v zcode >/dev/null 2>&1; then
  echo "add to PATH:  export PATH=\"$ROOT/bin:\$PATH\""
fi
