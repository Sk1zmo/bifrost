//! Persisted configuration at `~/.bifrost/config.json`.
//!
//! Written atomically: a crash mid-save must never leave a node with an
//! unreadable config and therefore a brand-new identity, which would orphan
//! every alias its peers had for it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::compute::TaskDefinition;
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

    // ── Shared compute ──
    /// Run work for other machines at all.
    ///
    /// Off by default. Chat and files are open to the LAN because they cost
    /// nothing but bytes; compute spends someone's electricity, memory and
    /// time, so it is opened deliberately.
    #[serde(default)]
    pub compute_enabled: bool,

    /// Require a peer to pair before it may submit work.
    ///
    /// On by default whenever compute is enabled. Turning it off opens this
    /// machine's declared tasks to anyone on the network.
    #[serde(default = "default_true")]
    pub compute_require_pairing: bool,

    /// Jobs this node will run at once.
    #[serde(default = "default_max_jobs")]
    pub compute_max_jobs: usize,

    /// Place work across the network, rather than only running it locally.
    #[serde(default = "default_true")]
    pub orchestrator_enabled: bool,

    /// Tokens earned by pairing with other devices, keyed by their node id.
    ///
    /// Persisted so pairing survives a restart; losing them would mean
    /// re-reading a code off every machine after every reboot.
    #[serde(default)]
    pub compute_tokens: HashMap<String, String>,

    /// The tasks this machine will run for others.
    ///
    /// The template is the whole permission: a peer names one of these and
    /// fills its placeholders, and can do nothing else. See
    /// [`TaskDefinition`] for the shape.
    #[serde(default)]
    pub tasks: HashMap<String, TaskDefinition>,
}

fn default_true() -> bool {
    true
}

fn default_share_fraction() -> f32 {
    DEFAULT_SHARE_FRACTION
}

fn default_max_jobs() -> usize {
    2
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
        Self::generate_in(&Self::base_dir())
    }

    /// A fresh config whose directories live under `base`.
    ///
    /// The caller's chosen location is the anchor, not a global home
    /// directory. Android and iOS have no home directory at all: `dirs`
    /// returns nothing, `base_dir` falls back to `.`, and the working
    /// directory on both is read-only — so a node generated the old way died
    /// on first launch with "cannot create ./.bifrost/downloads: Read-only
    /// file system". Deriving from wherever the config itself is going means
    /// an app that passes its own private directory gets a config entirely
    /// inside it.
    pub fn generate_in(base: &Path) -> Self {
        // Desktops have a real Downloads folder and a user who expects files
        // to land there. Mobile has app-private storage and nothing else
        // writable without extra permissions, so the base wins.
        let download_dir = if cfg!(any(target_os = "android", target_os = "ios")) {
            base.join("downloads")
        } else {
            dirs::download_dir()
                .map(|d| d.join("Bifrost"))
                .unwrap_or_else(|| base.join("downloads"))
        };

        Self {
            identity: Identity::generate(),
            ports: PortSet::default(),
            download_dir,
            data_dir: base.join("data"),
            aliases: HashMap::new(),
            sharing_enabled: true,
            share_fraction: DEFAULT_SHARE_FRACTION,
            auto_accept_files: true,
            max_incoming_file_bytes: None,
            web_enabled: true,
            web_lan_accessible: true,
            compute_enabled: false,
            compute_require_pairing: true,
            compute_max_jobs: default_max_jobs(),
            orchestrator_enabled: true,
            compute_tokens: HashMap::new(),
            tasks: HashMap::new(),
        }
    }

    /// Load from `path`, creating a default config if it does not exist.
    ///
    /// A config that exists but cannot be parsed is moved aside rather than
    /// overwritten, so a hand-edit with a stray comma is recoverable.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        // Whoever chose where the config lives has also chosen where its data
        // belongs — on mobile that is the only writable place there is.
        let base = path.parent().unwrap_or_else(|| Path::new("."));

        if !path.exists() {
            let config = Self::generate_in(base);
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
                let config = Self::generate_in(base);
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
    fn a_generated_config_keeps_its_data_beside_itself() {
        // The failure this guards against: on Android there is no home
        // directory, so a config anchored to one lands on a read-only path and
        // the node dies on first launch.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");

        let config = Config::load_or_create(&path).unwrap();

        assert!(
            config.data_dir.starts_with(dir.path()),
            "data_dir {} escaped the config's own directory",
            config.data_dir.display()
        );
        // The directories must actually be creatable there.
        config.ensure_dirs().unwrap();
        assert!(config.data_dir.exists());
    }

    #[test]
    fn generate_in_anchors_everything_to_the_given_base() {
        let base = std::path::Path::new("/somewhere/private");
        let config = Config::generate_in(base);

        assert_eq!(config.data_dir, base.join("data"));
        // Downloads may legitimately point at the desktop Downloads folder,
        // but must never fall back to a bare relative path.
        assert!(
            config.download_dir.is_absolute(),
            "download_dir {} is relative, which is read-only on mobile",
            config.download_dir.display()
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
