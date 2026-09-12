#!/usr/bin/env bash
# Build an OpenWrt 25.12+ APK (APKv3) via `apk mkpkg`.
# Requires apk-tools v3 (apk mkpkg) on the host — used by CI after compiling
# apk-tools from source.
#
# Usage:
#   openwrt/build-apk.sh <path-to-binary> [output.apk]
#
# Env:
#   PKG_VERSION  default 0.1.0
#   PKG_RELEASE  default 1
set -euo pipefail

BIN="${1:?usage: build-apk.sh <binary> [output.apk]}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OWRT="$ROOT/openwrt"

PKG_NAME="luci-app-mimo-desktop-bridge"
PKG_VERSION="${PKG_VERSION:-0.1.0}"
PKG_RELEASE="${PKG_RELEASE:-1}"
OUT="${2:-$OWRT/out/${PKG_NAME}-${PKG_VERSION}-r${PKG_RELEASE}.apk}"

mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"
test -f "$BIN" || { echo "!! binary not found: $BIN" >&2; exit 1; }

command -v apk >/dev/null 2>&1 || { echo "!! apk-tools v3 (apk) not found" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

DATA="$STAGE/data"
mkdir -p \
  "$DATA/usr/bin" \
  "$DATA/etc/init.d" \
  "$DATA/etc/config" \
  "$DATA/www/luci-static/resources/view/mimo-desktop-bridge" \
  "$DATA/usr/share/luci/menu.d" \
  "$DATA/usr/share/rpcd/acl.d"

install -m 0755 "$BIN" "$DATA/usr/bin/mimo_desktop_bridge"
install -m 0755 "$OWRT/files/etc/init.d/mimo_desktop_bridge" \
  "$DATA/etc/init.d/mimo_desktop_bridge"
install -m 0644 "$OWRT/files/etc/config/mimo_desktop_bridge" \
  "$DATA/etc/config/mimo_desktop_bridge"
install -m 0644 "$OWRT/luci/htdocs/luci-static/resources/view/mimo-desktop-bridge/overview.js" \
  "$DATA/www/luci-static/resources/view/mimo-desktop-bridge/overview.js"
install -m 0644 "$OWRT/luci/root/usr/share/luci/menu.d/luci-app-mimo-desktop-bridge.json" \
  "$DATA/usr/share/luci/menu.d/luci-app-mimo-desktop-bridge.json"
install -m 0644 "$OWRT/luci/root/usr/share/rpcd/acl.d/luci-app-mimo-desktop-bridge.json" \
  "$DATA/usr/share/rpcd/acl.d/luci-app-mimo-desktop-bridge.json"

INFO="$STAGE/info"
cat > "$INFO" <<EOF
PKGNAME = ${PKG_NAME}
PKGVER = ${PKG_VERSION}-r${PKG_RELEASE}
PKGDESC = LuCI app + headless server for mimo_desktop_bridge
PKGMAINTAINER = mimo_desktop_bridge
PKGARCH = aarch64
SIZE = $(du -sb "$DATA" | awk '{print $1}')
datahash = $(find "$DATA" -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | awk '{print $1}')
EOF

# apk mkpkg takes a directory tree + control info
apk mkpkg --root "$DATA" --info "$INFO" --output "$OUT"

echo "==> apk: $OUT"
ls -lh "$OUT"
