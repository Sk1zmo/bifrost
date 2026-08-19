//! Network interface enumeration.
//!
//! Used for two things: picking the LAN IP we advertise, and computing every
//! address a discovery beacon should be sent to.
//!
//! `Bifrost v2` and `v3` both sent beacons only to `255.255.255.255`. That is
//! the limited broadcast address, which is never forwarded and which a lot of
//! Wi-Fi drivers and APs drop outright — the "peers not appearing" entry in
//! both trees' troubleshooting docs. Sending additionally to each interface's
//! *subnet-directed* broadcast address and to a multicast group is what makes
//! discovery work without configuration.

use std::net::{IpAddr, Ipv4Addr};

use if_addrs::{IfAddr, Ifv4Addr};

/// A usable IPv4 interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    /// Subnet-directed broadcast address, e.g. `192.168.1.255`.
    pub broadcast: Ipv4Addr,
    /// True when the name looks like a wireless adapter.
    pub is_wireless: bool,
}

impl Interface {
    /// `"Wi-Fi"` or `"Ethernet"`, for display.
    pub fn kind(&self) -> &'static str {
        if self.is_wireless {
            "Wi-Fi"
        } else {
            "Ethernet"
        }
    }
}

/// Derive the subnet-directed broadcast address from an address and mask.
fn broadcast_of(ip: Ipv4Addr, mask: Ipv4Addr) -> Ipv4Addr {
    let ip = u32::from(ip);
    let mask = u32::from(mask);
    Ipv4Addr::from(ip | !mask)
}

fn looks_wireless(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["wi-fi", "wifi", "wlan", "wlp", "airport", "en0", "ath"]
        .iter()
        .any(|p| n.contains(p))
}

/// Every non-loopback IPv4 interface with a usable address.
///
/// Link-local `169.254.0.0/16` addresses are kept: two laptops with a direct
/// cable and no DHCP server self-assign into that range, and Bifrost should
/// still connect them.
pub fn usable_interfaces() -> Vec<Interface> {
    let addrs = match if_addrs::get_if_addrs() {
        Ok(addrs) => addrs,
        Err(e) => {
            tracing::warn!(error = %e, "could not enumerate network interfaces");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for iface in addrs {
        if iface.is_loopback() {
            continue;
        }
        let IfAddr::V4(Ifv4Addr { ip, netmask, .. }) = iface.addr else {
            // IPv6 is not used for discovery; beacons are IPv4-only.
            continue;
        };
        if ip.is_unspecified() {
            continue;
        }

        out.push(Interface {
            is_wireless: looks_wireless(&iface.name),
            name: iface.name,
            ip,
            netmask,
            broadcast: broadcast_of(ip, netmask),
        });
    }

    // Prefer wired interfaces, then lower IPs, so the advertised address is
    // stable across restarts rather than dependent on enumeration order.
    out.sort_by_key(|i| (i.is_wireless, u32::from(i.ip)));
    out
}

/// Best guess at the address other machines should reach us on.
///
/// Prefers a real private-range LAN address over link-local or CGNAT ranges,
/// and falls back to loopback so a single-machine test still runs.
pub fn local_ipv4() -> Ipv4Addr {
    let ifaces = usable_interfaces();

    let rank = |ip: &Ipv4Addr| -> u8 {
        let o = ip.octets();
        match () {
            // Ordinary home/office LANs.
            _ if o[0] == 192 && o[1] == 168 => 0,
            _ if o[0] == 10 => 1,
            _ if o[0] == 172 && (16..=31).contains(&o[1]) => 2,
            // Carrier-grade NAT: possible but unusual on a LAN.
            _ if o[0] == 100 && (64..=127).contains(&o[1]) => 3,
            // Self-assigned, e.g. a direct cable with no DHCP.
            _ if ip.is_link_local() => 4,
            _ => 5,
        }
    };

    ifaces
        .iter()
        .min_by_key(|i| (rank(&i.ip), i.is_wireless, u32::from(i.ip)))
        .map(|i| i.ip)
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

/// [`local_ipv4`] as a string.
pub fn local_ip_string() -> String {
    local_ipv4().to_string()
}

/// How reachable `candidate` looks from this machine.
///
/// Returns the prefix length of the most specific local subnet that contains
/// it, or `None` if no local interface shares a subnet with it.
///
/// This is how a peer's real LAN address is told apart from an overlay one.
/// A machine with Wi-Fi on `192.168.29.0/24` and a VPN on `26.0.0.0/8` hears
/// the same peer beacon on both; the `/24` match is far more likely to be the
/// physical LAN than the `/8` overlay, so the longer prefix wins. Without
/// this, whichever beacon happened to arrive last would set the peer's
/// address, and connecting to it would fail whenever the overlay does not
/// actually route between the two machines.
pub fn reachability_prefix(candidate: Ipv4Addr) -> Option<u8> {
    let candidate = u32::from(candidate);

    usable_interfaces()
        .iter()
        .filter_map(|iface| {
            let mask = u32::from(iface.netmask);
            // A /32 covers only ourselves and can never match a peer.
            if mask == u32::MAX {
                return None;
            }
            (u32::from(iface.ip) & mask == candidate & mask).then(|| mask.count_ones() as u8)
        })
        .max()
}

/// Every IPv4 address a discovery beacon should be sent to.
///
/// Ordered limited-broadcast first, then one subnet-directed broadcast per
/// interface. The caller adds the multicast group separately, since that is
/// sent through a differently-configured socket.
pub fn beacon_targets() -> Vec<IpAddr> {
    let mut targets = vec![IpAddr::V4(Ipv4Addr::BROADCAST)];

    for iface in usable_interfaces() {
        let bcast = IpAddr::V4(iface.broadcast);
        // A /32 broadcasts to itself; skip that and any duplicate subnet.
        if iface.broadcast != iface.ip && !targets.contains(&bcast) {
            targets.push(bcast);
        }
    }

    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_address_matches_the_mask() {
        let cases = [
            ([192, 168, 1, 42], [255, 255, 255, 0], [192, 168, 1, 255]),
            ([10, 0, 5, 7], [255, 0, 0, 0], [10, 255, 255, 255]),
            ([172, 16, 4, 9], [255, 255, 0, 0], [172, 16, 255, 255]),
            // /23 spans two octets of host space.
            ([192, 168, 2, 10], [255, 255, 254, 0], [192, 168, 3, 255]),
        ];

        for (ip, mask, expected) in cases {
            assert_eq!(
                broadcast_of(Ipv4Addr::from(ip), Ipv4Addr::from(mask)),
                Ipv4Addr::from(expected),
                "for {ip:?}/{mask:?}"
            );
        }
    }

    #[test]
    fn wireless_names_are_recognised() {
        for name in ["Wi-Fi", "wlan0", "wlp3s0", "WiFi 2"] {
            assert!(looks_wireless(name), "{name} should read as wireless");
        }
        for name in ["eth0", "enp0s31f6", "Ethernet 2"] {
            assert!(!looks_wireless(name), "{name} should read as wired");
        }
    }

    #[test]
    fn beacon_targets_always_include_limited_broadcast() {
        // Runs on any host, with or without a live network.
        let targets = beacon_targets();
        assert!(targets.contains(&IpAddr::V4(Ipv4Addr::BROADCAST)));

        let unique: std::collections::HashSet<_> = targets.iter().collect();
        assert_eq!(unique.len(), targets.len(), "targets must not repeat");
    }

    #[test]
    fn local_ipv4_is_never_unspecified() {
        assert!(!local_ipv4().is_unspecified());
    }

    #[test]
    fn our_own_address_is_reachable_from_ourselves() {
        // Whatever this machine's networking looks like, its own LAN address
        // must sit inside one of its own subnets.
        let ip = local_ipv4();
        if !ip.is_loopback() {
            assert!(
                reachability_prefix(ip).is_some(),
                "{ip} should match one of this machine's own subnets"
            );
        }
    }

    #[test]
    fn an_address_on_no_local_subnet_is_unreachable() {
        // Reserved for documentation (RFC 5737); no real interface uses it.
        assert_eq!(reachability_prefix(Ipv4Addr::new(203, 0, 113, 42)), None);
    }
}
