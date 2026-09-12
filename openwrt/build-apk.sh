#!/usr/bin/env bash
# Build a real, apk-installable .apk for OpenWrt 25.12+ — WITHOUT the full SDK.
#
# OpenWrt 25.12+ uses Alpine apk-tools v3; packages must be APKv3.
# Mirrors OpenWrt package-pack.mk: `apk mkpkg --info/--script/--files`.
#
# Filename: <name>-<version>.apk   Version: <pkgver>-r<release>
#
# Usage:
#   openwrt/build-apk.sh <path-to-binary> [output.apk]
#
# Env:
#   PKG_VERSION  default 0.1.0
#   PKG_RELEASE  default 1
#   PKG_ARCH     default aarch64_cortex-a53
#   APK_BIN      optional path to apk with mkpkg
set -euo pipefail

BIN="${1:?usage: build-apk.sh <binary> [output.apk]}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OWRT="$ROOT/openwrt"

PKG_NAME="luci-app-mimo-desktop-bridge"
PKG_VERSION="${PKG_VERSION:-0.1.0}"
PKG_RELEASE="${PKG_RELEASE:-1}"
PKG_ARCH="${PKG_ARCH:-aarch64_cortex-a53}"
PKG_VER_FULL="${PKG_VERSION}-r${PKG_RELEASE}"
PKG_DESC="LuCI app + headless server for mimo_desktop_bridge. Exposes the Xiaomi MiMo Desktop free channel as a local OpenAI/Anthropic/Responses endpoint."

OUT="${2:-$OWRT/out/${PKG_NAME}-${PKG_VER_FULL}.apk}"
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
  "$DATA/usr/share/rpcd/acl.d" \
  "$DATA/lib/apk/packages"

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

cat > "$DATA/lib/apk/packages/${PKG_NAME}.conffiles" <<EOF
/etc/config/mimo_desktop_bridge
EOF

if command -v sha256sum >/dev/null 2>&1; then
  csum=$(sha256sum "$DATA/etc/config/mimo_desktop_bridge" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  csum=$(shasum -a 256 "$DATA/etc/config/mimo_desktop_bridge" | awk '{print $1}')
else
  csum=""
fi
if [ -n "$csum" ]; then
  echo "/etc/config/mimo_desktop_bridge $csum" \
    > "$DATA/lib/apk/packages/${PKG_NAME}.conffiles_static"
fi

(
  cd "$DATA"
  find . \( -type f -o -type l \) | sed 's|^\./|/|' | sort \
    > "lib/apk/packages/${PKG_NAME}.list"
)

SCRIPTS="$STAGE/scripts"
mkdir -p "$SCRIPTS"

cat > "$SCRIPTS/post-install" <<'EOF'
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
cp "$SCRIPTS/post-install" "$SCRIPTS/post-upgrade"

cat > "$SCRIPTS/pre-deinstall" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT}" ] && exit 0
if [ -x /etc/init.d/mimo_desktop_bridge ]; then
	/etc/init.d/mimo_desktop_bridge stop 2>/dev/null
	/etc/init.d/mimo_desktop_bridge disable 2>/dev/null
fi
exit 0
EOF

cat > "$SCRIPTS/post-deinstall" <<'EOF'
#!/bin/sh
rm -f /tmp/luci-indexcache
rm -rf /tmp/luci-modulecache
killall -HUP rpcd 2>/dev/null
exit 0
EOF

chmod 0755 "$SCRIPTS"/*

apk_has_mkpkg() {
  local bin="$1" out
  [ -x "$bin" ] || return 1
  out="$("$bin" mkpkg 2>&1 || true)"
  case "$out" in
    *"required info field"*|*"--info"*) return 0 ;;
  esac
  return 1
}

resolve_apk() {
  if [ -n "${APK_BIN:-}" ]; then
    if apk_has_mkpkg "$APK_BIN"; then
      echo "$APK_BIN"
      return 0
    fi
    echo "!! APK_BIN=$APK_BIN does not support 'mkpkg'" >&2
    return 1
  fi
  if command -v apk >/dev/null 2>&1 && apk_has_mkpkg "$(command -v apk)"; then
    command -v apk
    return 0
  fi
  return 1
}

strip_xattrs() {
  if command -v xattr >/dev/null 2>&1; then
    xattr -cr "$1" 2>/dev/null || true
  fi
  find "$1" -name '._*' -delete 2>/dev/null || true
}

run_mkpkg_host() {
  local apk_bin="$1"
  strip_xattrs "$DATA"
  strip_xattrs "$SCRIPTS"

  # --info is repeated key:value flags, NOT a file path.
  local runner="$STAGE/run-mkpkg.sh"
  cat > "$runner" <<EOF
#!/bin/sh
set -e
chown -R 0:0 "$DATA" "$SCRIPTS" 2>/dev/null || true
exec "$apk_bin" mkpkg \\
  --xattrs=no \\
  --info "name:${PKG_NAME}" \\
  --info "version:${PKG_VER_FULL}" \\
  --info "description:${PKG_DESC}" \\
  --info "arch:${PKG_ARCH}" \\
  --info "license:MIT" \\
  --info "origin:openwrt/" \\
  --info "maintainer:mimo_desktop_bridge" \\
  --info "depends:luci-base" \\
  --info "tags:openwrt:section=luci" \\
  --script "post-install:${SCRIPTS}/post-install" \\
  --script "post-upgrade:${SCRIPTS}/post-upgrade" \\
  --script "pre-deinstall:${SCRIPTS}/pre-deinstall" \\
  --script "post-deinstall:${SCRIPTS}/post-deinstall" \\
  --files "$DATA" \\
  --output "$OUT"
EOF
  chmod 0755 "$runner"

  if [ "$(id -u)" -eq 0 ]; then
    "$runner"
  elif command -v fakeroot >/dev/null 2>&1; then
    if ! fakeroot -- "$runner"; then
      echo "==> fakeroot mkpkg failed; retrying without fakeroot" >&2
      "$runner"
    fi
  else
    echo "==> note: no root/fakeroot; package metadata may keep host uid/gid" >&2
    "$runner"
  fi
}

run_mkpkg_docker() {
  local image="${APK_IMAGE:-alpine:edge}"
  command -v docker >/dev/null 2>&1 || return 1
  docker info >/dev/null 2>&1 || return 1

  echo "==> host apk mkpkg not found; using docker image ${image}"
  local docker_out="/work/out/${PKG_NAME}-${PKG_VER_FULL}.apk"
  mkdir -p "$STAGE/out"
  strip_xattrs "$DATA"
  strip_xattrs "$SCRIPTS"
  docker run --rm -v "$STAGE:/work" -w /work "$image" sh -c "
    set -e
    apk add --no-cache apk-tools >/dev/null
    chown -R 0:0 /work/data /work/scripts
    apk mkpkg \
      --xattrs=no \
      --info 'name:${PKG_NAME}' \
      --info 'version:${PKG_VER_FULL}' \
      --info 'description:${PKG_DESC}' \
      --info 'arch:${PKG_ARCH}' \
      --info 'license:MIT' \
      --info 'origin:openwrt/' \
      --info 'maintainer:mimo_desktop_bridge' \
      --info 'depends:luci-base' \
      --info 'tags:openwrt:section=luci' \
      --script 'post-install:/work/scripts/post-install' \
      --script 'post-upgrade:/work/scripts/post-upgrade' \
      --script 'pre-deinstall:/work/scripts/pre-deinstall' \
      --script 'post-deinstall:/work/scripts/post-deinstall' \
      --files /work/data \
      --output '${docker_out}'
  "
  cp -f "$STAGE/out/${PKG_NAME}-${PKG_VER_FULL}.apk" "$OUT"
}

if APK_RESOLVED="$(resolve_apk)"; then
  run_mkpkg_host "$APK_RESOLVED"
elif run_mkpkg_docker; then
  :
else
  cat >&2 <<'ERR'
!! cannot build APKv3: no `apk mkpkg` available.

OpenWrt 25.12+ requires APKv3 from `apk mkpkg`. Install apk-tools v3
(meson -Dminimal=false) and set APK_BIN, or install Docker for alpine:edge.
Do NOT hand-craft APKv2 — OpenWrt rejects it.
ERR
  exit 1
fi

test -f "$OUT" || { echo "!! apk not produced: $OUT" >&2; exit 1; }
echo "==> apk: $OUT"
ls -lh "$OUT"
echo "    arch=${PKG_ARCH}  version=${PKG_VER_FULL}"
