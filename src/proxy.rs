//! OpenAI-compatible proxy to the MiMo free channel.

use crate::auth::API_BASE;
use crate::state::BridgeState;
use crate::upstream::{send_chat, SendError};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

/// Free-channel models observed from the desktop catalog + aliases.
pub fn known_models() -> Vec<Value> {
    [
        ("mimo-x-pro-preview", "MiMo-X-Pro-Preview"),
        ("mimo-x-flash-preview", "MiMo-X-Flash-Preview"),
        ("mimo-pro", "MiMo Pro"),
        ("mimo-flash", "MiMo Flash"),
        ("mimo-auto", "MiMo Auto"),
    ]
    .into_iter()
    .map(|(id, name)| {
        json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": "xiaomi",
            "name": name,
        })
    })
    .collect()
}

pub async fn models(State(state): State<Arc<BridgeState>>) -> Response {
    let _ = &state;
    Json(json!({
        "object": "list",
        "data": known_models(),
    }))
    .into_response()
}

pub fn error_response(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code,
            }
        })),
    )
        .into_response()
}

pub async fn chat(State(state): State<Arc<BridgeState>>, body: String) -> Response {
    let mut payload: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    if !payload.is_object() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "body must be a JSON object",
            "invalid_request_error",
        );
    }
    if payload.get("n").and_then(|v| v.as_i64()).unwrap_or(1) > 1 {
        payload["n"] = json!(1);
    }
    let stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let payload_model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let started = std::time::Instant::now();
    state.emit_log(json!({
        "ts": chrono::Utc::now().timestamp_millis(),
        "kind": "request",
        "path": "/v1/chat/completions",
        "model": payload_model,
        "stream": stream,
    }));

    let resp = match send_chat(&state, &payload).await {
        Ok(r) => r,
        Err(SendError::NotLoggedIn) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "not logged in — open the WebUI and sign in with a Xiaomi account",
                "invalid_api_key",
            )
        }
        Err(SendError::Upstream(e)) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream fetch failed: {e}"),
                "api_error",
            )
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    if !resp.status().is_success() && !stream {
        let text = resp.text().await.unwrap_or_default();
        state.usage.record(&payload_model, 0, 0, true);
        state.emit_log(json!({
            "ts": chrono::Utc::now().timestamp_millis(),
            "kind": "error",
            "path": "/v1/chat/completions",
            "status": status.as_u16(),
            "message": text.chars().take(200).collect::<String>(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }));
        return error_response(
            status,
            &format!(
                "upstream {}: {}",
                status.as_u16(),
                text.chars().take(400).collect::<String>()
            ),
            "api_error",
        );
    }

    state.emit_log(json!({
        "ts": chrono::Utc::now().timestamp_millis(),
        "kind": "response",
        "path": "/v1/chat/completions",
        "status": status.as_u16(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    }));

    // Stream body through
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        content_type
            .parse()
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/json")),
    );
    if stream {
        headers.insert(
            header::HeaderName::from_static("x-accel-buffering"),
            header::HeaderValue::from_static("no"),
        );
    }

    // Count a successful request now; token totals are parsed from the stream below.
    state.usage.record(&payload_model, 0, 0, false);

    let usage = state.usage.clone();
    let model_for_usage = payload_model.clone();
    let upstream = resp.bytes_stream();
    let counted = futures_util::stream::unfold(
        (
            upstream,
            Vec::<u8>::new(),
            0u64,
            0u64,
            usage,
            model_for_usage,
        ),
        |(mut up, mut buf, mut prompt_toks, mut comp_toks, usage, model)| async move {
            loop {
                if let Some(line) = crate::upstream::take_sse_line(&mut buf) {
                    let line = line.trim().to_string();
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            if prompt_toks > 0 || comp_toks > 0 {
                                usage.record_tokens(&model, prompt_toks, comp_toks);
                            }
                            let chunk = Bytes::from("data: [DONE]\n\n");
                            return Some((
                                Ok::<Bytes, std::io::Error>(chunk),
                                (up, buf, prompt_toks, comp_toks, usage, model),
                            ));
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(u) = v.get("usage") {
                                prompt_toks = u
                                    .get("prompt_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(prompt_toks);
                                comp_toks = u
                                    .get("completion_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(comp_toks);
                            }
                        }
                        let out = format!("data: {data}\n\n");
                        return Some((
                            Ok(Bytes::from(out)),
                            (up, buf, prompt_toks, comp_toks, usage, model),
                        ));
                    }
                    if line.is_empty() {
                        continue;
                    }
                    // Pass through non-data SSE lines (event:, id:, etc.)
                    return Some((
                        Ok(Bytes::from(format!("{line}\n"))),
                        (up, buf, prompt_toks, comp_toks, usage, model),
                    ));
                }
                match up.next().await {
                    Some(Ok(b)) => buf.extend_from_slice(&b),
                    Some(Err(e)) => {
                        usage.record(&model, prompt_toks, comp_toks, true);
                        return Some((
                            Err(std::io::Error::other(e.to_string())),
                            (up, buf, prompt_toks, comp_toks, usage, model),
                        ));
                    }
                    None => {
                        if prompt_toks > 0 || comp_toks > 0 {
                            usage.record_tokens(&model, prompt_toks, comp_toks);
                        }
                        return None;
                    }
                }
            }
        },
    );

    let body = Body::from_stream(counted);
    (status, headers, body).into_response()
}

/// GET /api/proxy/status
pub async fn proxy_status(State(state): State<Arc<BridgeState>>) -> Response {
    let session = state.storage.session();
    let (me_status, logged_in, preview) = match &session {
        Some(s) if s.pass_token.is_some() || s.service_token.is_some() => {
            match crate::auth::probe_me(&state.http, s).await {
                Ok((st, li, p)) => (Some(st), li, Some(p)),
                Err(_) => (None, s.is_authenticated(), None),
            }
        }
        _ => (None, false, None),
    };
    Json(json!({
        "loggedIn": session.as_ref().map(|s| s.is_authenticated()).unwrap_or(false),
        "userId": session.as_ref().and_then(|s| s.user_id.clone()),
        "nick": session.as_ref().and_then(|s| s.nick.clone()),
        "hasPassToken": session.as_ref().and_then(|s| s.pass_token.as_ref().map(|_| true)).unwrap_or(false),
        "hasServiceToken": session.as_ref().and_then(|s| s.service_token.as_ref().map(|_| true)).unwrap_or(false),
        "apiBase": API_BASE,
        "sid": crate::auth::SID,
        "meStatus": me_status,
        "meLoggedIn": logged_in,
        "mePreview": preview,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_shape() {
        let models = known_models();
        assert!(models.len() >= 5);
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"mimo-x-pro-preview"));
        assert!(ids.contains(&"mimo-auto"));
        for m in &models {
            assert_eq!(m["object"], "model");
            assert_eq!(m["owned_by"], "xiaomi");
        }
    }

    #[test]
    fn error_response_shape() {
        let resp = error_response(StatusCode::BAD_REQUEST, "boom", "bad");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
