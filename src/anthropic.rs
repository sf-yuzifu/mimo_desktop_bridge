//! Anthropic Messages ↔ OpenAI Chat Completions (text subset).
//!
//! Good enough for Claude Code / Anthropic SDKs on the MiMo free channel.

use crate::state::BridgeState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

fn anth_err(status: StatusCode, message: &str, err_type: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": { "type": err_type, "message": message }
        })),
    )
        .into_response()
}

/// Anthropic Messages body → OpenAI chat.completions body (always stream upstream).
pub fn anthropic_to_openai(body: &Value) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("model is required")?;
    let max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);

    // Tool calling is not translated — fail loudly instead of silently
    // dropping the tools and letting the model answer without them.
    let has_tools = body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
        || body.get("tool_choice").is_some();
    if has_tools {
        return Err(
            "tool calling is not supported by this Anthropic-compat bridge \
             (the MiMo free channel is reached via chat completions without tool translation). \
             Remove `tools`/`tool_choice`, or call /v1/chat/completions directly."
                .into(),
        );
    }

    let mut messages: Vec<Value> = Vec::new();

    if let Some(sys) = body.get("system") {
        let text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) else {
        return Err("messages is required".into());
    };
    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => {
                let mut chunks: Vec<String> = Vec::new();
                for p in parts {
                    match p.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                                chunks.push(t.to_string());
                            }
                        }
                        Some("thinking") => {
                            if let Some(t) = p.get("thinking").and_then(|t| t.as_str()) {
                                chunks.push(t.to_string());
                            }
                        }
                        Some("tool_result") => {
                            let inner = match p.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(other) => other.to_string(),
                                None => String::new(),
                            };
                            chunks.push(inner);
                        }
                        _ => {}
                    }
                }
                chunks.join("\n")
            }
            _ => String::new(),
        };
        if role == "tool" {
            messages.push(json!({ "role": "tool", "content": text }));
        } else {
            messages.push(json!({ "role": role, "content": text }));
        }
    }

    let mut out = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(temp) = body.get("temperature") {
        out["temperature"] = temp.clone();
    }
    if let Some(top_p) = body.get("top_p") {
        out["top_p"] = top_p.clone();
    }
    if let Some(stop) = body.get("stop_sequences") {
        out["stop"] = stop.clone();
    }
    Ok(out)
}

// axum Response is inherently large; boxing buys nothing here.
#[allow(clippy::result_large_err)]
async fn call_upstream(
    state: &Arc<BridgeState>,
    openai_body: &Value,
) -> Result<reqwest::Response, Response> {
    match crate::upstream::send_chat(state, openai_body).await {
        Ok(r) if r.status().is_success() => Ok(r),
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            Err(anth_err(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                &format!(
                    "upstream {status}: {}",
                    text.chars().take(300).collect::<String>()
                ),
                "api_error",
            ))
        }
        Err(crate::upstream::SendError::NotLoggedIn) => Err(anth_err(
            StatusCode::UNAUTHORIZED,
            "not logged in — open the WebUI and sign in",
            "authentication_error",
        )),
        Err(crate::upstream::SendError::Upstream(e)) => Err(anth_err(
            StatusCode::BAD_GATEWAY,
            &format!("upstream fetch failed: {e}"),
            "api_error",
        )),
    }
}

pub async fn messages(State(state): State<Arc<BridgeState>>, body: String) -> Response {
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return anth_err(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    let client_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let thinking_enabled =
        parsed.pointer("/thinking/type").and_then(|v| v.as_str()) == Some("enabled");

    let openai_body = match anthropic_to_openai(&parsed) {
        Ok(v) => v,
        Err(e) => return anth_err(StatusCode::BAD_REQUEST, &e, "invalid_request_error"),
    };
    let model = openai_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let resp = match call_upstream(&state, &openai_body).await {
        Ok(r) => r,
        Err(e) => return e,
    };

    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    let upstream = resp.bytes_stream();
    let state2 = state.clone();
    let model2 = model.clone();
    let mid = message_id.clone();

    let body_stream = futures_util::stream::unfold(
        (
            upstream,
            Vec::<u8>::new(),
            0u64,
            0u64,
            false,
            mid,
            thinking_enabled,
            model2,
            state2,
        ),
        |(mut up, mut buf, mut prompt_toks, mut comp_toks, mut done, mid, thinking, model, st)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(line) = crate::upstream::take_sse_line(&mut buf) {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        let events = format!(
                            "event: message_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
                            json!({
                                "type": "message_delta",
                                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                                "usage": { "output_tokens": comp_toks }
                            }),
                            json!({ "type": "message_stop" })
                        );
                        st.usage.record(&model, prompt_toks, comp_toks, false);
                        done = true;
                        return Some((
                            Ok::<Bytes, std::io::Error>(Bytes::from(events)),
                            (
                                up,
                                buf,
                                prompt_toks,
                                comp_toks,
                                done,
                                mid,
                                thinking,
                                model,
                                st,
                            ),
                        ));
                    }
                    let Ok(v) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
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
                    let choice = v.pointer("/choices/0");
                    let delta = choice
                        .and_then(|c| c.get("delta"))
                        .cloned()
                        .unwrap_or(json!({}));
                    let finish = choice
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|f| f.as_str())
                        .map(|s| s.to_string());

                    let mut events = String::new();
                    let content = delta.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    if !content.is_empty() {
                        comp_toks = comp_toks.max(content.chars().count() as u64 / 4);
                        events.push_str(&format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": { "type": "text_delta", "text": content }
                            })
                        ));
                    }
                    if thinking {
                        let reasoning = delta
                            .get("reasoning_content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        if !reasoning.is_empty() {
                            events.push_str(&format!(
                                "event: content_block_delta\ndata: {}\n\n",
                                json!({
                                    "type": "content_block_delta",
                                    "index": 1,
                                    "delta": { "type": "thinking_delta", "thinking": reasoning }
                                })
                            ));
                        }
                    }
                    if finish.is_some() {
                        events.push_str(&format!(
                            "event: message_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
                            json!({
                                "type": "message_delta",
                                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                                "usage": { "output_tokens": comp_toks }
                            }),
                            json!({ "type": "message_stop" })
                        ));
                        st.usage.record(&model, prompt_toks, comp_toks, false);
                        done = true;
                        return Some((
                            Ok(Bytes::from(events)),
                            (
                                up,
                                buf,
                                prompt_toks,
                                comp_toks,
                                done,
                                mid,
                                thinking,
                                model,
                                st,
                            ),
                        ));
                    }
                    if events.is_empty() {
                        continue;
                    }
                    return Some((
                        Ok(Bytes::from(events)),
                        (
                            up,
                            buf,
                            prompt_toks,
                            comp_toks,
                            done,
                            mid,
                            thinking,
                            model,
                            st,
                        ),
                    ));
                }
                match up.next().await {
                    Some(Ok(b)) => buf.extend_from_slice(&b),
                    Some(Err(e)) => {
                        st.usage.record(&model, prompt_toks, comp_toks, true);
                        done = true;
                        return Some((
                            Err(std::io::Error::other(e.to_string())),
                            (
                                up,
                                buf,
                                prompt_toks,
                                comp_toks,
                                done,
                                mid,
                                thinking,
                                model,
                                st,
                            ),
                        ));
                    }
                    None => {
                        if !done {
                            st.usage.record(&model, prompt_toks, comp_toks, false);
                            done = true;
                            let ev = format!(
                                "event: message_stop\ndata: {}\n\n",
                                json!({ "type": "message_stop" })
                            );
                            return Some((
                                Ok(Bytes::from(ev)),
                                (
                                    up,
                                    buf,
                                    prompt_toks,
                                    comp_toks,
                                    done,
                                    mid,
                                    thinking,
                                    model,
                                    st,
                                ),
                            ));
                        }
                        return None;
                    }
                }
            }
        },
    );

    let preamble = format!(
        "event: message_start\ndata: {}\n\nevent: content_block_start\ndata: {}\n\n",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        }),
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        })
    );

    if client_stream {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            header::HeaderName::from_static("cache-control"),
            header::HeaderValue::from_static("no-cache"),
        );
        let combined = futures_util::stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from(preamble))
        })
        .chain(body_stream);
        return (headers, Body::from_stream(combined)).into_response();
    }

    // Non-stream: buffer translated SSE, then emit one Anthropic message.
    let mut s = Box::pin(body_stream);
    let mut collected = String::new();
    while let Some(item) = s.next().await {
        let Ok(bytes) = item else {
            return anth_err(
                StatusCode::BAD_GATEWAY,
                "upstream stream error",
                "api_error",
            );
        };
        collected.push_str(&String::from_utf8_lossy(&bytes));
    }

    let mut text = String::new();
    let mut thinking_out = String::new();
    let mut out_tokens = 0u64;
    for line in collected.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("content_block_delta") => {
                if let Some(t) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
                if let Some(t) = v.pointer("/delta/thinking").and_then(|t| t.as_str()) {
                    thinking_out.push_str(t);
                }
            }
            Some("message_delta") => {
                if let Some(n) = v.pointer("/usage/output_tokens").and_then(|n| n.as_u64()) {
                    out_tokens = n;
                }
            }
            _ => {}
        }
    }

    let mut content = Vec::new();
    if thinking_enabled && !thinking_out.is_empty() {
        content.push(json!({ "type": "thinking", "thinking": thinking_out }));
    }
    content.push(json!({ "type": "text", "text": text }));

    Json(json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 0, "output_tokens": out_tokens }
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_model_and_messages() {
        assert!(anthropic_to_openai(&json!({})).is_err());
        assert!(anthropic_to_openai(&json!({"model": "m"})).is_err());
    }

    #[test]
    fn tools_are_rejected_explicitly() {
        let err = anthropic_to_openai(&json!({
            "model": "m",
            "messages": [],
            "tools": [{"name": "t", "description": "", "input_schema": {}}]
        }))
        .unwrap_err();
        assert!(err.contains("tool calling is not supported"));
        assert!(anthropic_to_openai(&json!({
            "model": "m",
            "messages": [],
            "tool_choice": {"type": "auto"}
        }))
        .is_err());
        // Empty tools array is fine
        assert!(anthropic_to_openai(&json!({"model": "m", "messages": [], "tools": []})).is_ok());
    }

    #[test]
    fn system_string_becomes_system_message() {
        let out = anthropic_to_openai(&json!({
            "model": "m",
            "system": "be nice",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be nice");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn system_array_blocks_joined() {
        let out = anthropic_to_openai(&json!({
            "model": "m",
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "messages": []
        }))
        .unwrap();
        assert_eq!(out["messages"][0]["content"], "a\nb");
    }

    #[test]
    fn defaults_max_tokens_and_maps_options() {
        let out = anthropic_to_openai(&json!({
            "model": "m",
            "messages": [],
            "temperature": 0.3,
            "top_p": 0.9,
            "stop_sequences": ["END"]
        }))
        .unwrap();
        assert_eq!(out["max_tokens"], 4096);
        assert_eq!(out["temperature"], 0.3);
        assert_eq!(out["top_p"], 0.9);
        assert_eq!(out["stop"], json!(["END"]));
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn content_parts_text_thinking_tool_result() {
        let out = anthropic_to_openai(&json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "T"},
                    {"type": "thinking", "thinking": "H"},
                    {"type": "tool_result", "content": "R"}
                ]
            }]
        }))
        .unwrap();
        assert_eq!(out["messages"][0]["content"], "T\nH\nR");
    }

    #[test]
    fn tool_result_object_content_stringified() {
        let out = anthropic_to_openai(&json!({
            "model": "m",
            "messages": [{
                "role": "tool",
                "content": [{"type": "tool_result", "content": {"ok": true}}]
            }]
        }))
        .unwrap();
        assert_eq!(out["messages"][0]["role"], "tool");
        assert!(out["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("ok"));
    }
}
