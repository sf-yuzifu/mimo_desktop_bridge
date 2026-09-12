//! Lightweight per-model usage counters.

use crate::error::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub errors: u64,
    pub last_used_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStore {
    #[serde(default)]
    pub models: BTreeMap<String, ModelUsage>,
    #[serde(default)]
    pub total: ModelUsage,
}

pub struct Usage {
    path: PathBuf,
    inner: RwLock<UsageStore>,
}

impl Usage {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("usage.json");
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: RwLock::new(inner),
        }
    }

    pub fn snapshot(&self) -> UsageStore {
        self.inner.read().clone()
    }

    fn persist(&self, store: &UsageStore) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn record(
        &self,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        error: bool,
    ) {
        self.record_inner(model, prompt_tokens, completion_tokens, error, true)
    }

    /// Token-only update (do not bump request count).
    pub fn record_tokens(&self, model: &str, prompt_tokens: u64, completion_tokens: u64) {
        self.record_inner(model, prompt_tokens, completion_tokens, false, false)
    }

    fn record_inner(
        &self,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        error: bool,
        count_request: bool,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        {
            let mut g = self.inner.write();
            let entry = g.models.entry(model.to_string()).or_default();
            if error {
                entry.errors += 1;
            } else {
                if count_request {
                    entry.requests += 1;
                }
                entry.prompt_tokens += prompt_tokens;
                entry.completion_tokens += completion_tokens;
                entry.total_tokens += prompt_tokens + completion_tokens;
            }
            entry.last_used_ms = Some(now);
            if error {
                g.total.errors += 1;
            } else {
                if count_request {
                    g.total.requests += 1;
                }
                g.total.prompt_tokens += prompt_tokens;
                g.total.completion_tokens += completion_tokens;
                g.total.total_tokens += prompt_tokens + completion_tokens;
            }
            g.total.last_used_ms = Some(now);
            let snap = g.clone();
            drop(g);
            if let Err(e) = self.persist(&snap) {
                tracing::warn!("usage persist: {e}");
            }
        }
    }
}
