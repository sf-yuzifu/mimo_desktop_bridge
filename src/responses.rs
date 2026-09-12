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
use uuid::Uuid;

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
        return responses_stream_from_chat(state, resp, parsed).await;
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

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

/// Translate OpenAI chat-completions SSE into OpenAI Responses SSE events.
async fn responses_stream_from_chat(
    state: Arc<BridgeState>,
    upstream: reqwest::Response,
    request: Value,
) -> Response {
    use bytes::Bytes;
    use futures_util::StreamExt;
    use tokio::sync::mpsc;

    let response_id = new_id("resp");
    let rs_id = new_id("rs");
    let msg_id = new_id("msg");
    let created_at = chrono::Utc::now().timestamp();
    let model = request
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("mimo-flash")
        .to_string();
    let model_json = json!(model);
    let usage_state = state.usage.clone();
    let model_for_usage = model.clone();

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let mut seq = 1_i64;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut usage = Value::Null;
        let mut buffer: Vec<u8> = Vec::new();
        let mut stream = upstream.bytes_stream();
        let mut reasoning_opened = false;
        let mut reasoning_closed = false;
        let mut message_opened = false;
        let mut msg_index = 0_i64;

        async fn send(tx: &mpsc::Sender<Result<Bytes, std::io::Error>>, value: Value) {
            let typ = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("message");
            if let Ok(data) = serde_json::to_string(&value) {
                let frame = format!("event: {typ}\ndata: {data}\n\n");
                let _ = tx.send(Ok(Bytes::from(frame))).await;
            }
        }

        fn resp_event(
            typ: &str,
            seq: i64,
            status: &str,
            id: &str,
            created_at: i64,
            model: &Value,
            output: &[Value],
            output_text: &str,
            usage: &Value,
        ) -> Value {
            json!({
                "type": typ,
                "sequence_number": seq,
                "response": {
                    "id": id,
                    "object": "response",
                    "created_at": created_at,
                    "status": status,
                    "model": model,
                    "output": output,
                    "output_text": output_text,
                    "usage": usage,
                    "error": null,
                    "incomplete_details": null,
                }
            })
        }

        fn close_reasoning(rs_id: &str, reasoning: &str, seq: &mut i64) -> Vec<Value> {
            let mut evts = Vec::new();
            evts.push(json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": rs_id, "output_index": 0, "summary_index": 0,
                "text": reasoning, "sequence_number": *seq,
            }));
            *seq += 1;
            evts.push(json!({
                "type": "response.reasoning_summary_part.done",
                "item_id": rs_id, "output_index": 0, "summary_index": 0,
                "part": {"type": "summary_text", "text": reasoning}, "sequence_number": *seq,
            }));
            *seq += 1;
            evts.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": rs_id, "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": reasoning}],
                    "status": "completed"
                },
                "sequence_number": *seq,
            }));
            *seq += 1;
            evts
        }

        send(
            &tx,
            resp_event(
                "response.created",
                seq,
                "in_progress",
                &response_id,
                created_at,
                &model_json,
                &[],
                "",
                &Value::Null,
            ),
        )
        .await;
        seq += 1;
        send(
            &tx,
            resp_event(
                "response.in_progress",
                seq,
                "in_progress",
                &response_id,
                created_at,
                &model_json,
                &[],
                "",
                &Value::Null,
            ),
        )
        .await;
        seq += 1;

        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                send(
                    &tx,
                    json!({
                        "type": "error",
                        "code": "upstream_stream_error",
                        "message": "upstream stream ended with an error",
                        "sequence_number": seq,
                    }),
                )
                .await;
                return;
            };
            buffer.extend_from_slice(&chunk);
            loop {
                let Some(pos) = buffer.iter().position(|&b| b == b'\n') else {
                    break;
                };
                let line_bytes: Vec<u8> = buffer.drain(..pos).collect();
                buffer.drain(..1);
                let mut line = String::from_utf8_lossy(&line_bytes).into_owned();
                if line.ends_with('\r') {
                    line.pop();
                }
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(u) = value.get("usage") {
                    let p = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let c = u
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    usage = json!({
                        "input_tokens": p,
                        "input_tokens_details": {"cached_tokens": 0},
                        "output_tokens": c,
                        "output_tokens_details": {
                            "reasoning_tokens": if reasoning.is_empty() { 0 } else { c }
                        },
                        "total_tokens": u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(p + c),
                    });
                }
                let choice = value.pointer("/choices/0").cloned().unwrap_or(Value::Null);
                let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

                if let Some(piece) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                {
                    if !reasoning_opened {
                        reasoning_opened = true;
                        send(
                            &tx,
                            json!({
                                "type": "response.output_item.added",
                                "output_index": 0,
                                "item": {
                                    "id": rs_id, "type": "reasoning",
                                    "summary": [], "status": "in_progress"
                                },
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                        send(
                            &tx,
                            json!({
                                "type": "response.reasoning_summary_part.added",
                                "item_id": rs_id,
                                "output_index": 0,
                                "summary_index": 0,
                                "part": {"type": "summary_text", "text": ""},
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                    }
                    reasoning.push_str(piece);
                    send(
                        &tx,
                        json!({
                            "type": "response.reasoning_summary_text.delta",
                            "item_id": rs_id,
                            "output_index": 0,
                            "summary_index": 0,
                            "delta": piece,
                            "sequence_number": seq,
                        }),
                    )
                    .await;
                    seq += 1;
                }

                if let Some(piece) = delta.get("content").and_then(|v| v.as_str()) {
                    if reasoning_opened && !reasoning_closed {
                        reasoning_closed = true;
                        for evt in close_reasoning(&rs_id, &reasoning, &mut seq) {
                            send(&tx, evt).await;
                        }
                    }
                    if !message_opened {
                        message_opened = true;
                        msg_index = if reasoning_opened { 1 } else { 0 };
                        send(
                            &tx,
                            json!({
                                "type": "response.output_item.added",
                                "output_index": msg_index,
                                "item": {
                                    "id": msg_id, "type": "message",
                                    "status": "in_progress", "role": "assistant", "content": []
                                },
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                        send(
                            &tx,
                            json!({
                                "type": "response.content_part.added",
                                "item_id": msg_id,
                                "output_index": msg_index,
                                "content_index": 0,
                                "part": {"type": "output_text", "text": "", "annotations": []},
                                "sequence_number": seq,
                            }),
                        )
                        .await;
                        seq += 1;
                    }
                    text.push_str(piece);
                    send(
                        &tx,
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": msg_id,
                            "output_index": msg_index,
                            "content_index": 0,
                            "delta": piece,
                            "sequence_number": seq,
                        }),
                    )
                    .await;
                    seq += 1;
                }
            }
        }

        if usage.is_null() {
            usage = json!({
                "input_tokens": 0,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 0,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 0,
            });
        }
        // Token accounting from final usage
        let p = usage
            .pointer("/input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let c = usage
            .pointer("/output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        usage_state.record_tokens(&model_for_usage, p, c);

        if reasoning_opened && !reasoning_closed {
            for evt in close_reasoning(&rs_id, &reasoning, &mut seq) {
                send(&tx, evt).await;
            }
        }
        if !message_opened {
            msg_index = if reasoning_opened { 1 } else { 0 };
            send(
                &tx,
                json!({
                    "type": "response.output_item.added",
                    "output_index": msg_index,
                    "item": {
                        "id": msg_id, "type": "message",
                        "status": "in_progress", "role": "assistant", "content": []
                    },
                    "sequence_number": seq,
                }),
            )
            .await;
            seq += 1;
            send(
                &tx,
                json!({
                    "type": "response.content_part.added",
                    "item_id": msg_id,
                    "output_index": msg_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                    "sequence_number": seq,
                }),
            )
            .await;
            seq += 1;
        }
        send(
            &tx,
            json!({
                "type": "response.output_text.done",
                "item_id": msg_id,
                "output_index": msg_index,
                "content_index": 0,
                "text": text,
                "sequence_number": seq,
            }),
        )
        .await;
        seq += 1;
        send(
            &tx,
            json!({
                "type": "response.content_part.done",
                "item_id": msg_id,
                "output_index": msg_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": []},
                "sequence_number": seq,
            }),
        )
        .await;
        seq += 1;
        send(
            &tx,
            json!({
                "type": "response.output_item.done",
                "output_index": msg_index,
                "item": {
                    "id": msg_id, "type": "message", "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]
                },
                "sequence_number": seq,
            }),
        )
        .await;
        seq += 1;

        let mut final_output = Vec::new();
        if !reasoning.is_empty() {
            final_output.push(json!({
                "id": rs_id, "type": "reasoning",
                "summary": [{"type": "summary_text", "text": reasoning}],
                "status": "completed",
            }));
        }
        final_output.push(json!({
            "id": msg_id, "type": "message", "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
        send(
            &tx,
            resp_event(
                "response.completed",
                seq,
                "completed",
                &response_id,
                created_at,
                &model_json,
                &final_output,
                &text,
                &usage,
            ),
        )
        .await;
    });

    let body_stream = futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    (headers, axum::body::Body::from_stream(body_stream)).into_response()
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
