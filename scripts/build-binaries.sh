#!/usr/bin/env bash
# Build release binaries and stage them under release/.
# Usage:
#   scripts/build-binaries.sh
# Env:
#   MDB_RUST_TARGET   optional rustc target triple
#   MDB_OUT_DIR       optional output dir (default: release/ or target-local/binaries/<target>)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_ARGS=()

if [ -n "${MDB_RUST_TARGET:-}" ]; then
  rustup target add "$MDB_RUST_TARGET"
  TARGET_ARGS=(--target "$MDB_RUST_TARGET")
fi

if [ -n "${MDB_OUT_DIR:-}" ]; then
  OUT_DIR="$MDB_OUT_DIR"
elif [ -n "${MDB_RUST_TARGET:-}" ]; then
  OUT_DIR="$ROOT/target-local/binaries/$MDB_RUST_TARGET"
else
  OUT_DIR="$ROOT/release"
fi

mkdir -p "$OUT_DIR"
cd "$ROOT/mimo_desktop_bridge"

if [ "${#TARGET_ARGS[@]}" -gt 0 ]; then
  cargo build --release "${TARGET_ARGS[@]}" --bin mimo_desktop_bridge
else
  cargo build --release --bin mimo_desktop_bridge
fi

target_dir="$ROOT/mimo_desktop_bridge/target"
if [ -n "${MDB_RUST_TARGET:-}" ]; then
  target_dir="$target_dir/$MDB_RUST_TARGET"
fi
target_dir="$target_dir/release"

name="mimo_desktop_bridge"
if [[ "${MDB_RUST_TARGET:-}" == *windows* ]]; then
  name="$name.exe"
fi

cp -f "$target_dir/$name" "$OUT_DIR/"
echo "==> binary: $OUT_DIR/$name"
ls -lh "$OUT_DIR/$name"
