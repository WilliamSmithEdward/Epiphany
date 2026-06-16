//! The operator secret store (ADR-0030): named credentials that HTTP connections
//! reference by name. Values are write-only over the API; only names are ever
//! read back, and a value never reaches the model, the logs, or the audit
//! stream. Persisted to an owner-only (0600) file, reusing the same write helper
//! as the security model and admin-password file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A name to value map of operator secrets, optionally backed by a durable file.
#[derive(Debug, Default)]
pub struct SecretStore {
    path: Option<PathBuf>,
    secrets: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct SecretDoc {
    format: String,
    #[serde(default, rename = "secret")]
    secrets: Vec<SecretEntry>,
}

#[derive(Serialize, Deserialize)]
struct SecretEntry {
    name: String,
    value: String,
}

const FORMAT: &str = "epiphany-secrets-v1";

impl SecretStore {
    /// An in-memory store (tests; no persistence).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Open the store at `path`, loading it if present (tolerant: a malformed
    /// file yields an empty store rather than blocking startup), else start empty
    /// and persist on the first write.
    pub fn open_or_create(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut secrets = BTreeMap::new();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            if let Ok(doc) = toml::from_str::<SecretDoc>(&text) {
                for entry in doc.secrets {
                    secrets.insert(entry.name, entry.value);
                }
            }
        }
        Ok(Self {
            path: Some(path),
            secrets,
        })
    }

    /// Set (create or replace) a secret, persisting the store.
    pub fn set(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> std::io::Result<()> {
        self.secrets.insert(name.into(), value.into());
        self.save()
    }

    /// Remove a secret, persisting if it existed. Returns whether it existed.
    pub fn remove(&mut self, name: &str) -> std::io::Result<bool> {
        let existed = self.secrets.remove(name).is_some();
        if existed {
            self.save()?;
        }
        Ok(existed)
    }

    /// The secret names, sorted. Never the values: this is the only listing the
    /// API exposes.
    pub fn names(&self) -> Vec<String> {
        self.secrets.keys().cloned().collect()
    }

    /// Whether a secret with this name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.secrets.contains_key(name)
    }

    /// The value for `name`, used only at fetch time to build an auth header. The
    /// API never returns this to a client.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let doc = SecretDoc {
            format: FORMAT.to_string(),
            secrets: self
                .secrets
                .iter()
                .map(|(name, value)| SecretEntry {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        };
        let text = toml::to_string(&doc).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("tmp");
        // Owner-only from creation: the file holds plaintext credentials (ADR-0030;
        // at-rest protection is the operator-managed posture of ADR-0025).
        crate::write_owner_only(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_list_get_remove() {
        let mut store = SecretStore::in_memory();
        store.set("token", "abc123").unwrap();
        store.set("basic", "user:pass").unwrap();
        assert_eq!(
            store.names(),
            vec!["basic".to_string(), "token".to_string()]
        );
        assert_eq!(store.get("token"), Some("abc123"));
        assert!(store.contains("basic"));
        assert!(store.remove("token").unwrap());
        assert!(!store.remove("token").unwrap());
        assert_eq!(store.names(), vec!["basic".to_string()]);
    }

    #[test]
    fn names_never_expose_values() {
        let mut store = SecretStore::in_memory();
        store.set("token", "super-secret-value").unwrap();
        // The only listing is of names; the value is not part of it.
        assert!(!store
            .names()
            .iter()
            .any(|n| n.contains("super-secret-value")));
    }

    #[test]
    fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("epiphany-secrets-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("secrets.toml");
        {
            let mut store = SecretStore::open_or_create(&path).unwrap();
            store.set("token", "abc123").unwrap();
        }
        let reloaded = SecretStore::open_or_create(&path).unwrap();
        assert_eq!(reloaded.get("token"), Some("abc123"));
        assert_eq!(reloaded.names(), vec!["token".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tolerant_of_a_malformed_file() {
        let dir = std::env::temp_dir().join(format!("epiphany-secrets-bad-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, "this is not valid toml = = =").unwrap();
        let store = SecretStore::open_or_create(&path).unwrap();
        assert!(store.names().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
