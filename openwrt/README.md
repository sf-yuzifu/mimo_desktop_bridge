# mimo_desktop_bridge on OpenWrt (LuCI 插件)

把 `mimo_desktop_bridge` 的 headless server 跑在路由器上，并在 LuCI 后台里通过
iframe 嵌入自带 WebUI。产品形态对齐 `miclaw_api_bridge/openwrt`。

```
浏览器
  http://192.168.1.1/  →  LuCI (uhttpd, :80)
    └─ 服务 ▸ MiMo Desktop Bridge
         <iframe src="http://192.168.1.1:8787/">
                │
   mimo_desktop_bridge server  (procd 常驻, :8787, 0.0.0.0)
                │
   mimo-server-cn.xiaomimimo.com  (小米账号 sid=mimopc)
```

## 安全提示

- 默认只应在**可信内网**使用；不要把 8787 暴露到 WAN。
- 登录后 session 落在 `/etc/mimo_desktop_bridge/session.json`。
- 建议在 WebUI 里设置 admin password，并按需开启 API Key。

## 交叉编译 aarch64-musl

在 Linux / macOS 开发机上（需要 Docker + rustup target）：

```bash
rustup target add aarch64-unknown-linux-musl
# 或使用 cross
cross build --release --target aarch64-unknown-linux-musl
```

产物：`target/aarch64-unknown-linux-musl/release/mimo_desktop_bridge`

## 手动安装（无包管理器）

```bash
ROUTER=root@192.168.1.1
scp target/aarch64-unknown-linux-musl/release/mimo_desktop_bridge $ROUTER:/usr/bin/
scp openwrt/files/etc/init.d/mimo_desktop_bridge $ROUTER:/etc/init.d/
scp openwrt/files/etc/config/mimo_desktop_bridge $ROUTER:/etc/config/
ssh $ROUTER 'chmod +x /etc/init.d/mimo_desktop_bridge /usr/bin/mimo_desktop_bridge
             /etc/init.d/mimo_desktop_bridge enable
             /etc/init.d/mimo_desktop_bridge start'
```

## LuCI 菜单（可选）

参考 `luci/` 下的 menu.d / acl.d / view 文件，把 iframe 指到 `http://<router-ip>:8787/`。

## UCI 配置

`/etc/config/mimo_desktop_bridge`：

| option | 默认 | 说明 |
|---|---|---|
| enabled | 1 | 是否随开机启动 |
| host | 0.0.0.0 | 监听地址 |
| port | 8787 | WebUI / API 端口 |
| verbose | 0 | debug 日志 |
| tls | 0 | 自动自签 HTTPS |
| tls_cert / tls_key | 空 | 自备 PEM |

## 持久化

把下面一行加进 `/etc/sysupgrade.conf`，升级固件时保留登录态：

```
/etc/mimo_desktop_bridge
```
