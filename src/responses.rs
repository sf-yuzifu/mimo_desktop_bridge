//! OpenAI Responses API — chat-completions compat for the MiMo free channel.

use crate::auth::{safe_cookie_header, CHAT_PATH, API_BASE, PC_UA, SOURCE};
use crate::proxy::error_response;
use crate::state::BridgeState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

/// `/v1/responses`: try chat-completions compat (MiMo free channel has no native
/// Responses endpoint). Convert Responses body → chat, call upstream, convert back.
pub async fn responses(State(state): State<Arc<BridgeState>>, body: String) -> Response {
    let started = std::time::Instant::now();
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    state.emit_log(json!({
        "ts": chrono::Utc::now().timestamp_millis(),
        "kind": "request",
        "path": "/v1/responses -> /v1/chat/completions",
        "model": model,
        "stream": stream,
    }));

    let chat_body = responses_to_chat_body(&parsed, stream);
    let session = match state.storage.session() {
        Some(s) if s.is_authenticated() => s,
        _ => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "not logged in — open the WebUI and sign in",
                "invalid_api_key",
            )
        }
    };

    let url = format!("{API_BASE}{CHAT_PATH}");
    let send = |cookie: String| {
        let url = url.clone();
        let body = chat_body.clone();
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
                .body(body.to_string());
            if let Some(c) = safe_cookie_header(&cookie) {
                req = req.header(reqwest::header::COOKIE, c);
            }
            req.send().await
        }
    };

    let mut resp = match send(session.business_cookie()).await {
        Ok(r) => r,
        Err(e) => {
            state.emit_log(json!({
                "ts": chrono::Utc::now().timestamp_millis(),
                "kind": "error",
                "path": "/v1/responses",
                "message": e.to_string(),
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }));
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream fetch failed: {e}"),
                "api_error",
            );
        }
    };

    if resp.status() == 401 {
        if let Some(mut s) = state.storage.session() {
            if s.pass_token.is_some()
                && crate::auth::mint_service_token(&state.http, &mut s)
                    .await
                    .is_ok()
            {
                let _ = state.storage.save_session(s.clone());
                if let Ok(r2) = send(s.business_cookie()).await {
                    resp = r2;
                }
            }
        }
    }

    let status = resp.status();
    state.emit_log(json!({
        "ts": chrono::Utc::now().timestamp_millis(),
        "kind": "response",
        "path": "/v1/responses -> /v1/chat/completions",
        "status": status.as_u16(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
    }));

    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return error_response(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            &format!(
                "upstream {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            ),
            "api_error",
        );
    }

    if stream {
        // Stream OpenAI chat SSE through unchanged (client asked for Responses
        // stream — many clients accept chat-shaped SSE when using the compat
        // path; full event-type translation is a follow-up if needed).
        let stream_body = resp.bytes_stream();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            axum::http::header::HeaderName::from_static("x-accel-buffering"),
            axum::http::HeaderValue::from_static("no"),
        );
        state.usage.record(&model, 0, 0, false);
        return (headers, axum::body::Body::from_stream(stream_body)).into_response();
    }

    let chat = match resp.json::<Value>().await {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream parse: {e}"),
                "api_error",
            )
        }
    };
    if let (Some(p), Some(c)) = (
        chat.pointer("/usage/prompt_tokens").and_then(|v| v.as_u64()),
        chat
            .pointer("/usage/completion_tokens")
            .and_then(|v| v.as_u64()),
    ) {
        state.usage.record(&model, p, c, false);
    } else {
        state.usage.record(&model, 0, 0, false);
    }
    Json(response_from_chat(&parsed, &chat)).into_response()
}

/// Responses request body → OpenAI chat.completions body.
pub fn responses_to_chat_body(body: &Value, stream: bool) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "model".into(),
        body.get("model").cloned().unwrap_or_else(|| json!("mimo-flash")),
    );
    out.insert("stream".into(), Value::Bool(stream));
    if stream {
        out.insert(
            "stream_options".into(),
            json!({ "include_usage": true }),
        );
    }
    if let Some(v) = body.get("temperature") {
        out.insert("temperature".into(), v.clone());
    }
    if let Some(v) = body.get("top_p") {
        out.insert("top_p".into(), v.clone());
    }
    if let Some(v) = body.get("max_output_tokens") {
        out.insert("max_tokens".into(), v.clone());
    }
    if let Some(v) = body.get("tool_choice") {
        out.insert("tool_choice".into(), v.clone());
    }
    if let Some(tools) = body.get("tools") {
        out.insert("tools".into(), tools.clone());
    }

    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        let text = match instructions {
            Value::String(s) => Some(s.clone()),
            other => other.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()),
        };
        if let Some(text) = text {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }
    messages.extend(messages_from_input(body.get("input")));
    if messages.is_empty() {
        messages.push(json!({ "role": "user", "content": "" }));
    }
    out.insert("messages".into(), Value::Array(messages));
    Value::Object(out)
}

fn messages_from_input(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(s)) => vec![json!({ "role": "user", "content": s })],
        Some(Value::Array(items)) => items.iter().filter_map(message_from_input_item).collect(),
        Some(other) => {
            if let Some(s) = other.as_str() {
                vec![json!({ "role": "user", "content": s })]
            } else {
                Vec::new()
            }
        }
        None => Vec::new(),
    }
}

fn message_from_input_item(item: &Value) -> Option<Value> {
    if let Some(text) = item.as_str() {
        return Some(json!({ "role": "user", "content": text }));
    }
    let obj = item.as_object()?;
    let typ = obj.get("type").and_then(|v| v.as_str());
    if typ == Some("input_text") {
        return obj
            .get("text")
            .and_then(|v| v.as_str())
            .map(|text| json!({ "role": "user", "content": text }));
    }
    if typ == Some("message") || obj.contains_key("role") {
        let role = obj
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user");
        let content = match obj.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| {
                    p.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        return Some(json!({ "role": role, "content": content }));
    }
    None
}

/// Chat completion JSON → Responses object (text subset).
pub fn response_from_chat(req: &Value, chat: &Value) -> Value {
    let model = req
        .get("model")
        .and_then(|v| v.as_str())
        .or_else(|| chat.get("model").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    let text = chat
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stop = chat
        .pointer("/choices/0/finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop");
    let id = chat
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| format!("resp_{s}"))
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
    let usage = chat.get("usage").cloned().unwrap_or(json!({}));

    json!({
        "id": id,
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": [{
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text,
                "annotations": []
            }]
        }],
        "output_text": text,
        "incomplete_details": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        },
        "metadata": {
            "finish_reason": stop,
            "via": "chat-completions-compat"
        }
    })
}
