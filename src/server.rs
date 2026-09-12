//! HTTP server: WebUI control plane + OpenAI-compatible /v1.

use crate::auth::{LoginOutcome, LoginRequest};
use crate::error::{BridgeError, Result};
use crate::state::BridgeState;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

#[derive(RustEmbed)]
#[folder = "webui/"]
struct Assets;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    /// Force-enable TLS regardless of settings (CLI --tls).
    pub tls: Option<bool>,
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8787,
            tls: None,
            tls_cert: None,
            tls_key: None,
        }
    }
}

pub struct HttpServer {
    pub addr: SocketAddr,
    pub tls: bool,
    task: tokio::task::JoinHandle<()>,
    state: Arc<BridgeState>,
}

impl HttpServer {
    pub fn webui_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}", self.addr)
    }

    pub async fn shutdown(self) {
        self.state.clear_bound_addr();
        self.state.usage.flush();
        self.task.abort();
    }
}

/// Resolve TLS config: user PEM if provided, else cached self-signed cert.
async fn load_or_make_tls(
    state: &Arc<BridgeState>,
) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let settings = state.storage.settings();
    if let (Some(cert), Some(key)) = (
        settings.tls_cert_path.clone(),
        settings.tls_key_path.clone(),
    ) {
        return axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .map_err(|e| BridgeError::Proxy(format!("load tls cert/key: {e}")));
    }
    let dir = state.storage.config_dir();
    let cert_path = dir.join("tls-cert.pem");
    let key_path = dir.join("tls-key.pem");
    if !cert_path.exists() || !key_path.exists() {
        let sans = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
            // Stable SAN so users can map via hosts file (router / NAS).
            "local.mimodesktopbridge.com".to_string(),
        ];
        let generated = rcgen::generate_simple_self_signed(sans)
            .map_err(|e| BridgeError::Proxy(e.to_string()))?;
        std::fs::write(&cert_path, generated.cert.pem())?;
        std::fs::write(&key_path, generated.key_pair.serialize_pem())?;
        crate::storage::restrict_permissions(&key_path);
        tracing::info!(
            target = "server",
            "generated self-signed TLS cert at {}",
            cert_path.display()
        );
    }
    axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .map_err(|e| BridgeError::Proxy(format!("load self-signed cert: {e}")))
}

pub async fn start_http(state: Arc<BridgeState>, config: ServerConfig) -> Result<HttpServer> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let addr = SocketAddr::new(config.host, config.port);
    let app = router(state.clone());

    // Persist CLI TLS overrides so status/WebUI see the effective mode.
    if config.tls.is_some() || config.tls_cert.is_some() || config.tls_key.is_some() {
        let mut s = state.storage.settings();
        if let Some(t) = config.tls {
            s.tls_enabled = t;
        }
        if let Some(c) = &config.tls_cert {
            s.tls_cert_path = Some(c.display().to_string());
        }
        if let Some(k) = &config.tls_key {
            s.tls_key_path = Some(k.display().to_string());
        }
        let _ = state.storage.save_settings(s);
    }

    let tls_enabled = state.storage.settings().tls_enabled;

    let std_listener = std::net::TcpListener::bind(addr)
        .map_err(|e| BridgeError::Proxy(format!("bind {addr}: {e}")))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| BridgeError::Proxy(format!("listener: {e}")))?;
    let bound = std_listener.local_addr().unwrap_or(addr);
    state.set_bound_addr(bound);

    let make_service = app.into_make_service();
    let state2 = state.clone();
    let task = if tls_enabled {
        let tls_config = load_or_make_tls(&state).await?;
        tokio::spawn(async move {
            if let Err(e) = axum_server::from_tcp_rustls(std_listener, tls_config)
                .serve(make_service)
                .await
            {
                tracing::error!(target = "server", "https server failed: {e}");
            }
            state2.clear_bound_addr();
        })
    } else {
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
            if let Err(e) = axum::serve(listener, make_service).await {
                tracing::error!(target = "server", "http server failed: {e}");
            }
            state2.clear_bound_addr();
        })
    };

    tokio::time::sleep(Duration::from_millis(10)).await;

    Ok(HttpServer {
        addr: bound,
        tls: tls_enabled,
        task,
        state,
    })
}

fn err_json(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "error": { "message": message, "type": "invalid_request_error", "code": code }
        })),
    )
        .into_response()
}

// ── admin session cookies ────────────────────────────────────────────

/// True when the server is reachable beyond loopback (LAN / 0.0.0.0 bind).
fn exposed_non_loopback(state: &BridgeState) -> bool {
    state
        .bound_addr()
        .map(|a| !a.ip().is_loopback())
        .unwrap_or(false)
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let kv = kv.trim();
        let (k, v) = kv.split_once('=')?;
        if k == "mdb_session" {
            Some(v.to_string())
        } else {
            None
        }
    })
}

fn admin_ok(state: &BridgeState, headers: &HeaderMap) -> bool {
    if !state.storage.admin_configured() {
        // First run: open only on loopback binds. On exposed binds the
        // setup/login gate stays reachable, everything else is locked until
        // an admin password is created — otherwise anyone on the LAN could
        // claim admin (or drive the Xiaomi login) first.
        return !exposed_non_loopback(state);
    }
    let Some(tok) = session_cookie(headers) else {
        return false;
    };
    let sessions = state.admin_sessions.lock();
    sessions.iter().any(|(t, issued)| {
        t == &tok && chrono::Utc::now().timestamp_millis() - issued < 12 * 3600 * 1000
    })
}

fn issue_admin_session(state: &BridgeState) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let mut sessions = state.admin_sessions.lock();
    // Drop expired entries while we're here so the Vec cannot grow unbounded.
    sessions.retain(|(_, issued)| now - issued < 12 * 3600 * 1000);
    let tok = uuid::Uuid::new_v4().simple().to_string();
    sessions.push((tok.clone(), now));
    tok
}

fn revoke_admin_session(state: &BridgeState, tok: &str) {
    state.admin_sessions.lock().retain(|(t, _)| t != tok);
}

fn clear_cookie_header() -> String {
    "mdb_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string()
}

// ── routes ───────────────────────────────────────────────────────────

pub fn router(state: Arc<BridgeState>) -> Router {
    let api = Router::new()
        // Control-plane routes that must work WITHOUT an admin session
        // (login gate + first-run setup).
        .route("/api/admin/session", get(api_admin_session))
        .route("/api/admin/setup", post(api_admin_setup))
        .route("/api/admin/login", post(api_admin_login))
        // Xiaomi account auth + status — require admin once configured.
        .route("/api/auth/status", get(api_auth_status))
        .route("/api/auth/login", post(api_login))
        .route("/api/auth/two-factor/send", post(api_send_ticket))
        .route("/api/auth/two-factor/verify", post(api_verify_ticket))
        .route("/api/auth/refresh", post(api_refresh))
        .route("/api/auth/logout", post(api_logout))
        .route("/api/proxy/status", get(crate::proxy::proxy_status))
        .route("/api/models", get(api_models))
        .route("/api/admin/logout", post(api_admin_logout))
        .route("/api/admin/password", post(api_admin_password))
        .route("/api/keys", get(api_keys_list).post(api_keys_create))
        .route("/api/keys/:id", axum::routing::delete(api_keys_delete))
        .route(
            "/api/settings/api-key-required",
            get(api_key_required_get).post(api_key_required_set),
        )
        .route("/api/settings/port", post(api_set_port))
        .route("/api/usage", get(api_usage))
        .route("/api/settings/tls", get(api_tls_get).post(api_tls_set))
        .route("/api/logs", get(api_logs))
        .route("/api/logs/stream", get(api_logs_stream))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_guard,
        ))
        .with_state(state.clone());

    let proxy = Router::new()
        .route("/v1/models", get(crate::proxy::models))
        .route("/v1/chat/completions", post(crate::proxy::chat))
        .route("/v1/messages", post(crate::anthropic::messages))
        .route("/v1/responses", post(crate::responses::responses))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api_key_guard,
        ));

    api.merge(proxy)
        .route("/healthz", get(healthz))
        .fallback(static_asset)
        .layer(cors_layer(&state))
        .with_state(state)
}

/// Unauthenticated liveness probe (orchestrators, Docker HEALTHCHECK).
async fn healthz() -> Response {
    Json(json!({ "ok": true })).into_response()
}

/// Same-origin by default. Only origins explicitly listed in settings get
/// CORS headers — a permissive default would let any website drive the
/// local /v1 bridge from the browser.
fn cors_layer(state: &BridgeState) -> CorsLayer {
    use axum::http::{HeaderValue, Method};
    let origins: Vec<HeaderValue> = state
        .storage
        .settings()
        .cors_origins
        .iter()
        .filter_map(|o| o.trim().parse().ok())
        .collect();
    if origins.is_empty() {
        return CorsLayer::new();
    }
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::HeaderName::from_static("x-api-key"),
        ])
}

/// Paths that must stay reachable without an admin session.
fn admin_open_path(path: &str) -> bool {
    matches!(
        path,
        "/api/admin/session" | "/api/admin/setup" | "/api/admin/login"
    )
}

/// Once an admin password is set (or the bind is exposed beyond loopback),
/// everything under /api (except the login/setup gate itself) requires a
/// valid mdb_session cookie.
async fn admin_guard(
    State(state): State<Arc<BridgeState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path();
    if admin_open_path(path) {
        return next.run(req).await;
    }
    if admin_ok(&state, req.headers()) {
        return next.run(req).await;
    }
    err_json(
        StatusCode::UNAUTHORIZED,
        "admin session required — unlock the WebUI first",
        "unauthorized",
    )
}

async fn api_key_guard(
    State(state): State<Arc<BridgeState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !state.storage.settings().api_key_required {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|x| x.trim().to_string()))
        .or_else(|| {
            req.headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });
    match presented {
        Some(k) if state.storage.verify_api_key(&k) => next.run(req).await,
        _ => err_json(
            StatusCode::UNAUTHORIZED,
            "missing or invalid API key",
            "invalid_api_key",
        ),
    }
}

async fn static_asset(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // Admin gate: full WebUI (and its assets) stay hidden until unlock —
    // or, on an exposed bind, until the first-run admin password exists.
    // /v1 is unaffected (separate router).
    if !admin_ok(&state, &headers) {
        return match Assets::get("login.html") {
            Some(f) => (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                f.data.into_owned(),
            )
                .into_response(),
            None => (StatusCode::UNAUTHORIZED, "admin session required").into_response(),
        };
    }

    match Assets::get(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            ([(header::CONTENT_TYPE, mime)], f.data.into_owned()).into_response()
        }
        None => {
            // SPA fallback
            match Assets::get("index.html") {
                Some(f) => Html(String::from_utf8_lossy(&f.data).into_owned()).into_response(),
                None => err_json(StatusCode::NOT_FOUND, "not found", "not_found"),
            }
        }
    }
}

// ── control plane handlers ───────────────────────────────────────────

async fn api_auth_status(State(state): State<Arc<BridgeState>>) -> Response {
    let s = state.storage.session();
    Json(json!({
        "loggedIn": s.as_ref().map(|x| x.is_authenticated()).unwrap_or(false),
        "userId": s.as_ref().and_then(|x| x.user_id.clone()),
        "nick": s.as_ref().and_then(|x| x.nick.clone()),
        "hasPassToken": s.as_ref().and_then(|x| x.pass_token.as_ref().map(|_| true)).unwrap_or(false),
        "sid": crate::auth::SID,
    }))
    .into_response()
}

async fn api_login(
    State(state): State<Arc<BridgeState>>,
    Json(req): Json<LoginRequest>,
) -> Response {
    match crate::auth::login_auth2(&state.http, &req).await {
        Ok((LoginOutcome::Authenticated { nick, user_id }, _, Some(mut session))) => {
            // Mint immediately when we already have passToken
            if session.pass_token.is_some() {
                if let Err(e) = crate::auth::mint_service_token(&state.http, &mut session).await {
                    tracing::warn!("mint after login: {e}");
                }
            }
            let _ = state.storage.save_session(session);
            Json(json!({
                "outcome": "authenticated",
                "nick": nick,
                "userId": user_id,
            }))
            .into_response()
        }
        Ok((LoginOutcome::TwoFactorRequired { options }, flow, _)) => {
            if let Some(f) = flow {
                *state.two_factor.lock() = Some(f);
            }
            Json(json!({
                "outcome": "two_factor_required",
                "options": options,
            }))
            .into_response()
        }
        Ok((LoginOutcome::CaptchaRequired { captcha_url }, _, _)) => Json(json!({
            "outcome": "captcha_required",
            "captchaUrl": captcha_url,
        }))
        .into_response(),
        Ok((LoginOutcome::Failed { code, description }, _, _)) => err_json(
            StatusCode::UNAUTHORIZED,
            &format!("{description} (code={code})"),
            "login_failed",
        ),
        Ok((LoginOutcome::Authenticated { .. }, _, None)) => err_json(
            StatusCode::UNAUTHORIZED,
            "login succeeded but no session was returned",
            "login_failed",
        ),
        Err(e) => err_json(e.status(), &e.to_string(), e.code()),
    }
}

#[derive(Deserialize)]
struct SendTicketBody {
    flag: i32,
}

async fn api_send_ticket(
    State(state): State<Arc<BridgeState>>,
    Json(body): Json<SendTicketBody>,
) -> Response {
    let flow = state.two_factor.lock().clone();
    let Some(flow) = flow else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "no in-flight 2FA — login first",
            "bad_request",
        );
    };
    match crate::auth::send_ticket(&state.http, &flow, body.flag).await {
        Ok((true, next_cookie)) => {
            let mut g = state.two_factor.lock();
            if let Some(f) = g.as_mut() {
                f.cookie_header = next_cookie;
            }
            Json(json!({ "ok": true })).into_response()
        }
        Ok((false, err)) => err_json(StatusCode::BAD_GATEWAY, &err, "send_ticket_failed"),
        Err(e) => err_json(e.status(), &e.to_string(), e.code()),
    }
}

#[derive(Deserialize)]
struct VerifyTicketBody {
    flag: i32,
    ticket: String,
}

async fn api_verify_ticket(
    State(state): State<Arc<BridgeState>>,
    Json(body): Json<VerifyTicketBody>,
) -> Response {
    let flow = state.two_factor.lock().clone();
    let Some(flow) = flow else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "no in-flight 2FA — login first",
            "bad_request",
        );
    };
    let cookie = flow.cookie_header.clone();
    match crate::auth::verify_ticket(&state.http, &flow, body.flag, &body.ticket, &cookie).await {
        Ok(mut session) => {
            if session.pass_token.is_some() && session.service_token.is_none() {
                if let Err(e) = crate::auth::mint_service_token(&state.http, &mut session).await {
                    tracing::warn!("mint after 2fa: {e}");
                }
            }
            let _ = state.storage.save_session(session.clone());
            *state.two_factor.lock() = None;
            Json(json!({
                "outcome": "authenticated",
                "userId": session.user_id,
                "hasServiceToken": session.service_token.is_some(),
            }))
            .into_response()
        }
        Err(e) => err_json(e.status(), &e.to_string(), e.code()),
    }
}

async fn api_refresh(State(state): State<Arc<BridgeState>>) -> Response {
    if state.storage.session().is_none() {
        return err_json(StatusCode::UNAUTHORIZED, "not logged in", "unauthorized");
    }
    match state.refresh_session(true).await {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err_json(e.status(), &e.to_string(), e.code()),
    }
}

async fn api_logout(State(state): State<Arc<BridgeState>>) -> Response {
    let _ = state.storage.clear_session();
    *state.two_factor.lock() = None;
    Json(json!({ "ok": true })).into_response()
}

async fn api_models() -> Response {
    Json(json!({
        "object": "list",
        "data": crate::proxy::known_models(),
    }))
    .into_response()
}

async fn api_admin_session(State(state): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    let configured = state.storage.admin_configured();
    let authed = admin_ok(&state, &headers);
    Json(json!({
        "configured": configured,
        "authenticated": authed,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AdminPasswordBody {
    password: String,
}

async fn api_admin_setup(
    State(state): State<Arc<BridgeState>>,
    Json(body): Json<AdminPasswordBody>,
) -> Response {
    if state.storage.admin_configured() {
        return err_json(
            StatusCode::CONFLICT,
            "admin password already configured",
            "conflict",
        );
    }
    if body.password.len() < 6 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "password must be at least 6 characters",
            "bad_request",
        );
    }
    if let Err(e) = state.storage.set_admin_password(&body.password) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage");
    }
    let tok = issue_admin_session(&state);
    (
        [(
            header::SET_COOKIE,
            format!("mdb_session={tok}; Path=/; HttpOnly; SameSite=Lax"),
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn api_admin_login(
    State(state): State<Arc<BridgeState>>,
    Json(body): Json<AdminPasswordBody>,
) -> Response {
    if !state.storage.verify_admin_password(&body.password) {
        return err_json(StatusCode::UNAUTHORIZED, "invalid password", "unauthorized");
    }
    let tok = issue_admin_session(&state);
    (
        [(
            header::SET_COOKIE,
            format!("mdb_session={tok}; Path=/; HttpOnly; SameSite=Lax"),
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn api_admin_logout(State(state): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    if let Some(tok) = session_cookie(&headers) {
        revoke_admin_session(&state, &tok);
    }
    (
        [(header::SET_COOKIE, clear_cookie_header())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct ChangePasswordBody {
    old_password: String,
    new_password: String,
}

async fn api_admin_password(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    if !state.storage.verify_admin_password(&body.old_password) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "old password is incorrect",
            "unauthorized",
        );
    }
    if body.new_password.len() < 6 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "new password must be at least 6 characters",
            "bad_request",
        );
    }
    if let Err(e) = state.storage.set_admin_password(&body.new_password) {
        return err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage");
    }
    // Rotate session: revoke old cookie, issue a fresh one.
    if let Some(tok) = session_cookie(&headers) {
        revoke_admin_session(&state, &tok);
    }
    let tok = issue_admin_session(&state);
    (
        [(
            header::SET_COOKIE,
            format!("mdb_session={tok}; Path=/; HttpOnly; SameSite=Lax"),
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn api_keys_list(State(state): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    Json(json!({ "keys": state.storage.list_keys() })).into_response()
}

#[derive(Deserialize)]
struct CreateKeyBody {
    #[serde(default)]
    label: String,
}

async fn api_keys_create(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    Json(body): Json<CreateKeyBody>,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    match state.storage.create_api_key(&body.label) {
        Ok((id, token)) => Json(json!({
            "id": id,
            "token": token,
            "note": "store this token now — it is not shown again",
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage"),
    }
}

async fn api_keys_delete(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    match state.storage.delete_api_key(&id) {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "key not found", "not_found"),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage"),
    }
}

async fn api_key_required_get(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    Json(json!({ "required": state.storage.settings().api_key_required })).into_response()
}

#[derive(Deserialize)]
struct BoolBody {
    required: bool,
}

async fn api_key_required_set(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    Json(body): Json<BoolBody>,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    match state.storage.set_api_key_required(body.required) {
        Ok(()) => Json(json!({ "ok": true, "required": body.required })).into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage"),
    }
}

#[derive(Deserialize)]
struct PortBody {
    port: u16,
}

async fn api_set_port(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    Json(body): Json<PortBody>,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    if body.port < 1024 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "port must be >= 1024",
            "bad_request",
        );
    }
    match state.storage.set_port(body.port) {
        Ok(()) => Json(json!({
            "ok": true,
            "port": body.port,
            "note": "restart the server to bind the new port",
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage"),
    }
}

async fn api_usage(State(state): State<Arc<BridgeState>>) -> Response {
    let snap = state.usage.snapshot();
    Json(json!({
        "models": snap.models,
        "total": snap.total,
    }))
    .into_response()
}

async fn api_tls_get(State(state): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    let s = state.storage.settings();
    Json(json!({
        "enabled": s.tls_enabled,
        "certPath": s.tls_cert_path,
        "keyPath": s.tls_key_path,
        "note": "restart the server to apply TLS changes",
    }))
    .into_response()
}

#[derive(Deserialize)]
struct TlsBody {
    enabled: bool,
}

async fn api_tls_set(
    State(state): State<Arc<BridgeState>>,
    headers: HeaderMap,
    Json(body): Json<TlsBody>,
) -> Response {
    if !admin_ok(&state, &headers) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            "admin session required",
            "unauthorized",
        );
    }
    match state.storage.set_tls_enabled(body.enabled) {
        Ok(()) => Json(json!({
            "ok": true,
            "enabled": body.enabled,
            "note": "restart the server to bind TLS",
        }))
        .into_response(),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "storage"),
    }
}

async fn api_logs(State(state): State<Arc<BridgeState>>) -> Response {
    Json(state.logs.snapshot()).into_response()
}

async fn api_logs_stream(
    State(state): State<Arc<BridgeState>>,
) -> axum::response::Sse<
    impl futures_util::Stream<
        Item = std::result::Result<axum::response::sse::Event, std::convert::Infallible>,
    >,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let rx = state.logs.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    let event = Event::default().json_data(payload).unwrap_or_else(|_| {
                        Event::default().data("{\"kind\":\"error\",\"message\":\"encode log\"}")
                    });
                    return Some((Ok(event), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
