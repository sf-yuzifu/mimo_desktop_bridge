//! Xiaomi Passport login + sid=mimopc serviceToken mint.
//! Protocol mirrors miclaw_api_bridge and the working Node prototype.

use crate::error::{BridgeError, Result};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use std::collections::HashMap;

pub const SID: &str = "mimopc";
pub const API_BASE: &str = "https://mimo-server-cn.xiaomimimo.com/api";
pub const CHAT_PATH: &str = "/route/chat/completions";
pub const ME_PATH: &str = "/user/xiaomi/me";
pub const SOURCE: &str = "mimocode-cli-free";

const ACCOUNT: &str = "https://account.xiaomi.com";
const AUTH2: &str = "https://account.xiaomi.com/pass/serviceLoginAuth2";
const SERVICE_LOGIN: &str = "https://account.xiaomi.com/pass/serviceLogin";

/// Dalvik UA — works for password / 2FA (same as miclaw).
pub const LOGIN_UA: &str =
    "Dalvik/2.1.0 (Linux; U; Android 16; 2509FPN0BC Build/BP2A.250605.031.A3)";
/// PC-like UA for STS / business hops.
pub const PC_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) XiaomiMiMo/1.0 Chrome/144.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub user_id: Option<String>,
    pub c_user_id: Option<String>,
    pub pass_token: Option<String>,
    pub ssecurity: Option<String>,
    pub service_token: Option<String>,
    pub nick: Option<String>,
    pub refreshed_at: Option<i64>,
}

impl Session {
    pub fn is_authenticated(&self) -> bool {
        self.service_token.is_some()
    }

    pub fn business_cookie(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = &self.service_token {
            parts.push(format!("serviceToken={t}"));
            parts.push(format!("{SID}_serviceToken={t}"));
        }
        if let Some(u) = &self.user_id {
            parts.push(format!("userId={u}"));
        }
        if let Some(c) = &self.c_user_id {
            parts.push(format!("cUserId={c}"));
        }
        parts.join("; ")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoginRequest {
    pub account: String,
    pub password: String,
    #[serde(default)]
    pub captcha: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LoginOutcome {
    Authenticated {
        nick: Option<String>,
        user_id: Option<String>,
    },
    TwoFactorRequired {
        options: Vec<i32>,
    },
    CaptchaRequired {
        captcha_url: String,
    },
    Failed {
        code: i64,
        description: String,
    },
}

/// In-flight 2FA context shared across send/verify.
#[derive(Debug, Clone, Default)]
pub struct TwoFactorFlow {
    pub options: Vec<i32>,
    pub account: String,
    pub password_hash: String,
    /// Cookie header accumulated from auth2 + identity/list.
    pub cookie_header: String,
}

pub fn md5_upper(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    hex::encode_upper(hasher.finalize())
}

pub fn strip_prefix(body: &str) -> &str {
    body.trim_start_matches("&&&START&&&")
}

/// Extract a large integer (nonce) from raw JSON text — JSON.parse loses precision.
fn extract_raw_number(json_str: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let pos = json_str.find(&pat)?;
    let after = &json_str[pos + pat.len()..];
    let colon = after.find(':')?;
    let tail = after[colon + 1..].trim_start();
    let bytes = tail.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut end = 0;
    if bytes[0] == b'-' {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 || (end == 1 && bytes[0] == b'-') {
        return None;
    }
    Some(tail[..end].to_string())
}

fn parse_session_fields(body: &serde_json::Value) -> Session {
    let pluck = |k: &str| -> Option<String> {
        match body.get(k) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    };
    let mut nick = pluck("nick");
    if nick.as_deref().map(str::is_empty).unwrap_or(true) {
        nick = pluck("nickName");
    }
    Session {
        user_id: pluck("userId"),
        c_user_id: pluck("cUserId"),
        pass_token: pluck("passToken"),
        ssecurity: pluck("ssecurity"),
        nick,
        service_token: None,
        refreshed_at: None,
    }
}

/// Parse `a=1; b=2` into a map.
pub fn parse_cookie_header(header: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in header.split(';') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

/// Keep only bytes that are legal in an HTTP header value (visible ASCII + tab).
fn sanitize_header_text(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let u = *c as u32;
            (0x20..=0x7E).contains(&u) || *c == '\t'
        })
        .collect()
}

/// Merge Set-Cookie headers into an existing Cookie header string.
/// Values are sanitized — Xiaomi sometimes sets cookies with bytes that
/// would make reqwest's HeaderValue builder fail ("http: builder error").
pub fn merge_set_cookie(cookie_header: &str, set_cookies: &[String]) -> String {
    let mut map = parse_cookie_header(cookie_header);
    for raw in set_cookies {
        let head = raw.split(';').next().unwrap_or("").trim();
        if let Some((k, v)) = head.split_once('=') {
            let name = sanitize_header_text(k.trim());
            let value = sanitize_header_text(v.trim());
            if name.is_empty() {
                continue;
            }
            // Skip deleted cookies
            if value.is_empty() || value.eq_ignore_ascii_case("deleted") {
                map.remove(&name);
            } else {
                map.insert(name, value);
            }
        }
    }
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Build a Cookie header that is guaranteed to be a valid HeaderValue.
pub fn safe_cookie_header(cookie: &str) -> Option<String> {
    let s = sanitize_header_text(cookie);
    if s.trim().is_empty() {
        return None;
    }
    // Final check: no CR/LF/NUL that sanitize might have missed via tab-only
    if s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return None;
    }
    Some(s)
}

fn set_cookies_from_headers(headers: &reqwest::header::HeaderMap) -> Vec<String> {
    headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
        .collect()
}

fn session_from_cookies(cookie_header: &str) -> Session {
    let map = parse_cookie_header(cookie_header);
    let pick = |names: &[&str]| -> Option<String> {
        names.iter().find_map(|n| map.get(*n).cloned()).filter(|s| !s.is_empty())
    };
    Session {
        pass_token: pick(&["passToken"]),
        user_id: pick(&["userId"]),
        c_user_id: pick(&["cUserId"]),
        ssecurity: pick(&["ssecurity", "sSecurity"]),
        service_token: pick(&["serviceToken", &format!("{SID}_serviceToken")]),
        nick: None,
        refreshed_at: None,
    }
}

fn merge_session(json: Session, cookie_header: &str) -> Session {
    let from_cookie = session_from_cookies(cookie_header);
    Session {
        user_id: json.user_id.or(from_cookie.user_id),
        c_user_id: json.c_user_id.or(from_cookie.c_user_id),
        pass_token: json.pass_token.or(from_cookie.pass_token),
        ssecurity: json.ssecurity.or(from_cookie.ssecurity),
        service_token: json.service_token.or(from_cookie.service_token),
        nick: json.nick,
        refreshed_at: None,
    }
}

/// Non-empty location string, or None.
fn nonempty_loc(s: Option<&str>) -> Option<String> {
    s.map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != "null")
        .map(|s| s.to_string())
}

async fn follow_redirs(
    client: &reqwest::Client,
    url: String,
    cookie_header: &str,
    ua: &str,
) -> Result<(String, reqwest::Response)> {
    let mut url = url.trim().to_string();
    if url.is_empty() {
        return Err(BridgeError::Login("follow_redirs: empty url".into()));
    }
    let mut cookie = cookie_header.to_string();
    for _ in 0..8 {
        let mut req = client.get(&url).header(reqwest::header::USER_AGENT, ua);
        if let Some(c) = safe_cookie_header(&cookie) {
            req = req.header(reqwest::header::COOKIE, c);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| BridgeError::Login(format!("follow_redirs GET {url}: {e}")))?;
        cookie = merge_set_cookie(&cookie, &set_cookies_from_headers(resp.headers()));
        let status = resp.status();
        if status.is_redirection() {
            let loc = nonempty_loc(
                resp.headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
            );
            if let Some(loc) = loc {
                url = url::Url::parse(&url)
                    .ok()
                    .and_then(|base| base.join(&loc).ok())
                    .map(|u| u.to_string())
                    .unwrap_or(loc);
                continue;
            }
        }
        return Ok((cookie, resp));
    }
    Err(BridgeError::Login("too many redirects".into()))
}

/// POST /pass/serviceLoginAuth2
pub async fn login_auth2(
    client: &reqwest::Client,
    req: &LoginRequest,
) -> Result<(LoginOutcome, Option<TwoFactorFlow>, Option<Session>)> {
    if req.account.is_empty() || req.password.is_empty() {
        return Err(BridgeError::Login("empty credentials".into()));
    }
    let hash = md5_upper(&req.password);
    let mut form = vec![
        ("user", req.account.clone()),
        ("hash", hash.clone()),
        ("sid", SID.to_string()),
        ("_json", "true".into()),
        ("_locale", "zh_CN".into()),
    ];
    if let Some(c) = req.captcha.as_deref().filter(|s| !s.is_empty()) {
        form.push(("captCode", c.to_string()));
    }

    let resp = {
        let r = client
            .post(AUTH2)
            .header(reqwest::header::USER_AGENT, LOGIN_UA)
            .form(&form);
        r.send()
            .await
            .map_err(|e| BridgeError::Login(format!("auth2: {e}")))?
    };
    // If auth2 itself redirects, walk the chain first (cookies land on hops).
    let (cookie_from_auth2, resp) = if resp.status().is_redirection() {
        let loc = nonempty_loc(
            resp.headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
        );
        let c0 = merge_set_cookie("", &set_cookies_from_headers(resp.headers()));
        match loc {
            Some(loc) => {
                let (c, r) = follow_redirs(client, loc, &c0, LOGIN_UA).await?;
                (c, r)
            }
            None => (c0, resp),
        }
    } else {
        (String::new(), resp)
    };
    let mut cookie = merge_set_cookie(&cookie_from_auth2, &set_cookies_from_headers(resp.headers()));
    let text = resp.text().await?;
    let body: serde_json::Value = serde_json::from_str(strip_prefix(&text)).map_err(|e| {
        BridgeError::Login(format!(
            "auth2 parse: {e} body[..200]={}",
            text.chars().take(200).collect::<String>()
        ))
    })?;

    if let Some(captcha_url) = body.get("captchaUrl").and_then(|v| v.as_str()) {
        if !captcha_url.is_empty() {
            return Ok((
                LoginOutcome::CaptchaRequired {
                    captcha_url: captcha_url.to_string(),
                },
                None,
                None,
            ));
        }
    }

    let notification_url = body
        .get("notificationUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "null");
    if let Some(url) = notification_url {
        let list_url = url
            .replace("fe/service/identity/authStart", "identity/list")
            .replace("fe/service/identityauthStart", "identity/list");
        let list_resp = {
            let mut r = client
                .get(&list_url)
                .header(reqwest::header::USER_AGENT, LOGIN_UA);
            if let Some(c) = safe_cookie_header(&cookie) {
                r = r.header(reqwest::header::COOKIE, c);
            }
            let resp = r
                .send()
                .await
                .map_err(|e| BridgeError::Login(format!("identity/list: {e}")))?;
            if resp.status().is_redirection() {
                let loc = nonempty_loc(
                    resp.headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok()),
                );
                cookie = merge_set_cookie(&cookie, &set_cookies_from_headers(resp.headers()));
                match loc {
                    Some(loc) => {
                        let (c, r) = follow_redirs(client, loc, &cookie, LOGIN_UA).await?;
                        cookie = c;
                        r
                    }
                    None => resp,
                }
            } else {
                resp
            }
        };
        cookie = merge_set_cookie(&cookie, &set_cookies_from_headers(list_resp.headers()));
        let list_text = list_resp.text().await?;
        let list_json: serde_json::Value = serde_json::from_str(strip_prefix(&list_text))
            .map_err(|e| BridgeError::Login(format!("identity/list parse: {e}")))?;
        if list_json
            .get("twoFactorAuth")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(BridgeError::Login(
                "hardware 2FA is not supported".into(),
            ));
        }
        let options: Vec<i32> = list_json
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_i64().map(|n| n as i32))
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return Err(BridgeError::Login("identity/list returned no options".into()));
        }
        let flow = TwoFactorFlow {
            options: options.clone(),
            account: req.account.clone(),
            password_hash: hash,
            cookie_header: cookie,
        };
        return Ok((LoginOutcome::TwoFactorRequired { options }, Some(flow), None));
    }

    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let description = body
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok((LoginOutcome::Failed { code, description }, None, None));
    }

    let json_session = parse_session_fields(&body);
    let session = merge_session(json_session, &cookie);
    let nick = session.nick.clone();
    let user_id = session.user_id.clone();
    Ok((
        LoginOutcome::Authenticated { nick, user_id },
        None,
        Some(session),
    ))
}

pub fn two_factor_paths(flag: i32) -> (&'static str, &'static str) {
    if flag == 4 {
        (
            "/identity/auth/sendPhoneTicket",
            "/identity/auth/verifyPhone",
        )
    } else {
        (
            "/identity/auth/sendEmailTicket",
            "/identity/auth/verifyEmail",
        )
    }
}

/// Send 2FA ticket (SMS/email). Reuses cookie jar from login_auth2.
pub async fn send_ticket(
    client: &reqwest::Client,
    flow: &TwoFactorFlow,
    flag: i32,
) -> Result<(bool, String)> {
    let (send_path, _) = two_factor_paths(flag);
    let url = format!("{ACCOUNT}{send_path}?_dc={}", chrono::Utc::now().timestamp_millis());
    let mut req = client
        .post(&url)
        .header(reqwest::header::USER_AGENT, LOGIN_UA)
        .form(&[("_json", "true"), ("retry", "0"), ("icode", "")]);
    if let Some(c) = safe_cookie_header(&flow.cookie_header) {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| BridgeError::Login(format!("sendTicket: {e}")))?;
    let next_cookie = merge_set_cookie(&flow.cookie_header, &set_cookies_from_headers(resp.headers()));
    let text = resp.text().await?;
    let body: serde_json::Value = serde_json::from_str(strip_prefix(&text))
        .map_err(|e| BridgeError::Login(format!("sendTicket parse: {e}")))?;
    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 0 {
        return Ok((true, next_cookie));
    }
    let tips = body
        .get("tips")
        .or_else(|| body.get("desc"))
        .or_else(|| body.get("description"))
        .and_then(|v| v.as_str())
        .unwrap_or("send failed");
    Ok((false, format!("{tips} (code={code})")))
}

/// Verify 2FA ticket then replay serviceLoginAuth2; harvest passToken from JSON + cookies.
pub async fn verify_ticket(
    client: &reqwest::Client,
    flow: &TwoFactorFlow,
    flag: i32,
    ticket: &str,
    cookie_header: &str,
) -> Result<Session> {
    let (_, verify_path) = two_factor_paths(flag);
    let url = format!("{ACCOUNT}{verify_path}?_dc={}", chrono::Utc::now().timestamp_millis());
    let mut req = client
        .post(&url)
        .header(reqwest::header::USER_AGENT, LOGIN_UA)
        .form(&[
            ("_flag", flag.to_string()),
            ("ticket", ticket.to_string()),
            ("trust", "true".into()),
            ("_json", "true".into()),
        ]);
    if let Some(c) = safe_cookie_header(cookie_header) {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| BridgeError::Login(format!("verifyTicket: {e}")))?;
    let cookie0 = merge_set_cookie(cookie_header, &set_cookies_from_headers(resp.headers()));
    // verifyPhone sometimes 302s — walk the chain so identity cookies land
    let (mut cookie, resp) = if resp.status().is_redirection() {
        let loc = nonempty_loc(
            resp.headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
        );
        match loc {
            Some(loc) => follow_redirs(client, loc, &cookie0, LOGIN_UA).await?,
            None => (cookie0, resp),
        }
    } else {
        (cookie0, resp)
    };
    let text = resp.text().await?;
    let body: serde_json::Value = serde_json::from_str(strip_prefix(&text))
        .map_err(|e| BridgeError::Login(format!("verifyTicket parse: {e}")))?;
    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code == 70014 || code == 7014 {
        return Err(BridgeError::VerificationCode);
    }
    if code != 0 {
        let description = body
            .get("description")
            .or_else(|| body.get("tips"))
            .and_then(|v| v.as_str())
            .unwrap_or("verify failed")
            .to_string();
        return Err(BridgeError::Login(format!("verify code={code} desc={description}")));
    }

    if let Some(loc) = nonempty_loc(body.get("location").and_then(|v| v.as_str())) {
        let (c, _) = follow_redirs(client, loc, &cookie, LOGIN_UA).await?;
        cookie = c;
    }

    // Replay auth2 with trusted identity_session
    let form = [
        ("user", flow.account.clone()),
        ("hash", flow.password_hash.clone()),
        ("sid", SID.to_string()),
        ("_json", "true".into()),
        ("_locale", "zh_CN".into()),
    ];
    let auth2 = {
        let mut r = client
            .post(AUTH2)
            .header(reqwest::header::USER_AGENT, LOGIN_UA)
            .form(&form);
        if let Some(c) = safe_cookie_header(&cookie) {
            r = r.header(reqwest::header::COOKIE, c);
        }
        let resp = r
            .send()
            .await
            .map_err(|e| BridgeError::Login(format!("post-2fa auth2: {e}")))?;
        if resp.status().is_redirection() {
            let loc = nonempty_loc(
                resp.headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
            );
            cookie = merge_set_cookie(&cookie, &set_cookies_from_headers(resp.headers()));
            match loc {
                Some(loc) => {
                    let (c, r) = follow_redirs(client, loc, &cookie, LOGIN_UA).await?;
                    cookie = c;
                    r
                }
                None => resp,
            }
        } else {
            resp
        }
    };
    cookie = merge_set_cookie(&cookie, &set_cookies_from_headers(auth2.headers()));
    let auth2_loc = nonempty_loc(
        auth2
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
    );
    let auth2_text = auth2.text().await?;
    let auth2_json: serde_json::Value = match serde_json::from_str(strip_prefix(&auth2_text)) {
        Ok(v) => v,
        Err(_) => {
            if let Some(loc) = auth2_loc.clone() {
                let (c, r) = follow_redirs(client, loc, &cookie, LOGIN_UA).await?;
                cookie = c;
                let t = r.text().await?;
                serde_json::from_str(strip_prefix(&t))
                    .map_err(|e| BridgeError::Login(format!("post-2fa auth2 parse: {e}")))?
            } else {
                return Err(BridgeError::Login("post-2fa auth2 parse fail".into()));
            }
        }
    };

    let auth2_code = auth2_json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let json_session = parse_session_fields(&auth2_json);
    let mut session = merge_session(json_session, &cookie);

    if auth2_code != 0 && session.pass_token.is_none() {
        let description = auth2_json
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("post-2fa auth2 failed")
            .to_string();
        return Err(BridgeError::Login(format!(
            "post-2fa auth2 code={auth2_code} desc={description}"
        )));
    }

    // Follow auth2 location chain — often the hop that Set-Cookies passToken
    if let Some(loc) = auth2_loc.or_else(|| {
        nonempty_loc(
            auth2_json
                .get("location")
                .and_then(|v| v.as_str()),
        )
    }) {
        let (c, _) = follow_redirs(client, loc, &cookie, LOGIN_UA).await?;
        cookie = c;
        session = merge_session(session, &cookie);
    }

    if session.pass_token.is_none() {
        let keys: Vec<String> = match &auth2_json {
            serde_json::Value::Object(m) => m.keys().cloned().collect(),
            _ => vec!["<non-object>".into()],
        };
        let cookie_names: Vec<String> = cookie
            .split(';')
            .filter_map(|s| s.split_once('=').map(|(k, _)| k.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        let body_preview: String = auth2_text.chars().take(180).collect();
        return Err(BridgeError::Login(format!(
            "auth2 ok but no passToken (keys=[{}] cookies=[{}] body={})",
            keys.join(","),
            cookie_names.join(","),
            body_preview
        )));
    }
    Ok(session)
}

fn stable_device_id(user_id: Option<&str>) -> String {
    let seed = user_id.unwrap_or("anonymous");
    let mut hasher = Md5::new();
    hasher.update(format!("mimo-desktop-bridge:{seed}").as_bytes());
    format!("pc_{}", hex::encode(hasher.finalize())[..16].to_string())
}

fn sha1_base64(s: &str) -> String {
    use base64::Engine;
    let mut hasher = Sha1::new();
    hasher.update(s.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

fn url_form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pick_service_token(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for raw in set_cookies_from_headers(headers) {
        let head = raw.split(';').next().unwrap_or("").trim();
        for prefix in [format!("serviceToken="), format!("{SID}_serviceToken=")] {
            if let Some(v) = head.strip_prefix(&prefix) {
                if !v.is_empty() && v != "EXPIRED" {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Mint sid=SID serviceToken via phase1/phase2, with WebView-style follow-chain fallback.
pub async fn mint_service_token(client: &reqwest::Client, session: &mut Session) -> Result<()> {
    let pass_token = session
        .pass_token
        .clone()
        .ok_or_else(|| BridgeError::Login("missing passToken".into()))?;
    let device_id = stable_device_id(session.user_id.as_deref());
    let mut cookie = format!("passToken={pass_token}");
    if let Some(u) = &session.user_id {
        cookie.push_str(&format!("; userId={u}"));
    }
    if let Some(c) = &session.c_user_id {
        cookie.push_str(&format!("; cUserId={c}"));
    }
    cookie.push_str(&format!(
        "; deviceId={device_id}; uLocale=zh_CN; pass_ua=pc"
    ));

    // Phase 1
    let phase1_url = format!(
        "{SERVICE_LOGIN}?_locale=zh_CN&_snsNone=true&sid={SID}&_json=true"
    );
    let p1 = {
        let mut r = client
            .get(&phase1_url)
            .header(reqwest::header::USER_AGENT, PC_UA);
        if let Some(c) = safe_cookie_header(&cookie) {
            r = r.header(reqwest::header::COOKIE, c);
        }
        r.send()
            .await
            .map_err(|e| BridgeError::Login(format!("phase1: {e}")))?
    };
    let p1_text = p1.text().await?;
    let p1_json: serde_json::Value = serde_json::from_str(strip_prefix(&p1_text))
        .map_err(|e| BridgeError::Login(format!("phase1 parse: {e}")))?;
    let p1_code = p1_json.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if p1_code != 0 {
        let desc = p1_json
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Err(BridgeError::Login(format!(
            "phase1 code={p1_code} desc={desc}"
        )));
    }
    let location = p1_json
        .get("location")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BridgeError::Login("phase1 missing location".into()))?
        .to_string();
    let ssecurity = p1_json
        .get("ssecurity")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let nonce = extract_raw_number(strip_prefix(&p1_text), "nonce")
        .ok_or_else(|| BridgeError::Login("phase1 missing nonce".into()))?;

    if session.user_id.is_none() {
        if let Some(u) = p1_json.get("userId") {
            session.user_id = match u {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            };
        }
    }
    if session.c_user_id.is_none() {
        if let Some(c) = p1_json.get("cUserId").and_then(|v| v.as_str()) {
            session.c_user_id = Some(c.to_string());
        }
    }
    if !ssecurity.is_empty() {
        session.ssecurity = Some(ssecurity.clone());
    }

    // Phase 2
    let sig_input = if ssecurity.trim().is_empty() {
        format!("nonce={nonce}")
    } else {
        format!("nonce={nonce}&{ssecurity}")
    };
    let signature = url_form_encode(&sha1_base64(&sig_input));
    let sep = if location.contains('?') { '&' } else { '?' };
    let phase2_url = format!("{location}{sep}clientSign={signature}");

    let p2 = client
        .get(&phase2_url)
        .header(reqwest::header::USER_AGENT, PC_UA)
        .send()
        .await?;
    let mut token = pick_service_token(p2.headers());
    if token.is_none() && p2.status().is_redirection() {
        if let Some(loc) = p2
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        {
            let abs = url::Url::parse(&phase2_url)
                .ok()
                .and_then(|b| b.join(loc).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| loc.to_string());
            let p3 = client
                .get(&abs)
                .header(reqwest::header::USER_AGENT, PC_UA)
                .send()
                .await?;
            token = pick_service_token(p3.headers());
            if token.is_none() && p3.status().is_redirection() {
                if let Some(loc3) = p3
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                {
                    let abs3 = url::Url::parse(&abs)
                        .ok()
                        .and_then(|b| b.join(loc3).ok())
                        .map(|u| u.to_string())
                        .unwrap_or_else(|| loc3.to_string());
                    let p4 = client
                        .get(&abs3)
                        .header(reqwest::header::USER_AGENT, PC_UA)
                        .send()
                        .await?;
                    token = pick_service_token(p4.headers());
                }
            }
        }
    }

    if token.is_none() {
        // WebView-style fallback: /user/xiaomi/me → passport → /api/sts
        let start = format!("{API_BASE}{ME_PATH}");
        let (c, _) = follow_redirs(client, start, &cookie, PC_UA).await?;
        let map = parse_cookie_header(&c);
        token = map
            .get("serviceToken")
            .or_else(|| map.get(&format!("{SID}_serviceToken")))
            .cloned()
            .filter(|t| !t.is_empty() && t != "EXPIRED");
    }

    let token = token.ok_or_else(|| {
        BridgeError::Login("phase2 returned no serviceToken".into())
    })?;
    session.service_token = Some(token);
    session.refreshed_at = Some(chrono::Utc::now().timestamp_millis());
    Ok(())
}

/// GET /user/xiaomi/me with session cookies.
pub async fn probe_me(client: &reqwest::Client, session: &Session) -> Result<(u16, bool, String)> {
    let url = format!("{API_BASE}{ME_PATH}");
    let resp = {
        let mut r = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, PC_UA)
            .header("X-Mimo-Source", SOURCE);
        if let Some(c) = safe_cookie_header(&session.business_cookie()) {
            r = r.header(reqwest::header::COOKIE, c);
        }
        r.send()
            .await
            .map_err(|e| BridgeError::Login(format!("probe_me: {e}")))?
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let logged_in = serde_json::from_str::<serde_json::Value>(strip_prefix(&text))
        .map(|j| {
            j.get("code").and_then(|v| v.as_i64()) == Some(0)
                && j.pointer("/data/userId").is_some()
        })
        .unwrap_or(false);
    Ok((status, logged_in, text.chars().take(200).collect()))
}
