//! In-memory registry of every node seen on the LAN.
//!
//! Cloneable and internally synchronised (`Arc<RwLock<…>>`), so every service
//! and both dashboards share one view without threading a lock through call
//! signatures.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::net::protocol::{
    Beacon, PortSet, SharedResources, Telemetry, PEER_FORGET_SECS, PEER_STALE_SECS,
};

/// A node other than us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub node_id: String,
    pub username: String,
    /// Locally-assigned nickname, persisted in `config.json`.
    pub alias: Option<String>,
    pub ip: String,
    pub device_type: String,
    pub os: String,
    pub app_version: String,
    pub ports: PortSet,
    pub telemetry: Telemetry,
    pub shared: SharedResources,
    /// What this peer will run for others.
    #[serde(default)]
    pub compute: crate::compute::protocol::Capabilities,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub online: bool,
    /// Beacons received. Useful for spotting a flapping Wi-Fi link.
    pub beacons_seen: u64,
    /// Round-trip time of the last successful ping, in milliseconds.
    pub latency_ms: Option<f64>,
}

impl Peer {
    /// Alias if one is set, otherwise the username.
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.username)
    }

    /// Address for the chat/control service.
    pub fn messaging_addr(&self) -> Option<SocketAddr> {
        format!("{}:{}", self.ip, self.ports.messaging).parse().ok()
    }

    /// Address for the file-transfer service.
    pub fn file_addr(&self) -> Option<SocketAddr> {
        format!("{}:{}", self.ip, self.ports.files).parse().ok()
    }

    /// Address for the voice signalling service.
    pub fn voice_signal_addr(&self) -> Option<SocketAddr> {
        format!("{}:{}", self.ip, self.ports.voice_signal)
            .parse()
            .ok()
    }

    /// Address for the voice audio stream.
    pub fn voice_audio_addr(&self) -> Option<SocketAddr> {
        format!("{}:{}", self.ip, self.ports.voice_audio)
            .parse()
            .ok()
    }

    /// URL of this peer's own web dashboard.
    pub fn dashboard_url(&self) -> String {
        format!("http://{}:{}", self.ip, self.ports.web)
    }

    pub fn seconds_since_seen(&self) -> i64 {
        (Utc::now() - self.last_seen).num_seconds()
    }
}

/// Thread-safe map of `node_id` → [`Peer`].
#[derive(Clone, Debug, Default)]
pub struct PeerRegistry {
    peers: Arc<RwLock<HashMap<String, Peer>>>,
    /// Aliases restored from `config.json`.
    ///
    /// Held here rather than applied once at startup, because a peer is
    /// usually discovered *after* the node boots: applying the saved names
    /// only to peers already present would silently drop the name the user
    /// gave a machine every time either end restarted.
    saved_aliases: Arc<RwLock<HashMap<String, String>>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or refresh a peer from a beacon.
    ///
    /// `source_ip` is the UDP source address. It wins over the `ip` the peer
    /// claims, because a multi-homed sender routinely advertises the wrong
    /// interface and we know for certain which address its packet arrived
    /// from. Both legacy trees trusted the claimed field and so produced peers
    /// that appeared in the list but could never be connected to.
    pub async fn observe_beacon(&self, beacon: &Beacon, source_ip: Option<std::net::IpAddr>) {
        let now = Utc::now();
        let ip = source_ip
            .map(|i| i.to_string())
            .unwrap_or_else(|| beacon.ip.clone());
        let telemetry = beacon.effective_telemetry();

        let mut peers = self.peers.write().await;
        match peers.get_mut(&beacon.node_id) {
            Some(peer) => {
                if !peer.online {
                    tracing::info!(peer = %peer.display_name(), "peer back online");
                }
                peer.username = beacon.username.clone();
                if address_is_at_least_as_good(&ip, &peer.ip) {
                    peer.ip = ip;
                }
                peer.device_type = beacon.device_type.clone();
                peer.os = beacon.os.clone();
                peer.app_version = beacon.app_version.clone();
                peer.ports = beacon.ports;
                peer.telemetry = telemetry;
                peer.shared = beacon.shared.clone();
                peer.compute = beacon.compute.clone();
                peer.last_seen = now;
                peer.online = true;
                peer.beacons_seen += 1;
            }
            None => {
                tracing::info!(
                    node_id = %beacon.node_id,
                    username = %beacon.username,
                    %ip,
                    "discovered a new peer"
                );
                let alias = self
                    .saved_aliases
                    .read()
                    .await
                    .get(&beacon.node_id)
                    .cloned();
                peers.insert(
                    beacon.node_id.clone(),
                    Peer {
                        node_id: beacon.node_id.clone(),
                        username: beacon.username.clone(),
                        alias,
                        ip,
                        device_type: beacon.device_type.clone(),
                        os: beacon.os.clone(),
                        app_version: beacon.app_version.clone(),
                        ports: beacon.ports,
                        telemetry,
                        shared: beacon.shared.clone(),
                        compute: beacon.compute.clone(),
                        first_seen: now,
                        last_seen: now,
                        online: true,
                        beacons_seen: 1,
                        latency_ms: None,
                    },
                );
            }
        }
    }

    /// Assign a local nickname. Returns false if the peer is unknown.
    ///
    /// The name is remembered for this node id, so it is reapplied if the peer
    /// goes away and comes back.
    pub async fn set_alias(&self, node_id: &str, alias: Option<String>) -> bool {
        let alias = alias.filter(|a| !a.trim().is_empty());

        {
            let mut saved = self.saved_aliases.write().await;
            match &alias {
                Some(name) => saved.insert(node_id.to_string(), name.clone()),
                None => saved.remove(node_id),
            };
        }

        match self.peers.write().await.get_mut(node_id) {
            Some(peer) => {
                peer.alias = alias;
                true
            }
            None => false,
        }
    }

    /// Load saved aliases from `config.json`.
    ///
    /// Applies them to any peer already known, and keeps them so peers
    /// discovered later are named too.
    pub async fn apply_saved_aliases(&self, aliases: &HashMap<String, String>) {
        *self.saved_aliases.write().await = aliases.clone();

        let mut peers = self.peers.write().await;
        for (node_id, alias) in aliases {
            if let Some(peer) = peers.get_mut(node_id) {
                peer.alias = Some(alias.clone());
            }
        }
    }

    pub async fn record_latency(&self, node_id: &str, latency_ms: f64) {
        if let Some(peer) = self.peers.write().await.get_mut(node_id) {
            peer.latency_ms = Some(latency_ms);
        }
    }

    pub async fn get(&self, node_id: &str) -> Option<Peer> {
        self.peers.read().await.get(node_id).cloned()
    }

    /// Look a peer up by full node id, alias, or username — case-insensitive,
    /// and by unique node-id prefix, so `/sendfile 550e84 file.txt` works
    /// instead of demanding a full UUID.
    pub async fn resolve(&self, needle: &str) -> Option<Peer> {
        let needle = needle.trim();
        if needle.is_empty() {
            return None;
        }
        let peers = self.peers.read().await;

        if let Some(peer) = peers.get(needle) {
            return Some(peer.clone());
        }

        let lower = needle.to_ascii_lowercase();
        let exact_name: Vec<&Peer> = peers
            .values()
            .filter(|p| {
                p.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(needle))
                    || p.username.eq_ignore_ascii_case(needle)
            })
            .collect();
        if exact_name.len() == 1 {
            return Some(exact_name[0].clone());
        }

        // Ambiguous prefixes resolve to nothing rather than to an arbitrary
        // peer — sending a file to the wrong machine is worse than an error.
        let by_prefix: Vec<&Peer> = peers
            .values()
            .filter(|p| p.node_id.to_ascii_lowercase().starts_with(&lower))
            .collect();
        if by_prefix.len() == 1 {
            return Some(by_prefix[0].clone());
        }

        None
    }

    /// All peers, online first, then most recently seen.
    pub async fn list(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self.peers.read().await.values().cloned().collect();
        peers.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then(b.last_seen.cmp(&a.last_seen))
                .then(a.node_id.cmp(&b.node_id))
        });
        peers
    }

    pub async fn online(&self) -> Vec<Peer> {
        self.peers
            .read()
            .await
            .values()
            .filter(|p| p.online)
            .cloned()
            .collect()
    }

    pub async fn online_count(&self) -> usize {
        self.peers
            .read()
            .await
            .values()
            .filter(|p| p.online)
            .count()
    }

    pub async fn total_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// Mark silent peers offline, and drop long-gone ones.
    ///
    /// Going offline is kept separate from being forgotten so a peer that
    /// sleeps and wakes keeps its alias and history instead of arriving as a
    /// brand-new stranger.
    pub async fn prune(&self) {
        let now = Utc::now();
        let stale = Duration::seconds(PEER_STALE_SECS);
        let forget = Duration::seconds(PEER_FORGET_SECS);

        let mut peers = self.peers.write().await;
        for peer in peers.values_mut() {
            if peer.online && now - peer.last_seen > stale {
                tracing::info!(peer = %peer.display_name(), "peer went offline");
                peer.online = false;
            }
        }
        peers.retain(|_, p| p.online || now - p.last_seen <= forget);
    }

    /// Aggregate resources across this LAN, for the dashboard header.
    pub async fn pool_totals(&self) -> PoolTotals {
        let peers = self.peers.read().await;
        let mut totals = PoolTotals::default();

        for peer in peers.values().filter(|p| p.online) {
            totals.nodes += 1;
            totals.cpu_cores += peer.telemetry.cpu_cores;
            totals.ram_total_mb += peer.telemetry.ram_total_mb;
            totals.ram_available_mb += peer.telemetry.ram_available_mb();
            totals.disk_total_gb += peer.telemetry.disk_total_gb;
            totals.disk_available_gb += peer.telemetry.disk_available_gb();
            if peer.shared.enabled {
                totals.sharing_nodes += 1;
                totals.shared_ram_mb += peer.shared.ram_offered_mb;
                totals.shared_disk_gb += peer.shared.disk_offered_gb;
            }
        }

        totals
    }
}

/// Is `candidate` at least as reachable as `current`?
///
/// One peer's beacons arrive over every shared network, so on a machine with
/// a VPN or a Docker bridge the same peer is seen at several addresses. Taking
/// whichever arrived last would let an overlay address that does not actually
/// route displace the working LAN one. Ranking by how specific the matching
/// local subnet is keeps the physical LAN address; ties go to the newer
/// observation, so a peer that genuinely moves networks still follows.
fn address_is_at_least_as_good(candidate: &str, current: &str) -> bool {
    if candidate == current {
        return true;
    }

    let rank = |raw: &str| {
        raw.parse::<std::net::Ipv4Addr>()
            .ok()
            .and_then(crate::net::iface::reachability_prefix)
    };

    match (rank(candidate), rank(current)) {
        (Some(new), Some(old)) => new >= old,
        // A candidate on one of our subnets always beats one that is on none.
        (Some(_), None) => true,
        (None, Some(_)) => false,
        // Neither is on a local subnet — nothing to choose between them, so
        // take the newer one.
        (None, None) => true,
    }
}

/// Summed resources over every online node, including this one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolTotals {
    pub nodes: usize,
    pub sharing_nodes: usize,
    pub cpu_cores: usize,
    pub ram_total_mb: u64,
    pub ram_available_mb: u64,
    pub disk_total_gb: f64,
    pub disk_available_gb: f64,
    pub shared_ram_mb: u64,
    pub shared_disk_gb: f64,
}

impl PoolTotals {
    /// Fold this node's own numbers into the totals.
    pub fn add_self(&mut self, telemetry: &Telemetry, shared: &SharedResources) {
        self.nodes += 1;
        self.cpu_cores += telemetry.cpu_cores;
        self.ram_total_mb += telemetry.ram_total_mb;
        self.ram_available_mb += telemetry.ram_available_mb();
        self.disk_total_gb += telemetry.disk_total_gb;
        self.disk_available_gb += telemetry.disk_available_gb();
        if shared.enabled {
            self.sharing_nodes += 1;
            self.shared_ram_mb += shared.ram_offered_mb;
            self.shared_disk_gb += shared.disk_offered_gb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::protocol::{DISCOVERY_MAGIC, PROTOCOL_VERSION};

    fn beacon(node_id: &str, username: &str) -> Beacon {
        Beacon {
            magic: DISCOVERY_MAGIC.to_string(),
            version: PROTOCOL_VERSION,
            node_id: node_id.to_string(),
            username: username.to_string(),
            device_type: "linux-laptop".to_string(),
            os: "linux".to_string(),
            app_version: "1.0.0".to_string(),
            ip: "192.168.1.50".to_string(),
            ports: PortSet::default(),
            telemetry: Telemetry::default(),
            shared: SharedResources::default(),
            compute: Default::default(),
            timestamp: Utc::now(),
            cpu_usage: None,
            ram_available_mb: None,
        }
    }

    #[tokio::test]
    async fn beacons_insert_then_refresh_rather_than_duplicate() {
        let reg = PeerRegistry::new();
        reg.observe_beacon(&beacon("node-a", "alice"), None).await;
        reg.observe_beacon(&beacon("node-a", "alice-renamed"), None)
            .await;

        assert_eq!(reg.total_count().await, 1);
        let peer = reg.get("node-a").await.unwrap();
        assert_eq!(peer.username, "alice-renamed");
        assert_eq!(peer.beacons_seen, 2);
    }

    #[test]
    fn a_more_specific_subnet_wins_and_an_overlay_cannot_displace_it() {
        // This machine's own address is by definition on one of its subnets,
        // so it must beat an address that is on none of them.
        let ours = crate::net::iface::local_ipv4().to_string();
        if crate::net::iface::reachability_prefix(ours.parse().unwrap()).is_some() {
            assert!(address_is_at_least_as_good(&ours, "203.0.113.9"));
            assert!(!address_is_at_least_as_good("203.0.113.9", &ours));
        }

        // Same address is always acceptable.
        assert!(address_is_at_least_as_good("192.168.1.5", "192.168.1.5"));
        // Neither reachable: the newer observation wins.
        assert!(address_is_at_least_as_good("203.0.113.9", "198.51.100.4"));
    }

    #[tokio::test]
    async fn source_address_overrides_the_advertised_one() {
        let reg = PeerRegistry::new();
        // Peer claims a VPN address but its packet came from the LAN.
        let mut b = beacon("node-a", "alice");
        b.ip = "10.8.0.3".to_string();
        reg.observe_beacon(&b, Some("192.168.1.77".parse().unwrap()))
            .await;

        assert_eq!(reg.get("node-a").await.unwrap().ip, "192.168.1.77");
    }

    #[tokio::test]
    async fn resolve_accepts_id_prefix_alias_and_username() {
        let reg = PeerRegistry::new();
        reg.observe_beacon(&beacon("550e8400-e29b", "alice"), None)
            .await;
        reg.observe_beacon(&beacon("660f9500-f30c", "bob"), None)
            .await;
        reg.set_alias("660f9500-f30c", Some("Bob's ThinkPad".into()))
            .await;

        assert_eq!(
            reg.resolve("550e8400-e29b").await.unwrap().username,
            "alice"
        );
        assert_eq!(reg.resolve("550e").await.unwrap().username, "alice");
        assert_eq!(reg.resolve("ALICE").await.unwrap().node_id, "550e8400-e29b");
        assert_eq!(reg.resolve("Bob's ThinkPad").await.unwrap().username, "bob");
        assert!(reg.resolve("nobody").await.is_none());
    }

    #[tokio::test]
    async fn an_ambiguous_prefix_resolves_to_nothing() {
        let reg = PeerRegistry::new();
        reg.observe_beacon(&beacon("abc-111", "alice"), None).await;
        reg.observe_beacon(&beacon("abc-222", "bob"), None).await;

        // Matching either peer would risk sending a file to the wrong machine.
        assert!(reg.resolve("abc").await.is_none());
        assert!(reg.resolve("abc-111").await.is_some());
    }

    #[tokio::test]
    async fn pruning_marks_offline_before_forgetting() {
        let reg = PeerRegistry::new();
        reg.observe_beacon(&beacon("node-a", "alice"), None).await;

        // Silent for longer than the stale window but well inside the forget
        // window: still listed, no longer online.
        {
            let mut peers = reg.peers.write().await;
            let peer = peers.get_mut("node-a").unwrap();
            peer.last_seen = Utc::now() - Duration::seconds(PEER_STALE_SECS + 5);
        }
        reg.prune().await;
        assert_eq!(reg.total_count().await, 1);
        assert_eq!(reg.online_count().await, 0);
        assert!(!reg.get("node-a").await.unwrap().online);

        // Past the forget window: dropped entirely.
        {
            let mut peers = reg.peers.write().await;
            peers.get_mut("node-a").unwrap().last_seen =
                Utc::now() - Duration::seconds(PEER_FORGET_SECS + 5);
        }
        reg.prune().await;
        assert_eq!(reg.total_count().await, 0);
    }

    #[tokio::test]
    async fn a_saved_alias_is_applied_to_a_peer_discovered_later() {
        // The order that matters in practice: a node restarts, loads its
        // saved names, and only then hears the peer beacon.
        let reg = PeerRegistry::new();
        let saved = HashMap::from([("node-a".to_string(), "Workshop NUC".to_string())]);
        reg.apply_saved_aliases(&saved).await;

        reg.observe_beacon(&beacon("node-a", "alice"), None).await;

        assert_eq!(
            reg.get("node-a").await.unwrap().display_name(),
            "Workshop NUC"
        );
    }

    #[tokio::test]
    async fn clearing_an_alias_forgets_it_for_next_time() {
        let reg = PeerRegistry::new();
        reg.observe_beacon(&beacon("node-a", "alice"), None).await;
        reg.set_alias("node-a", Some("Temporary".into())).await;
        reg.set_alias("node-a", None).await;

        // Rediscovering must not resurrect the cleared name.
        reg.observe_beacon(&beacon("node-a", "alice"), None).await;
        assert_eq!(reg.get("node-a").await.unwrap().display_name(), "alice");
    }

    #[tokio::test]
    async fn a_returning_peer_keeps_its_alias() {
        let reg = PeerRegistry::new();
        reg.observe_beacon(&beacon("node-a", "alice"), None).await;
        reg.set_alias("node-a", Some("Studio Mac".into())).await;

        {
            let mut peers = reg.peers.write().await;
            peers.get_mut("node-a").unwrap().last_seen =
                Utc::now() - Duration::seconds(PEER_STALE_SECS + 5);
        }
        reg.prune().await;
        reg.observe_beacon(&beacon("node-a", "alice"), None).await;

        let peer = reg.get("node-a").await.unwrap();
        assert!(peer.online);
        assert_eq!(peer.display_name(), "Studio Mac");
    }

    #[tokio::test]
    async fn pool_totals_count_only_online_peers() {
        let reg = PeerRegistry::new();
        for (id, ram) in [("a", 8192u64), ("b", 16384)] {
            let mut b = beacon(id, id);
            b.telemetry.ram_total_mb = ram;
            b.telemetry.cpu_cores = 4;
            reg.observe_beacon(&b, None).await;
        }
        {
            let mut peers = reg.peers.write().await;
            peers.get_mut("b").unwrap().online = false;
        }

        let totals = reg.pool_totals().await;
        assert_eq!(totals.nodes, 1);
        assert_eq!(totals.ram_total_mb, 8192);
        assert_eq!(totals.cpu_cores, 4);
    }
}
