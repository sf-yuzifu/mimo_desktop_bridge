#!/usr/bin/env bash
# Build a real, opkg-installable .ipk for OpenWrt — WITHOUT needing the SDK.
#
# An ipk is a gzip-compressed tar of:
#   ./debian-binary  ./data.tar.gz  ./control.tar.gz
#
# Usage:
#   openwrt/build-ipk.sh <path-to-binary> [output.ipk]
#
# Env overrides:
#   PKG_VERSION   default 0.2.0
#   PKG_RELEASE   default 1
#   PKG_ARCH      default aarch64_cortex-a53
set -euo pipefail

BIN="${1:?usage: build-ipk.sh <binary> [output.ipk]}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OWRT="$ROOT/openwrt"

PKG_NAME="luci-app-mimo-desktop-bridge"
PKG_VERSION="${PKG_VERSION:-0.2.0}"
PKG_RELEASE="${PKG_RELEASE:-1}"
PKG_ARCH="${PKG_ARCH:-aarch64_cortex-a53}"

OUT="${2:-$OWRT/out/${PKG_NAME}_${PKG_VERSION}-${PKG_RELEASE}_${PKG_ARCH}.ipk}"
mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"

test -f "$BIN" || { echo "!! binary not found: $BIN" >&2; exit 1; }
export COPYFILE_DISABLE=1

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

install -m 0755 "$BIN" \
  "$DATA/usr/bin/mimo_desktop_bridge"
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

INSTALLED_SIZE=0
while IFS= read -r f; do
  sz=$(wc -c < "$f")
  INSTALLED_SIZE=$((INSTALLED_SIZE + sz))
done < <(find "$DATA" -type f)

CTRL="$STAGE/control"
mkdir -p "$CTRL"
cat > "$CTRL/control" <<EOF
Package: ${PKG_NAME}
Version: ${PKG_VERSION}-${PKG_RELEASE}
Depends: luci-base
Source: openwrt/
SourceName: ${PKG_NAME}
Section: luci
SourceDateEpoch: 0
Architecture: ${PKG_ARCH}
Installed-Size: ${INSTALLED_SIZE}
Maintainer: mimo_desktop_bridge
Description: LuCI app + headless server for mimo_desktop_bridge.
 Runs Xiaomi MiMo Desktop free channel as a local OpenAI/Anthropic endpoint
 and embeds its WebUI into LuCI via an iframe.
EOF

cat > "$CTRL/conffiles" <<EOF
/etc/config/mimo_desktop_bridge
EOF

cat > "$CTRL/postinst" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT}" ] && exit 0
if [ -x /etc/init.d/mimo_desktop_bridge ]; then
	/etc/init.d/mimo_desktop_bridge enable 2>/dev/null
	/etc/init.d/mimo_desktop_bridge start 2>/dev/null
fi
rm -f /tmp/luci-indexcache
rm -rf /tmp/luci-modulecache
killall -HUP rpcd 2>/dev/null
exit 0
EOF

cat > "$CTRL/prerm" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT}" ] && exit 0
if [ -x /etc/init.d/mimo_desktop_bridge ]; then
	/etc/init.d/mimo_desktop_bridge stop 2>/dev/null
	/etc/init.d/mimo_desktop_bridge disable 2>/dev/null
fi
exit 0
EOF

cat > "$CTRL/postrm" <<'EOF'
#!/bin/sh
rm -f /tmp/luci-indexcache
rm -rf /tmp/luci-modulecache
killall -HUP rpcd 2>/dev/null
exit 0
EOF

chmod 0755 "$CTRL/postinst" "$CTRL/prerm" "$CTRL/postrm"

BUILD="$STAGE/build"
mkdir -p "$BUILD"
printf '2.0\n' > "$BUILD/debian-binary"

tar_opts=""
if tar --version 2>/dev/null | grep -qi 'gnu tar'; then
  tar_opts="--numeric-owner --owner=0 --group=0 --sort=name"
elif tar --version 2>/dev/null | grep -qi 'bsdtar'; then
  tar_opts="--numeric-owner --uid 0 --gid 0 --no-mac-metadata --no-xattrs"
fi

# shellcheck disable=SC2086
( cd "$CTRL" && tar $tar_opts -czf "$BUILD/control.tar.gz" ./* )
# shellcheck disable=SC2086
( cd "$DATA" && tar $tar_opts -czf "$BUILD/data.tar.gz" ./* )

rm -f "$OUT"
# shellcheck disable=SC2086
( cd "$BUILD" && tar $tar_opts -czf "$OUT" ./debian-binary ./data.tar.gz ./control.tar.gz )

echo "==> ipk: $OUT"
ls -lh "$OUT"
echo "    arch=${PKG_ARCH}  version=${PKG_VERSION}-${PKG_RELEASE}  installed-size=${INSTALLED_SIZE}"
