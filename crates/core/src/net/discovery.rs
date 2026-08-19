//! Zero-configuration LAN peer discovery.
//!
//! Every [`BEACON_INTERVAL_SECS`] a node sends a [`Beacon`] to
//!
//! * `255.255.255.255` — limited broadcast, what v2/v3 used,
//! * each interface's subnet-directed broadcast, e.g. `192.168.1.255`,
//! * the multicast group `239.255.42.99`.
//!
//! Sending to all three is deliberate. Limited broadcast alone is what made
//! discovery unreliable in both legacy trees: a great many Wi-Fi drivers and
//! access points silently drop `255.255.255.255` while happily forwarding
//! subnet-directed broadcast, and most enterprise gear drops both but forwards
//! multicast. Sending all three costs well under 1 KiB every two seconds and
//! turns "check your router" into "it just works".
//!
//! The listener binds with `SO_REUSEADDR` (plus `SO_REUSEPORT` on Unix) so
//! several nodes can share a machine — needed for the integration tests, and
//! for anyone running two nodes side by side.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;

use crate::net::iface;
use crate::net::peers::PeerRegistry;
use crate::net::protocol::{
    Beacon, PortSet, SharedResources, Telemetry, BEACON_INTERVAL_SECS, DISCOVERY_MAGIC,
    DISCOVERY_MULTICAST_V4, MAX_DATAGRAM, PROTOCOL_VERSION,
};
use crate::util::identity::Identity;

/// How often stale peers are re-evaluated.
const PRUNE_INTERVAL_SECS: u64 = 3;

/// Everything the beacon builder needs, refreshed from the node on each tick.
#[derive(Clone, Debug)]
pub struct BeaconSource {
    /// A watch channel rather than a value, so `/me <name>` takes effect on
    /// the very next beacon without restarting discovery.
    pub identity: watch::Receiver<Identity>,
    pub ports: PortSet,
    pub telemetry: watch::Receiver<Telemetry>,
    pub shared: watch::Receiver<SharedResources>,
}

impl BeaconSource {
    fn build(&self) -> Beacon {
        let telemetry = self.telemetry.borrow().clone();
        let identity = self.identity.borrow().clone();
        Beacon {
            magic: DISCOVERY_MAGIC.to_string(),
            version: PROTOCOL_VERSION,
            node_id: identity.node_id.clone(),
            username: identity.username.clone(),
            device_type: identity.device_type.clone(),
            os: identity.os.clone(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            ip: iface::local_ip_string(),
            ports: self.ports,
            // Populate the legacy flat fields too, so an un-upgraded v3 node
            // on the same LAN still shows live numbers for us.
            cpu_usage: Some(telemetry.cpu_usage),
            ram_available_mb: Some(telemetry.ram_available_mb()),
            telemetry,
            shared: self.shared.borrow().clone(),
            timestamp: Utc::now(),
        }
    }
}

/// Bind a UDP socket that can coexist with other listeners on the same port.
fn bind_reusable(port: u16) -> Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create the discovery socket")?;

    socket
        .set_reuse_address(true)
        .context("failed to set SO_REUSEADDR")?;
    // On Unix, SO_REUSEADDR alone does not let two processes bind the same
    // UDP port; SO_REUSEPORT does. Windows has no equivalent and does not
    // need one, since its SO_REUSEADDR already permits this.
    #[cfg(unix)]
    socket
        .set_reuse_port(true)
        .context("failed to set SO_REUSEPORT")?;

    socket
        .set_broadcast(true)
        .context("failed to enable broadcast")?;
    socket
        .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
        .with_context(|| format!("failed to bind UDP port {port} for discovery"))?;
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

/// Join the discovery multicast group on every usable interface.
///
/// Joining per-interface rather than on `INADDR_ANY` matters on machines with
/// several adapters (Wi-Fi plus a VPN, plus Docker or WSL virtual switches),
/// where the kernel's default choice is frequently the wrong one.
fn join_multicast(socket: &std::net::UdpSocket) -> usize {
    let group = Ipv4Addr::from(DISCOVERY_MULTICAST_V4);
    let mut joined = 0;

    let mut interfaces: Vec<Ipv4Addr> = iface::usable_interfaces().iter().map(|i| i.ip).collect();
    // Also ask the kernel to pick, as a safety net on unusual setups.
    interfaces.push(Ipv4Addr::UNSPECIFIED);

    for addr in interfaces {
        match socket.join_multicast_v4(&group, &addr) {
            Ok(()) => joined += 1,
            // Already-joined and unsupported-interface are both routine.
            Err(e) => tracing::debug!(interface = %addr, error = %e, "multicast join skipped"),
        }
    }

    joined
}

/// Run discovery until `shutdown` fires. Never returns on its own.
pub async fn run(
    source: BeaconSource,
    registry: PeerRegistry,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<()> {
    let our_node_id = source.identity.borrow().node_id.clone();
    let port = source.ports.discovery;

    let listener = bind_reusable(port)?;
    let joined = join_multicast(&listener);
    let listener = UdpSocket::from_std(listener)?;

    let sender = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    sender.set_broadcast(true)?;
    // TTL 1 confines beacons to the local segment: Bifrost is LAN-only by
    // design and beacons must never be routed off it.
    let _ = sender.set_multicast_ttl_v4(1);
    let _ = sender.set_ttl(1);

    tracing::info!(port, multicast_interfaces = joined, "discovery listening");

    let multicast_target = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::from(DISCOVERY_MULTICAST_V4),
        port,
    ));

    let mut beacon_tick = tokio::time::interval(Duration::from_secs(BEACON_INTERVAL_SECS));
    let mut prune_tick = tokio::time::interval(Duration::from_secs(PRUNE_INTERVAL_SECS));
    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        tokio::select! {
            _ = beacon_tick.tick() => {
                if let Err(e) = send_beacon(&sender, &source, port, multicast_target).await {
                    tracing::warn!(error = %e, "beacon send failed");
                }
            }
            _ = prune_tick.tick() => {
                registry.prune().await;
            }
            result = listener.recv_from(&mut buf) => {
                match result {
                    Ok((len, from)) => {
                        handle_datagram(&buf[..len], from, &our_node_id, &registry).await;
                    }
                    Err(e) => {
                        // On Windows an ICMP port-unreachable from a previous
                        // send surfaces as a recv error (WSAECONNRESET). It is
                        // not fatal and must not kill the listener.
                        tracing::debug!(error = %e, "discovery recv error, continuing");
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("discovery shutting down");
                return Ok(());
            }
        }
    }
}

/// Send one beacon to every broadcast target plus the multicast group.
async fn send_beacon(
    socket: &UdpSocket,
    source: &BeaconSource,
    port: u16,
    multicast_target: SocketAddr,
) -> Result<()> {
    let beacon = source.build();
    let bytes = serde_json::to_vec(&beacon).context("failed to serialise the beacon")?;

    let mut targets = iface::beacon_targets();
    targets.push(multicast_target.ip());

    let mut delivered = 0usize;
    let mut last_error = None;

    for target in targets {
        let addr = SocketAddr::new(target, port);
        match socket.send_to(&bytes, addr).await {
            Ok(_) => delivered += 1,
            // A target being unreachable is expected: an interface may be down
            // or a subnet may have no listeners. Only a total failure matters.
            Err(e) => {
                tracing::trace!(%addr, error = %e, "beacon target unreachable");
                last_error = Some(e);
            }
        }
    }

    if delivered == 0 {
        if let Some(e) = last_error {
            return Err(e).context("every beacon target failed");
        }
    }

    tracing::trace!(delivered, bytes = bytes.len(), "beacon sent");
    Ok(())
}

/// Parse one datagram and fold it into the registry.
async fn handle_datagram(
    data: &[u8],
    from: SocketAddr,
    our_node_id: &str,
    registry: &PeerRegistry,
) {
    // Anything unparseable is other traffic on a shared port, not an error.
    let Ok(beacon) = serde_json::from_slice::<Beacon>(data) else {
        return;
    };

    if !beacon.is_valid() {
        tracing::debug!(%from, "ignoring a foreign or unsupported beacon");
        return;
    }

    // Our own beacons come back to us: we broadcast, and we are on the LAN.
    if beacon.node_id == our_node_id {
        return;
    }

    registry.observe_beacon(&beacon, Some(from.ip())).await;
}

/// Send a single beacon immediately, outside the periodic loop.
///
/// Used at startup and on shutdown so peers see a node appear and disappear
/// without waiting out a full interval.
pub async fn announce_once(source: &BeaconSource) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let _ = socket.set_multicast_ttl_v4(1);

    let port = source.ports.discovery;
    let multicast = SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::from(DISCOVERY_MULTICAST_V4),
        port,
    ));
    send_beacon(&socket, source, port, multicast).await
}

/// Diagnostic summary of the local network, used by `bifrost doctor`.
pub fn network_report() -> NetworkReport {
    let interfaces = iface::usable_interfaces();
    NetworkReport {
        chosen_ip: iface::local_ipv4(),
        multicast_group: Ipv4Addr::from(DISCOVERY_MULTICAST_V4),
        broadcast_targets: iface::beacon_targets(),
        interfaces,
    }
}

/// What `bifrost doctor` prints about this machine's networking.
#[derive(Debug, Clone)]
pub struct NetworkReport {
    pub chosen_ip: Ipv4Addr,
    pub multicast_group: Ipv4Addr,
    pub broadcast_targets: Vec<IpAddr>,
    pub interfaces: Vec<iface::Interface>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sockets_can_share_the_discovery_port() {
        // Underpins running several nodes on one machine, and the
        // multi-node integration tests.
        let a = bind_reusable(0).expect("first bind");
        let port = a.local_addr().unwrap().port();
        let b = bind_reusable(port).expect("second bind on the same port");

        assert_eq!(
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port()
        );
    }

    #[tokio::test]
    async fn a_node_ignores_its_own_beacon() {
        let registry = PeerRegistry::new();
        let beacon = Beacon {
            magic: DISCOVERY_MAGIC.to_string(),
            version: PROTOCOL_VERSION,
            node_id: "me".into(),
            username: "me".into(),
            device_type: "linux-laptop".into(),
            os: "linux".into(),
            app_version: "1.0.0".into(),
            ip: "192.168.1.5".into(),
            ports: PortSet::default(),
            telemetry: Telemetry::default(),
            shared: SharedResources::default(),
            timestamp: Utc::now(),
            cpu_usage: None,
            ram_available_mb: None,
        };
        let raw = serde_json::to_vec(&beacon).unwrap();
        let from: SocketAddr = "192.168.1.5:9001".parse().unwrap();

        handle_datagram(&raw, from, "me", &registry).await;
        assert_eq!(
            registry.total_count().await,
            0,
            "own beacon must be ignored"
        );

        handle_datagram(&raw, from, "someone-else", &registry).await;
        assert_eq!(registry.total_count().await, 1);
    }

    #[tokio::test]
    async fn unrelated_udp_traffic_is_ignored_quietly() {
        let registry = PeerRegistry::new();
        let from: SocketAddr = "192.168.1.9:9001".parse().unwrap();

        for junk in [
            &b"not json at all"[..],
            b"{}",
            br#"{"magic":"OTHER","version":2}"#,
        ] {
            handle_datagram(junk, from, "me", &registry).await;
        }
        assert_eq!(registry.total_count().await, 0);
    }
}
