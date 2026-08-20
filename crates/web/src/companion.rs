//! Companion devices — phones and tablets as real peers, with no app.
//!
//! A browser cannot be a Bifrost peer. Web pages cannot open UDP sockets or
//! listen on TCP ports, so a phone loading the dashboard is only ever a remote
//! control for the laptop serving it: it does not beacon, it is not discovered,
//! and it does not appear in anyone's device list. That is a platform limit,
//! not a design choice, and no amount of work on the page changes it.
//!
//! A companion closes that gap. When a phone first opens the dashboard it
//! claims one, and the host process starts a **complete [`Node`]** for it —
//! its own identity, its own ports, its own discovery beacons, its own file
//! transfer service. To every other machine on the network it is simply
//! another device: it shows up in `/peers`, messages from it are from *it*,
//! and files can be sent to it by name.
//!
//! What is genuinely different from a native app:
//!
//! * its sockets live in the host laptop's process, so it is present only
//!   while that laptop is running Bifrost
//! * files sent to it land on the host, and the phone pulls them over HTTP
//! * it reports no CPU or memory of its own, because a browser cannot see them
//!
//! Those are the honest costs of needing nothing installed. A native client is
//! the answer when they matter.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bifrost_core::net::protocol::PortSet;
use bifrost_core::{Config, Node};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// A companion with no HTTP activity for this long is shut down.
///
/// Generous: a phone with its screen off stops polling, and coming back to a
/// tab an hour later should find the device still on the network rather than
/// silently gone.
const IDLE_TIMEOUT_MINUTES: i64 = 90;

/// Upper bound on companions per host, so a misbehaving script cannot exhaust
/// the port range.
const MAX_COMPANIONS: usize = 16;

/// What the browser is told about its device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanionInfo {
    /// Doubles as the session token. A UUID, and this is a LAN tool with no
    /// accounts, so knowing it is the same thing as being on the network.
    pub id: String,
    pub node_id: String,
    pub name: String,
    pub device_type: String,
    pub ports: PortSet,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

struct Companion {
    info: CompanionInfo,
    node: Node,
}

/// Every companion this host is running.
#[derive(Clone, Default)]
pub struct CompanionManager {
    inner: Arc<RwLock<HashMap<String, Companion>>>,
}

impl std::fmt::Debug for CompanionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanionManager").finish_non_exhaustive()
    }
}

impl CompanionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where a companion's identity and downloads live.
    ///
    /// Persisted per companion so a phone that reconnects keeps the same node
    /// id — and therefore the alias every other machine gave it.
    fn directory(id: &str) -> PathBuf {
        Config::base_dir().join("companions").join(id)
    }

    /// Start a companion, or revive one whose id the browser still holds.
    pub async fn claim(
        &self,
        host: &Node,
        requested_id: Option<String>,
        name: &str,
        device_type: &str,
    ) -> Result<CompanionInfo> {
        // An id the browser already has: either it is still running, or it was
        // pruned and we bring the same identity back.
        if let Some(id) = &requested_id {
            if let Some(companion) = self.inner.read().await.get(id) {
                return Ok(companion.info.clone());
            }
        }

        {
            let existing = self.inner.read().await;
            if existing.len() >= MAX_COMPANIONS {
                anyhow::bail!("this host is already running {MAX_COMPANIONS} companion devices");
            }
        }

        let id = requested_id.unwrap_or_else(new_id);
        let directory = Self::directory(&id);
        let config_path = directory.join("config.json");

        // A companion must share the host's discovery port, or it would be on
        // a different network and invisible to everyone.
        let ports = allocate_ports(host.ports().discovery, &self.used_ports().await)
            .context("no free ports left for another companion device")?;

        let mut config = Config::load_or_create(&config_path)?;
        config.identity.username = name.trim().to_string();
        config.identity.device_type = device_type.to_string();
        config.identity.os = "browser".to_string();
        config.ports = ports;
        config.download_dir = directory.join("downloads");
        config.data_dir = directory.join("data");
        // A phone has no spare capacity to lend, and advertising some would
        // skew the pool totals every other device sees.
        config.sharing_enabled = false;
        config.web_enabled = false;
        config.save(&config_path)?;

        let node = Node::start(config.clone(), &config_path)
            .await
            .with_context(|| format!("could not start a companion device for \"{name}\""))?;

        let info = CompanionInfo {
            id: id.clone(),
            node_id: node.node_id(),
            name: config.identity.username.clone(),
            device_type: device_type.to_string(),
            ports,
            created_at: Utc::now(),
            last_seen: Utc::now(),
        };

        tracing::info!(
            companion = %info.name,
            node_id = %info.node_id,
            messaging = ports.messaging,
            "companion device joined the network"
        );

        self.inner.write().await.insert(
            id,
            Companion {
                info: info.clone(),
                node,
            },
        );

        Ok(info)
    }

    /// The node for a companion, marking it active.
    pub async fn node(&self, id: &str) -> Option<Node> {
        let mut companions = self.inner.write().await;
        let companion = companions.get_mut(id)?;
        companion.info.last_seen = Utc::now();
        Some(companion.node.clone())
    }

    pub async fn info(&self, id: &str) -> Option<CompanionInfo> {
        self.inner.read().await.get(id).map(|c| c.info.clone())
    }

    pub async fn list(&self) -> Vec<CompanionInfo> {
        let mut all: Vec<CompanionInfo> = self
            .inner
            .read()
            .await
            .values()
            .map(|c| c.info.clone())
            .collect();
        all.sort_by_key(|c| c.created_at);
        all
    }

    /// Shut a companion down and take it off the network.
    pub async fn release(&self, id: &str) -> bool {
        let companion = self.inner.write().await.remove(id);
        match companion {
            Some(companion) => {
                tracing::info!(companion = %companion.info.name, "companion device left");
                companion.node.shutdown().await;
                true
            }
            None => false,
        }
    }

    /// Every port any live companion holds, so a new one does not collide.
    async fn used_ports(&self) -> Vec<PortSet> {
        self.inner
            .read()
            .await
            .values()
            .map(|c| c.info.ports)
            .collect()
    }

    /// Shut down companions whose browser has stopped polling.
    pub async fn prune_idle(&self) {
        let cutoff = Utc::now() - chrono::Duration::minutes(IDLE_TIMEOUT_MINUTES);

        let stale: Vec<String> = self
            .inner
            .read()
            .await
            .values()
            .filter(|c| c.info.last_seen < cutoff)
            .map(|c| c.info.id.clone())
            .collect();

        for id in stale {
            tracing::info!(%id, "companion device idle, shutting it down");
            self.release(&id).await;
        }
    }

    /// Stop every companion, e.g. when the host is shutting down.
    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = self.inner.read().await.keys().cloned().collect();
        for id in ids {
            self.release(&id).await;
        }
    }
}

/// Find a free port set for a companion, sharing `discovery` with the host.
///
/// Ports are probed rather than assumed: the host may already be offset, other
/// companions hold their own, and something unrelated may sit on any of them.
fn allocate_ports(discovery: u16, taken: &[PortSet]) -> Option<PortSet> {
    // Start well clear of the default block so a companion never collides with
    // a second real node someone started with --port-offset.
    for offset in (100..=900).step_by(10) {
        let candidate = PortSet {
            web: 0, // companions serve no dashboard of their own
            discovery,
            messaging: 9002 + offset,
            files: 9003 + offset,
            voice_signal: 9004 + offset,
            voice_audio: 9005 + offset,
            compute: 9006 + offset,
        };

        let clashes = taken.iter().any(|t| {
            t.messaging == candidate.messaging
                || t.files == candidate.files
                || t.voice_signal == candidate.voice_signal
                || t.voice_audio == candidate.voice_audio
        });
        if clashes {
            continue;
        }

        let free = is_tcp_free(candidate.messaging)
            && is_tcp_free(candidate.files)
            && is_tcp_free(candidate.voice_signal)
            && is_udp_free(candidate.voice_audio);

        if free {
            return Some(candidate);
        }
    }

    None
}

fn is_tcp_free(port: u16) -> bool {
    std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).is_ok()
}

fn is_udp_free(port: u16) -> bool {
    std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).is_ok()
}

/// A fresh companion id, which doubles as the browser's session token.
///
/// A real v4 UUID rather than anything derived from the clock: Windows'
/// timer resolution is coarse enough that two phones joining in the same
/// instant would otherwise be handed the same id, and with it each other's
/// device.
fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Guess a device type from the browser's `User-Agent`.
///
/// Only used for the icon and the default name, so a wrong guess is cosmetic —
/// the user renames it in the join prompt either way.
pub fn device_type_from_user_agent(user_agent: &str) -> &'static str {
    let ua = user_agent.to_ascii_lowercase();

    // iPadOS reports itself as a Mac, distinguishable only by touch support,
    // which is not in the header. "ipad" first catches the older string.
    if ua.contains("ipad") {
        return "tablet";
    }
    if ua.contains("iphone") || ua.contains("ipod") {
        return "iphone";
    }
    if ua.contains("android") {
        // Android tablets omit "mobile".
        return if ua.contains("mobile") {
            "android-phone"
        } else {
            "tablet"
        };
    }
    if ua.contains("windows") {
        return "windows-pc";
    }
    if ua.contains("mac os") || ua.contains("macintosh") {
        return "mac";
    }
    if ua.contains("linux") || ua.contains("x11") {
        return "linux-laptop";
    }

    "device"
}

/// A sensible default name for a device, from its `User-Agent`.
pub fn default_name_from_user_agent(user_agent: &str) -> String {
    match device_type_from_user_agent(user_agent) {
        "iphone" => "iPhone",
        "tablet" if user_agent.to_ascii_lowercase().contains("ipad") => "iPad",
        "tablet" => "Tablet",
        "android-phone" => "Android phone",
        "mac" => "Mac",
        "windows-pc" => "Windows PC",
        "linux-laptop" => "Linux laptop",
        _ => "Companion device",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agents_map_to_the_right_device() {
        let cases = [
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Safari",
                "iphone",
            ),
            (
                "Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X) Safari",
                "tablet",
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) Mobile Safari",
                "android-phone",
            ),
            ("Mozilla/5.0 (Linux; Android 14; SM-X200) Safari", "tablet"),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome",
                "windows-pc",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Safari",
                "mac",
            ),
            ("Mozilla/5.0 (X11; Linux x86_64) Chrome", "linux-laptop"),
            ("something-unrecognised", "device"),
        ];

        for (agent, expected) in cases {
            assert_eq!(device_type_from_user_agent(agent), expected, "for {agent}");
        }
    }

    #[test]
    fn android_phones_are_told_apart_from_android_tablets() {
        // The only signal is the "Mobile" token; tablets omit it.
        assert_eq!(
            device_type_from_user_agent("Android 14; Pixel 8 Mobile Safari"),
            "android-phone"
        );
        assert_eq!(
            device_type_from_user_agent("Android 14; SM-X200 Safari"),
            "tablet"
        );
    }

    #[test]
    fn default_names_are_human() {
        assert_eq!(
            default_name_from_user_agent("(iPhone; CPU iPhone OS 17_0)"),
            "iPhone"
        );
        assert_eq!(default_name_from_user_agent("(iPad; CPU OS 17_0)"), "iPad");
        assert_eq!(
            default_name_from_user_agent("(Linux; Android 14; Pixel 8) Mobile"),
            "Android phone"
        );
        assert!(!default_name_from_user_agent("").is_empty());
    }

    #[test]
    fn allocated_ports_share_discovery_and_avoid_collisions() {
        let host_discovery = 9001;
        let first = allocate_ports(host_discovery, &[]).expect("a first companion should fit");

        // It must be on the host's discovery port, or it is on another network.
        assert_eq!(first.discovery, host_discovery);
        // And clear of the defaults a real second node would use.
        assert!(first.messaging >= 9102, "got {}", first.messaging);

        let second = allocate_ports(host_discovery, &[first]).expect("a second should fit too");
        assert_ne!(first.messaging, second.messaging);
        assert_ne!(first.files, second.files);
        assert_ne!(first.voice_audio, second.voice_audio);
    }

    #[test]
    fn generated_ids_do_not_repeat() {
        // Two phones joining in the same instant must not be handed the same
        // device; anything clock-derived would collide here on Windows.
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000, "companion ids must be unique");
        assert!(ids
            .iter()
            .all(|id| id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())));
    }
}
