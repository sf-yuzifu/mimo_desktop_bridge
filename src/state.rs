//! Shared bridge state.

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
}

impl BridgeState {
    pub fn new(storage: Arc<Storage>) -> Self {
        let usage = Arc::new(Usage::load(storage.config_dir()));
        let logs = Arc::new(LogHub::new(200));
        let http = reqwest::Client::builder()
            .gzip(true)
            .timeout(std::time::Duration::from_secs(120))
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
        }
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
