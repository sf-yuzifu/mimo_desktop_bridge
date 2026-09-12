//! Shared upstream call path used by /v1/chat/completions, /v1/messages and
//! /v1/responses: session lookup, request build, single-flight 401 retry,
//! and byte-safe SSE line extraction.

use crate::auth::{safe_cookie_header, API_BASE, CHAT_PATH, PC_UA, SOURCE};
use crate::state::BridgeState;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug)]
pub enum SendError {
    /// No (or not-yet-authenticated) Xiaomi session on disk.
    NotLoggedIn,
    /// The request to the upstream never completed.
    Upstream(String),
}

/// Refresh tokens older than this proactively so an idle bridge does not
/// pay a 401 round-trip on the first request after a long pause.
const PROACTIVE_REFRESH_AGE_MS: i64 = 24 * 3600 * 1000;

/// POST a chat-completions payload to the MiMo free channel.
///
/// On 401, performs one single-flight token refresh and retries once.
pub async fn send_chat(state: &Arc<BridgeState>, payload: &Value) -> Result<reqwest::Response, SendError> {
    let mut session = match state.storage.session() {
        Some(s) if s.is_authenticated() => s,
        _ => return Err(SendError::NotLoggedIn),
    };
    if let Some(at) = session.refreshed_at {
        let stale = chrono::Utc::now().timestamp_millis() - at > PROACTIVE_REFRESH_AGE_MS;
        if stale {
            // Failure is fine — the old token may still work; a real expiry
            // surfaces as 401 and triggers the retry below.
            if let Ok(s) = state.refresh_session(false).await {
                session = s;
            }
        }
    }
    let stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    send_with_cookie(state, payload, session.business_cookie(), stream).await
}

async fn send_with_cookie(
    state: &Arc<BridgeState>,
    payload: &Value,
    cookie: String,
    stream: bool,
) -> Result<reqwest::Response, SendError> {
    let url = format!("{API_BASE}{CHAT_PATH}");
    let send = |cookie: String| {
        let url = url.clone();
        let payload = payload.clone();
        let client = state.http.clone();
        async move {
            let mut req = client
                .post(&url)
                .header(reqwest::header::USER_AGENT, PC_UA)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header("X-Mimo-Source", SOURCE)
                .header(
                    reqwest::header::ACCEPT,
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(payload.to_string());
            if let Some(c) = safe_cookie_header(&cookie) {
                req = req.header(reqwest::header::COOKIE, c);
            }
            req.send().await
        }
    };

    let mut resp = send(cookie)
        .await
        .map_err(|e| SendError::Upstream(e.to_string()))?;

    // 401 → one refresh attempt (single-flight; losers reuse the minted token)
    if resp.status() == 401 {
        if let Ok(s) = state.refresh_session(false).await {
            if let Ok(r2) = send(s.business_cookie()).await {
                resp = r2;
            }
        }
    }
    Ok(resp)
}

/// Pull one complete line (sans terminator) from a raw byte buffer.
///
/// The buffer stays bytes so a multi-byte UTF-8 character split across TCP
/// chunks is only decoded once the full line has arrived. Returns `None`
/// while no full line is buffered.
pub fn take_sse_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let line_bytes: Vec<u8> = buf.drain(..pos).collect();
    buf.drain(..1);
    let mut line = String::from_utf8_lossy(&line_bytes).into_owned();
    if line.ends_with('\r') {
        line.pop();
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_sse_line_waits_for_full_line() {
        let mut buf = Vec::new();
        assert_eq!(take_sse_line(&mut buf), None);
        buf.extend_from_slice(b"data: par");
        assert_eq!(take_sse_line(&mut buf), None);
        buf.extend_from_slice(b"tial");
        assert_eq!(take_sse_line(&mut buf), None);
        buf.extend_from_slice(b"\n");
        assert_eq!(take_sse_line(&mut buf).as_deref(), Some("data: partial"));
        assert!(buf.is_empty());
    }

    #[test]
    fn take_sse_line_handles_crlf_and_multiple() {
        let mut buf = b"a\r\nb\n".to_vec();
        assert_eq!(take_sse_line(&mut buf).as_deref(), Some("a"));
        assert_eq!(take_sse_line(&mut buf).as_deref(), Some("b"));
        assert_eq!(take_sse_line(&mut buf), None);
    }

    #[test]
    fn take_sse_line_decodes_split_multibyte_only_when_complete() {
        // "中" = E4 B8 AD; split across two chunks must not garble.
        let mut buf = vec![0xE4, 0xB8];
        assert_eq!(take_sse_line(&mut buf), None);
        buf.extend_from_slice(&[0xAD, b'\n']);
        assert_eq!(take_sse_line(&mut buf).as_deref(), Some("中"));
    }
}
