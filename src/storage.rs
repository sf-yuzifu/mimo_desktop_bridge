//! Config dir, session blob, API keys, admin password, port settings.

use crate::auth::Session;
use crate::error::{BridgeError, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use directories::ProjectDirs;
use parking_lot::RwLock;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub api_key_required: bool,
    /// Argon2 hash of admin password; None = first-run setup.
    pub admin_password_hash: Option<String>,
    /// Serve HTTPS (self-signed cert auto-generated under config dir).
    #[serde(default)]
    pub tls_enabled: bool,
    /// Optional user-provided PEM cert/key (overrides auto-generated).
    #[serde(default)]
    pub tls_cert_path: Option<String>,
    #[serde(default)]
    pub tls_key_path: Option<String>,
    /// Extra allowed CORS origins (e.g. "http://localhost:3000").
    /// Empty (default) = no CORS headers = same-origin only. A permissive
    /// default would let any website call the local /v1 from the browser.
    #[serde(default)]
    pub cors_origins: Vec<String>,
}

fn default_port() -> u16 {
    8787
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: default_port(),
            api_key_required: false,
            admin_password_hash: None,
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
            cors_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub hash: String,
    pub prefix: String,
    pub label: String,
    pub created: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyStore {
    pub keys: Vec<ApiKeyRecord>,
}

/// Config files may hold secrets (session.json has passToken in cleartext).
/// Keep them owner-only on Unix; a no-op elsewhere.
pub fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("chmod 600 {}: {e}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub struct Storage {
    config_dir: PathBuf,
    settings: RwLock<Settings>,
    session: RwLock<Option<Session>>,
    keys: RwLock<KeyStore>,
}

impl Storage {
    pub fn open() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "mimo", "mimo_desktop_bridge")
            .ok_or_else(|| BridgeError::Storage("cannot resolve config dir".into()))?;
        let config_dir = dirs.config_dir().to_path_buf();
        std::fs::create_dir_all(&config_dir)?;
        let s = Self {
            config_dir,
            settings: RwLock::new(Settings::default()),
            session: RwLock::new(None),
            keys: RwLock::new(KeyStore::default()),
        };
        s.load_all()?;
        Ok(s)
    }

    pub fn open_in(dir: impl AsRef<Path>) -> Result<Self> {
        let config_dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&config_dir)?;
        let s = Self {
            config_dir,
            settings: RwLock::new(Settings::default()),
            session: RwLock::new(None),
            keys: RwLock::new(KeyStore::default()),
        };
        s.load_all()?;
        Ok(s)
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    fn settings_path(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }
    fn session_path(&self) -> PathBuf {
        self.config_dir.join("session.json")
    }
    fn keys_path(&self) -> PathBuf {
        self.config_dir.join("api-keys.json")
    }

    fn load_all(&self) -> Result<()> {
        if let Ok(t) = std::fs::read_to_string(self.settings_path()) {
            if let Ok(v) = serde_json::from_str::<Settings>(&t) {
                *self.settings.write() = v;
            }
        }
        if let Ok(t) = std::fs::read_to_string(self.session_path()) {
            if let Ok(v) = serde_json::from_str::<Session>(&t) {
                *self.session.write() = Some(v);
            }
        }
        if let Ok(t) = std::fs::read_to_string(self.keys_path()) {
            if let Ok(v) = serde_json::from_str::<KeyStore>(&t) {
                *self.keys.write() = v;
            }
        }
        Ok(())
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let tmp = path.with_extension("tmp");
        let s = serde_json::to_string_pretty(value)?;
        std::fs::write(&tmp, s)?;
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn save_settings(&self, settings: Settings) -> Result<()> {
        self.write_json(&self.settings_path(), &settings)?;
        *self.settings.write() = settings;
        Ok(())
    }

    pub fn set_port(&self, port: u16) -> Result<()> {
        let mut s = self.settings();
        s.port = port;
        self.save_settings(s)
    }

    pub fn set_api_key_required(&self, required: bool) -> Result<()> {
        let mut s = self.settings();
        s.api_key_required = required;
        self.save_settings(s)
    }

    pub fn set_tls_enabled(&self, enabled: bool) -> Result<()> {
        let mut s = self.settings();
        s.tls_enabled = enabled;
        self.save_settings(s)
    }

    pub fn session(&self) -> Option<Session> {
        self.session.read().clone()
    }

    pub fn save_session(&self, session: Session) -> Result<()> {
        self.write_json(&self.session_path(), &session)?;
        *self.session.write() = Some(session);
        Ok(())
    }

    pub fn clear_session(&self) -> Result<()> {
        let _ = std::fs::remove_file(self.session_path());
        *self.session.write() = None;
        Ok(())
    }

    // ── admin password ──────────────────────────────────────────────
    pub fn admin_configured(&self) -> bool {
        self.settings.read().admin_password_hash.is_some()
    }

    pub fn set_admin_password(&self, password: &str) -> Result<()> {
        let salt = SaltString::generate(&mut rand::rngs::OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| BridgeError::Storage(format!("argon2: {e}")))?
            .to_string();
        let mut s = self.settings();
        s.admin_password_hash = Some(hash);
        self.save_settings(s)
    }

    pub fn verify_admin_password(&self, password: &str) -> bool {
        let Some(hash) = self.settings.read().admin_password_hash.clone() else {
            return false;
        };
        let Ok(parsed) = PasswordHash::new(&hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }

    // ── API keys ────────────────────────────────────────────────────
    pub fn list_keys(&self) -> Vec<ApiKeyRecord> {
        self.keys.read().keys.clone()
    }

    /// Create a key. Returns (id, plaintext). Only hash + prefix are stored.
    pub fn create_api_key(&self, label: &str) -> Result<(String, String)> {
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token = format!("mdb_{}", hex::encode(raw));
        let hash = hex::encode(Sha256::digest(token.as_bytes()));
        let prefix = token.chars().take(12).collect::<String>();
        let id = format!("key_{}", uuid::Uuid::new_v4().simple());
        let rec = ApiKeyRecord {
            id: id.clone(),
            hash,
            prefix,
            label: if label.is_empty() {
                "unnamed".into()
            } else {
                label.to_string()
            },
            created: chrono::Utc::now().timestamp_millis(),
        };
        {
            let mut ks = self.keys.write();
            ks.keys.push(rec);
            self.write_json(&self.keys_path(), &*ks)?;
        }
        Ok((id, token))
    }

    pub fn delete_api_key(&self, id: &str) -> Result<bool> {
        let mut ks = self.keys.write();
        let before = ks.keys.len();
        ks.keys.retain(|k| k.id != id);
        let removed = ks.keys.len() < before;
        if removed {
            self.write_json(&self.keys_path(), &*ks)?;
        }
        Ok(removed)
    }

    pub fn verify_api_key(&self, presented: &str) -> bool {
        let hash = hex::encode(Sha256::digest(presented.as_bytes()));
        self.keys.read().keys.iter().any(|k| k.hash == hash)
    }
}

/// Constant-time-ish string compare for session cookies (not security-critical).
pub fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches() {
        assert!(ct_eq("abc", "abc"));
        assert!(ct_eq("", ""));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
