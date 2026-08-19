//! Persisted configuration at `~/.bifrost/config.json`.
//!
//! Written atomically: a crash mid-save must never leave a node with an
//! unreadable config and therefore a brand-new identity, which would orphan
//! every alias its peers had for it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::net::protocol::PortSet;
use crate::resources::sharing::{SharingPolicy, DEFAULT_SHARE_FRACTION};
use crate::util::identity::Identity;

/// Everything persisted between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub identity: Identity,
    #[serde(default)]
    pub ports: PortSet,
    /// Where received files are written.
    pub download_dir: PathBuf,
    /// Where chat history and other state are kept.
    pub data_dir: PathBuf,
    /// Peer node id → local nickname.
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub sharing_enabled: bool,
    #[serde(default = "default_share_fraction")]
    pub share_fraction: f32,
    #[serde(default = "default_true")]
    pub auto_accept_files: bool,
    /// Reject inbound files above this size, if set.
    #[serde(default)]
    pub max_incoming_file_bytes: Option<u64>,
    /// Serve the web dashboard.
    #[serde(default = "default_true")]
    pub web_enabled: bool,
    /// Bind the dashboard to `0.0.0.0` so phones on the LAN can reach it.
    ///
    /// This is the whole point of the dashboard, so it defaults on; setting it
    /// false restricts the dashboard to `127.0.0.1`.
    #[serde(default = "default_true")]
    pub web_lan_accessible: bool,
}

fn default_true() -> bool {
    true
}

fn default_share_fraction() -> f32 {
    DEFAULT_SHARE_FRACTION
}

impl Config {
    /// `~/.bifrost`, or `./.bifrost` if there is no home directory.
    pub fn base_dir() -> PathBuf {
        std::env::var_os("BIFROST_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".bifrost")
            })
    }

    pub fn default_path() -> PathBuf {
        Self::base_dir().join("config.json")
    }

    /// A fresh config for this machine.
    pub fn generate() -> Self {
        let base = Self::base_dir();
        Self {
            identity: Identity::generate(),
            ports: PortSet::default(),
            download_dir: dirs::download_dir()
                .map(|d| d.join("Bifrost"))
                .unwrap_or_else(|| base.join("downloads")),
            data_dir: base.join("data"),
            aliases: HashMap::new(),
            sharing_enabled: true,
            share_fraction: DEFAULT_SHARE_FRACTION,
            auto_accept_files: true,
            max_incoming_file_bytes: None,
            web_enabled: true,
            web_lan_accessible: true,
        }
    }

    /// Load from `path`, creating a default config if it does not exist.
    ///
    /// A config that exists but cannot be parsed is moved aside rather than
    /// overwritten, so a hand-edit with a stray comma is recoverable.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::generate();
            config.save(path)?;
            tracing::info!(
                path = %path.display(),
                node_id = %config.identity.node_id,
                "created a new identity"
            );
            return Ok(config);
        }

        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;

        match serde_json::from_str::<Config>(&raw) {
            Ok(config) => {
                tracing::info!(path = %path.display(), "config loaded");
                Ok(config)
            }
            Err(e) => {
                let backup = path.with_extension("json.corrupt");
                let _ = std::fs::rename(path, &backup);
                tracing::error!(
                    error = %e,
                    backup = %backup.display(),
                    "config unreadable; kept a copy and generated a new identity"
                );
                let config = Self::generate();
                config.save(path)?;
                Ok(config)
            }
        }
    }

    /// Write atomically: to a temporary file, then rename over the target.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }

        let json = serde_json::to_string_pretty(self).context("cannot serialise the config")?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json).with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }

    pub fn sharing_policy(&self) -> SharingPolicy {
        SharingPolicy {
            enabled: self.sharing_enabled,
            fraction: self.share_fraction,
        }
    }

    /// Create the download and data directories.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.download_dir, &self.data_dir] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_created_then_reloaded_identically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let first = Config::load_or_create(&path).unwrap();
        assert!(path.exists());

        let second = Config::load_or_create(&path).unwrap();
        assert_eq!(
            first.identity, second.identity,
            "identity must be stable across restarts"
        );
    }

    #[test]
    fn a_corrupt_config_is_preserved_rather_than_silently_destroyed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let config = Config::load_or_create(&path).unwrap();
        assert!(!config.identity.node_id.is_empty());

        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "the unreadable file should be kept");
        assert!(std::fs::read_to_string(backup)
            .unwrap()
            .contains("this is not json"));
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        // A config written by an older build, plus a field from a newer one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "identity": {
                    "node_id": "abc", "username": "alice",
                    "device_type": "linux-laptop", "os": "Linux"
                },
                "download_dir": "/tmp/dl",
                "data_dir": "/tmp/data",
                "some_future_setting": 42
            }"#,
        )
        .unwrap();

        let config = Config::load_or_create(&path).unwrap();
        assert_eq!(config.identity.username, "alice");
        assert_eq!(
            config.ports,
            PortSet::default(),
            "ports should fall back to defaults"
        );
        assert!(config.sharing_enabled);
        assert!(config.auto_accept_files);
        assert_eq!(config.share_fraction, DEFAULT_SHARE_FRACTION);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut config = Config::generate();
        config.aliases.insert("peer-1".into(), "Studio Mac".into());
        config.save(&path).unwrap();

        assert!(!path.with_extension("json.tmp").exists());
        let reloaded = Config::load_or_create(&path).unwrap();
        assert_eq!(
            reloaded.aliases.get("peer-1").map(String::as_str),
            Some("Studio Mac")
        );
    }

    #[test]
    fn the_sharing_policy_reflects_the_config() {
        let mut config = Config::generate();
        config.sharing_enabled = false;
        config.share_fraction = 0.5;

        let policy = config.sharing_policy();
        assert!(!policy.enabled);
        assert_eq!(policy.fraction, 0.5);
    }
}
