//! Shared bridge state.

use crate::error::{BridgeError, Result};
use crate::storage::Storage;
use crate::usage::Usage;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct BridgeState {
    pub storage: Arc<Storage>,
    pub usage: Arc<Usage>,
    pub logs: Arc<LogHub>,
    pub http: reqwest::Client,
    /// In-flight 2FA flow (one at a time is enough for a local bridge).
    pub two_factor: Mutex<Option<crate::auth::TwoFactorFlow>>,
    /// Admin session tokens (cookie value → issued-at ms).
    pub admin_sessions: Mutex<Vec<(String, i64)>>,
    pub bound_addr: Mutex<Option<SocketAddr>>,
    /// Serializes serviceToken refresh so concurrent 401s mint once.
    refresh_lock: tokio::sync::Mutex<()>,
}

/// Skip re-minting if another request refreshed within this window.
const REFRESH_DEDUP_MS: i64 = 60_000;

impl BridgeState {
    pub fn new(storage: Arc<Storage>) -> Self {
        let usage = Arc::new(Usage::load(storage.config_dir()));
        let logs = Arc::new(LogHub::new(200));
        let http = reqwest::Client::builder()
            .gzip(true)
            // Connection setup gets its own bound; the total timeout must NOT
            // cover the whole body read or long SSE generations get cut off.
            .connect_timeout(std::time::Duration::from_secs(30))
            // Applied per read: for SSE this means "max silence between chunks".
            .read_timeout(std::time::Duration::from_secs(120))
            // Manual redirect handling so we can harvest Set-Cookie on every hop
            // (auto-follow drops intermediate cookies — passToken often lands there).
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self {
            storage,
            usage,
            logs,
            http,
            two_factor: Mutex::new(None),
            admin_sessions: Mutex::new(Vec::new()),
            bound_addr: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Mint a fresh serviceToken and persist it.
    ///
    /// Single-flight: concurrent callers serialize on an internal lock. With
    /// `force=false` (401 retry path) a caller that lost the race reuses the
    /// session another request just saved instead of minting again.
    pub async fn refresh_session(&self, force: bool) -> Result<crate::auth::Session> {
        let _guard = self.refresh_lock.lock().await;
        let Some(mut s) = self.storage.session() else {
            return Err(BridgeError::Unauthorized);
        };
        if s.pass_token.is_none() {
            return Err(BridgeError::Login("no passToken to refresh with".into()));
        }
        if !force {
            if let Some(at) = s.refreshed_at {
                let fresh = chrono::Utc::now().timestamp_millis() - at < REFRESH_DEDUP_MS;
                if fresh && s.service_token.is_some() {
                    return Ok(s);
                }
            }
        }
        crate::auth::mint_service_token(&self.http, &mut s).await?;
        let _ = self.storage.save_session(s.clone());
        Ok(s)
    }

    pub fn set_bound_addr(&self, addr: SocketAddr) {
        *self.bound_addr.lock() = Some(addr);
    }

    pub fn clear_bound_addr(&self) {
        *self.bound_addr.lock() = None;
    }

    pub fn bound_addr(&self) -> Option<SocketAddr> {
        *self.bound_addr.lock()
    }

    pub fn emit_log(&self, payload: Value) {
        self.logs.push(payload);
    }
}

const LOG_BYTE_CAP: usize = 8 * 1024 * 1024;

pub struct LogHub {
    rows: Mutex<VecDeque<(usize, Value)>>,
    cap: usize,
    tx: broadcast::Sender<Value>,
}

impl LogHub {
    pub fn new(cap: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            rows: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
            tx,
        }
    }

    pub fn push(&self, payload: Value) {
        let size = payload.to_string().len() + 64;
        {
            let mut rows = self.rows.lock();
            let mut total: usize = rows.iter().map(|(s, _)| s).sum();
            while rows.len() >= self.cap || total + size > LOG_BYTE_CAP {
                match rows.pop_back() {
                    Some((s, _)) => total -= s,
                    None => break,
                }
            }
            rows.push_front((size, payload.clone()));
        }
        let _ = self.tx.send(payload);
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.rows.lock().iter().map(|(_, v)| v.clone()).collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }
}
