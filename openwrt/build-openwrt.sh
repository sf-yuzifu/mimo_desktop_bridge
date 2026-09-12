#!/usr/bin/env bash
# Cross-compile the headless mimo_desktop_bridge server for OpenWrt
# (aarch64-unknown-linux-musl, fully static — reqwest/rustls, no C deps).
#
# WebUI is inlined by rust-embed from ./webui at compile time; no extra
# frontend build step is required.
#
# Usage:
#   bash openwrt/build-openwrt.sh
#
# Output:
#   openwrt/out/mimo_desktop_bridge   (aarch64 static, stripped)
set -euo pipefail

TARGET="aarch64-unknown-linux-musl"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="$ROOT/openwrt/out"

echo "==> repo root: $ROOT"
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
  : "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:?set this to your musl linker, e.g. aarch64-linux-musl-gcc}"
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
  echo "   ...or set CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER and rerun." >&2
  build_with_rustup
fi

BIN="$ROOT/target/$TARGET/release/mimo_desktop_bridge"
test -f "$BIN" || { echo "!! build did not produce $BIN" >&2; exit 1; }

cp -f "$BIN" "$OUT_DIR/mimo_desktop_bridge"
if command -v aarch64-linux-musl-strip >/dev/null 2>&1; then
  aarch64-linux-musl-strip "$OUT_DIR/mimo_desktop_bridge" || true
elif command -v llvm-strip >/dev/null 2>&1; then
  llvm-strip "$OUT_DIR/mimo_desktop_bridge" || true
fi

echo "==> binary: $OUT_DIR/mimo_desktop_bridge"
file "$OUT_DIR/mimo_desktop_bridge" 2>/dev/null || true
ls -lh "$OUT_DIR/mimo_desktop_bridge"
