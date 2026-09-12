#!/usr/bin/env bash
# Pack a tarball for manual on-router install (no opkg).
# Usage: openwrt/pack-bundle.sh <path-to-binary> [out.tar.gz]
set -euo pipefail

BIN="${1:?usage: pack-bundle.sh <binary> [out.tar.gz]}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OWRT="$ROOT/openwrt"
NAME=mimo_desktop_bridge
OUT="${2:-$OWRT/out/${NAME}_openwrt_aarch64.tar.gz}"

mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"
test -f "$BIN" || { echo "!! binary not found: $BIN" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
BUNDLE="$STAGE/${NAME}_openwrt"
mkdir -p "$BUNDLE"

cp -f "$BIN" "$BUNDLE/$NAME"
chmod +x "$BUNDLE/$NAME"
cp -R "$OWRT/files" "$BUNDLE/files"
cp -R "$OWRT/luci" "$BUNDLE/luci"
cp -f "$OWRT/install.sh" "$BUNDLE/install.sh"
chmod +x "$BUNDLE/install.sh"

tar -czf "$OUT" -C "$STAGE" "$(basename "$BUNDLE")"
echo "==> bundle: $OUT"
ls -lh "$OUT"
