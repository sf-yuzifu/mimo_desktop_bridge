# mimo_desktop_bridge

Standalone Rust bridge that exposes the **Xiaomi MiMo Desktop free account channel** as a local OpenAI-compatible API — same product shape as `miclaw_api_bridge` (WebUI, optional API keys, Docker), but a **different upstream**.

| | miclaw_api_bridge | mimo_desktop_bridge |
|---|---|---|
| Upstream | Super XiaoAI `api.miclaw.xiaomi.net` | MiMo Desktop `mimo-server-cn.xiaomimimo.com` |
| Passport sid | `miclaw` | `mimopc` |
| Chat path | `/osbot/pc/llm/v2/chat/completions` | `/api/route/chat/completions` |
| Client marker | — | `X-Mimo-Source: mimocode-cli-free` |
| Default port | 8765 | **8787** |

Free-channel models include `mimo-x-pro-preview`, `mimo-x-flash-preview`, `mimo-pro`, `mimo-flash`, `mimo-auto`.

> This uses an **undocumented** account session channel. Prefer the official `https://api.xiaomimimo.com/v1` + API key for production. Personal-use reverse bridge only.

## Quick start

```bash
cargo run --release -- server --open
```

Open `http://127.0.0.1:8787`, sign in with a Xiaomi account (SMS 2FA supported).

OpenAI base URL:

```
http://127.0.0.1:8787/v1
```

### opencode

```jsonc
{
  "provider": {
    "mimo-desktop": {
      "name": "MiMo Desktop Free",
      "npm": "@ai-sdk/openai-compatible",
      "options": {
        "baseURL": "http://127.0.0.1:8787/v1",
        "apiKey": "unused-or-your-mdb-key"
      },
      "models": {
        "mimo-x-pro-preview": { "name": "MiMo-X-Pro-Preview" },
        "mimo-x-flash-preview": { "name": "MiMo-X-Flash-Preview" },
        "mimo-pro": { "name": "MiMo Pro" },
        "mimo-flash": { "name": "MiMo Flash" }
      }
    }
  }
}
```

## CLI

```bash
mimo_desktop_bridge server [--port 8787] [--host 127.0.0.1] [--open] [--config-dir DIR]
mimo_desktop_bridge status
```

Config lives under the OS config dir (`%APPDATA%\mimo\mimo_desktop_bridge` on Windows) unless `--config-dir` is set:

- `settings.json` — port, admin password hash, api-key-required
- `session.json` — Xiaomi session (passToken / serviceToken)
- `api-keys.json` — sha256 hashes + display prefixes only

## HTTP surface

| Method | Path | Notes |
|---|---|---|
| GET | `/` | WebUI |
| GET | `/v1/models` | free-channel model list |
| POST | `/v1/chat/completions` | OpenAI-compatible, SSE passthrough |
| GET | `/api/auth/status` | login state |
| POST | `/api/auth/login` | Xiaomi account + password |
| POST | `/api/auth/two-factor/send` | `{flag:4\|8}` |
| POST | `/api/auth/two-factor/verify` | `{flag,ticket}` |
| POST | `/api/auth/refresh` | re-mint serviceToken |
| POST | `/api/auth/logout` | clear session |
| GET | `/api/proxy/status` | session + `/user/xiaomi/me` probe |
| GET/POST | `/api/keys` | list / create |
| DELETE | `/api/keys/:id` | revoke |
| GET/POST | `/api/settings/api-key-required` | protect `/v1` |

## Docker

```bash
docker build -t mimo_desktop_bridge .
docker run -d --name mdb -p 8787:8787 -v mdb-data:/data mimo_desktop_bridge
```

Open `http://<host>:8787` and sign in. Session persists in the `/data` volume.

## Protocol notes

1. `POST /pass/serviceLoginAuth2` with `sid=mimopc`, password = MD5(upper hex).
2. 2FA: `sendPhoneTicket` / `verifyPhone` (flag 4), then replay auth2.
3. Mint `serviceToken`:  
   `GET /pass/serviceLogin?sid=mimopc&_json=true` (phase1) →  
   `GET <location>&clientSign=<base64(sha1("nonce=N&ssecurity"))>` (phase2, no cookies).
4. Business calls: `Cookie: serviceToken=…; userId=…` + `X-Mimo-Source: mimocode-cli-free`.

`nonce` is parsed from **raw** JSON text (JSON number precision would truncate it).

## Develop / release

```bash
# local check
cargo check --all-targets && cargo test

# stage binaries under release/
scripts/build-binaries.sh

# cross-compile (example)
MDB_RUST_TARGET=aarch64-unknown-linux-musl scripts/build-binaries.sh
```

This repository is **self-contained** (crate + WebUI + Docker + OpenWrt + GitHub Actions live here).

| Workflow | Trigger | Output |
|---|---|---|
| `.github/workflows/ci.yml` | push / PR | `cargo check` + `cargo test` |
| `.github/workflows/release.yml` | tag `v*` / manual | draft Release with macOS / Windows / Linux archives |
| `.github/workflows/openwrt.yml` | tag `v*` / manual | aarch64-musl binary + tar bundle + `.ipk` + `.apk` |

```bash
git tag v0.1.0
git push origin v0.1.0
```

OpenWrt packaging: see `openwrt/README.md` (`build-ipk.sh` / `pack-bundle.sh`).

## Related

- `miclaw_api_bridge` — Super XiaoAI bridge (different upstream; not merged on purpose)

## License

MIT — see [LICENSE](LICENSE). Use at your own risk; respect Xiaomi ToS.
