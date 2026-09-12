#!/usr/bin/env bash
# Cross-compile the headless mimo_desktop_bridge server for OpenWrt
# (musl, fully static — reqwest/rustls, no C deps).
#
# WebUI is inlined by rust-embed from ./webui at compile time; no extra
# frontend build step is required.
#
# Usage:
#   bash openwrt/build-openwrt.sh
#   MDB_OPENWRT_TARGET=x86_64-unknown-linux-musl \
#   MDB_OPENWRT_ARCH=x86_64 \
#     bash openwrt/build-openwrt.sh
#
# Output:
#   openwrt/out/mimo_desktop_bridge_<arch>   (static, stripped)
set -euo pipefail

TARGET="${MDB_OPENWRT_TARGET:-aarch64-unknown-linux-musl}"
ARCH_LABEL="${MDB_OPENWRT_ARCH:-aarch64}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="$ROOT/openwrt/out"
OUT_BIN="$OUT_DIR/mimo_desktop_bridge_${ARCH_LABEL}"

echo "==> repo root: $ROOT  target=$TARGET arch=$ARCH_LABEL"
mkdir -p "$OUT_DIR"
test -f "$ROOT/webui/index.html" || { echo "!! webui/index.html missing" >&2; exit 1; }

cd "$ROOT"

build_with_cross() {
  echo "==> cross build for $TARGET (docker)"
  cross build --release --target "$TARGET" --bin mimo_desktop_bridge
}

build_with_rustup() {
  echo "==> native rustup cross build for $TARGET"
  rustup target add "$TARGET"
  local linker_var
  linker_var="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER"
  : "${!linker_var:?set $linker_var, e.g. ${ARCH_LABEL}-linux-musl-gcc}"
  cargo build --release --target "$TARGET" --bin mimo_desktop_bridge
}

if command -v cross >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  build_with_cross
elif command -v cross >/dev/null 2>&1; then
  echo "!! 'cross' found but Docker daemon not reachable; falling back to rustup." >&2
  build_with_rustup
else
  echo "!! 'cross' not found. Install it for the easiest path:" >&2
  echo "     cargo install cross --git https://github.com/cross-rs/cross" >&2
  echo "   ...or set the musl linker env and rerun." >&2
  build_with_rustup
fi

BIN="$ROOT/target/$TARGET/release/mimo_desktop_bridge"
test -f "$BIN" || { echo "!! build did not produce $BIN" >&2; exit 1; }

cp -f "$BIN" "$OUT_BIN"
if command -v "${ARCH_LABEL}-linux-musl-strip" >/dev/null 2>&1; then
  "${ARCH_LABEL}-linux-musl-strip" "$OUT_BIN" || true
elif command -v llvm-strip >/dev/null 2>&1; then
  llvm-strip "$OUT_BIN" || true
fi

echo "==> binary: $OUT_BIN"
file "$OUT_BIN" 2>/dev/null || true
ls -lh "$OUT_BIN"
