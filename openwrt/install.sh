#!/bin/sh
# On-router installer for the mimo_desktop_bridge OpenWrt bundle.
#
# Extract a bundle that contains:
#   ./mimo_desktop_bridge  ./files/  ./luci/
# then run:  sh install.sh
set -e

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN_SRC="$HERE/mimo_desktop_bridge"
NAME=mimo_desktop_bridge

echo "==> mimo_desktop_bridge OpenWrt installer"
[ -f "$BIN_SRC" ] || { echo "!! missing binary: $BIN_SRC" >&2; exit 1; }

case "$(uname -m)" in
	aarch64|arm64) : ;;
	*) echo "!! warning: router arch is $(uname -m), bundle is aarch64. Continuing anyway." >&2 ;;
esac

echo "==> installing /usr/bin/$NAME"
[ -x /etc/init.d/$NAME ] && /etc/init.d/$NAME stop 2>/dev/null || true
cp -f "$BIN_SRC" /usr/bin/$NAME
chmod +x /usr/bin/$NAME

echo "==> installing init script + UCI config"
cp -f "$HERE/files/etc/init.d/$NAME" /etc/init.d/$NAME
chmod +x /etc/init.d/$NAME
if [ ! -f /etc/config/$NAME ]; then
	cp -f "$HERE/files/etc/config/$NAME" /etc/config/$NAME
else
	echo "   keeping existing /etc/config/$NAME"
fi

echo "==> installing LuCI app"
mkdir -p /www/luci-static/resources/view/mimo-desktop-bridge
cp -f "$HERE/luci/htdocs/luci-static/resources/view/mimo-desktop-bridge/overview.js" \
	/www/luci-static/resources/view/mimo-desktop-bridge/overview.js
mkdir -p /usr/share/luci/menu.d /usr/share/rpcd/acl.d
cp -f "$HERE/luci/root/usr/share/luci/menu.d/luci-app-mimo-desktop-bridge.json" \
	/usr/share/luci/menu.d/luci-app-mimo-desktop-bridge.json
cp -f "$HERE/luci/root/usr/share/rpcd/acl.d/luci-app-mimo-desktop-bridge.json" \
	/usr/share/rpcd/acl.d/luci-app-mimo-desktop-bridge.json

echo "==> enabling + starting service"
/etc/init.d/$NAME enable 2>/dev/null || true
/etc/init.d/$NAME start

echo "==> done. Open http://<router-ip>:8787/"
echo "    Persist session across sysupgrade:"
echo "      echo /etc/$NAME >> /etc/sysupgrade.conf"
