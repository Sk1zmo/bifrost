//! Finding devices that are *not* yet running Bifrost.
//!
//! Discovery ([`super::discovery`]) answers "who else is on this network
//! already?" — it hears beacons, so it only ever finds Bifrost. That is the
//! wrong question when you have just installed it on one laptop and want to
//! know what else is around to install it on, or which machine has the GPU.
//!
//! This module answers the other question, by looking in four places at once:
//!
//! | Where | Finds | Costs |
//! |---|---|---|
//! | The peer registry | Bifrost nodes, and what each offers | nothing |
//! | The neighbour table | Everything this machine has recently talked to | one command |
//! | A TCP probe of the local subnet | Bifrost nodes whose beacons were dropped | 254 connects |
//! | mDNS | Phones, printers, TVs, and anything else advertising itself | one packet |
//! | The platform's Bluetooth list | Devices this machine has paired with | one command |
//!
//! # What this is not
//!
//! It is **not** a security scanner and does not try to be. It sends one TCP
//! connection to one port, and one standard mDNS query — the same traffic any
//! phone on the network generates constantly. Nothing is fingerprinted and
//! nothing is probed for weakness.
//!
//! Bluetooth is **listed, not scanned**. Bifrost's mesh cannot run over BLE:
//! the peripheral role needed to advertise is unavailable on desktop platforms
//! through any portable Rust crate, so a BLE-carried mesh would work on
//! precisely the devices that already have Wi-Fi. What the list is good for is
//! answering "is that machine here?" — a device paired over Bluetooth is a
//! device you own and could install Bifrost on.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::net::iface::usable_interfaces;
use crate::net::peers::Peer;

/// How long to wait for a TCP connection before deciding nothing is there.
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Connections in flight at once during a subnet sweep.
///
/// High enough to finish a /24 in about two seconds, low enough that a laptop
/// Wi-Fi driver does not start dropping frames under its own scan.
const PROBE_CONCURRENCY: usize = 48;

/// How long to listen for mDNS answers after asking.
const MDNS_WINDOW: Duration = Duration::from_millis(2500);

/// The largest subnet worth sweeping, as a prefix length.
///
/// A /16 is 65,534 addresses; sweeping one would take minutes and flood the
/// network. Anything larger than a /22 is left to the passive sources.
const MIN_SWEEPABLE_PREFIX: u32 = 22;

/// Where a device was found. A device can be found several ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// It is beaconing: a Bifrost node.
    Bifrost,
    /// It answered on a Bifrost port, but has not been heard beaconing.
    Port,
    /// It is in this machine's ARP or neighbour table.
    Neighbour,
    /// It advertises a service over mDNS.
    Mdns,
    /// The platform lists it as a known Bluetooth device.
    Bluetooth,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Bifrost => "running Bifrost",
            Self::Port => "answered on a Bifrost port",
            Self::Neighbour => "seen on this network",
            Self::Mdns => "advertising itself",
            Self::Bluetooth => "paired over Bluetooth",
        }
    }
}

/// Something found on, or near, this network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoundDevice {
    /// An IPv4 address, or a Bluetooth address where there is no IP.
    pub address: String,
    /// The friendliest name available, where anything offered one.
    pub name: Option<String>,
    /// Hardware address, where the neighbour table gave one.
    pub hardware: Option<String>,
    /// Every way this device was found, so the evidence is visible.
    pub sources: Vec<Source>,
    /// Services it advertises, or other detail worth showing.
    pub details: Vec<String>,
    /// It is a Bifrost node right now.
    pub running_bifrost: bool,
    /// It is a Bifrost node *and* is offering its hardware.
    pub sharing_compute: bool,
}

impl FoundDevice {
    /// One line describing what this is, for a terminal or a tooltip.
    pub fn summary(&self) -> String {
        if self.sharing_compute {
            return "running Bifrost, sharing its hardware".to_string();
        }
        if self.running_bifrost {
            return "running Bifrost".to_string();
        }
        match self.sources.first() {
            Some(source) => source.label().to_string(),
            None => "seen on this network".to_string(),
        }
    }

    /// Is this worth suggesting the user install Bifrost on?
    pub fn is_a_candidate(&self) -> bool {
        !self.running_bifrost
    }
}

/// The machine running the scan, when a node is running on it.
///
/// Passed in rather than detected: a scan that reports every device on the
/// network except the one you are sitting at reads as a bug, and the address
/// sweep deliberately skips this machine's own address, so it would never find
/// itself.
#[derive(Debug, Clone)]
pub struct ThisDevice {
    pub address: String,
    pub name: String,
    pub sharing_compute: bool,
}

/// The result of one sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReport {
    pub devices: Vec<FoundDevice>,
    /// Subnets that were swept, for the report to be honest about coverage.
    pub swept: Vec<String>,
    /// Anything that could not be done, and why. Never silently skipped.
    pub skipped: Vec<String>,
    pub scanned_for_secs: f64,
}

impl ScanReport {
    pub fn running_bifrost(&self) -> usize {
        self.devices.iter().filter(|d| d.running_bifrost).count()
    }

    pub fn candidates(&self) -> usize {
        self.devices.iter().filter(|d| d.is_a_candidate()).count()
    }
}

/// What to include in a sweep.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Probe every address on the local subnet. The slow part, and the part
    /// that finds a node whose beacons a hostile access point ate.
    pub sweep_subnet: bool,
    /// Read the platform's ARP or neighbour table.
    pub use_neighbours: bool,
    pub use_mdns: bool,
    pub use_bluetooth: bool,
    /// The port a probe connects to.
    pub bifrost_port: u16,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sweep_subnet: true,
            use_neighbours: true,
            use_mdns: true,
            use_bluetooth: true,
            bifrost_port: crate::net::protocol::PortSet::default().messaging,
        }
    }
}

/// Look everywhere, and merge what comes back.
///
/// `peers` is what discovery already knows; it is passed in rather than
/// re-derived so a scan reflects exactly what the rest of the node believes.
pub async fn scan(peers: &[Peer], this: Option<&ThisDevice>, options: &ScanOptions) -> ScanReport {
    let started = std::time::Instant::now();
    let mut report = ScanReport::default();

    // Keyed by address so the same machine found four ways is one entry.
    let mut found: BTreeMap<String, FoundDevice> = BTreeMap::new();

    // ── 1. What discovery already knows ─────────────────────────────────────
    for peer in peers.iter().filter(|p| p.online) {
        let entry = found.entry(peer.ip.clone()).or_insert_with(|| FoundDevice {
            address: peer.ip.clone(),
            name: None,
            hardware: None,
            sources: Vec::new(),
            details: Vec::new(),
            running_bifrost: false,
            sharing_compute: false,
        });
        entry.name = Some(peer.display_name().to_string());
        entry.running_bifrost = true;
        entry.sharing_compute = peer.compute.enabled;
        note(entry, Source::Bifrost);
        entry
            .details
            .push(format!("{} · {}", peer.device_type, peer.os));
        if peer.compute.enabled {
            if let Some(gpu) = peer.compute.accelerator() {
                entry.details.push(gpu.summary());
            }
        }
    }

    // This machine, if a node is running on it.
    if let Some(this) = this {
        let entry = found
            .entry(this.address.clone())
            .or_insert_with(|| FoundDevice {
                address: this.address.clone(),
                name: None,
                hardware: None,
                sources: Vec::new(),
                details: Vec::new(),
                running_bifrost: false,
                sharing_compute: false,
            });
        entry.name = Some(this.name.clone());
        entry.running_bifrost = true;
        entry.sharing_compute = this.sharing_compute;
        entry.details.push("this device".to_string());
        note(entry, Source::Bifrost);
    }

    // ── 2. The neighbour table ──────────────────────────────────────────────
    if options.use_neighbours {
        match neighbours().await {
            Ok(entries) => {
                for (ip, mac) in entries {
                    let entry = found.entry(ip.clone()).or_insert_with(|| FoundDevice {
                        address: ip.clone(),
                        name: None,
                        hardware: None,
                        sources: Vec::new(),
                        details: Vec::new(),
                        running_bifrost: false,
                        sharing_compute: false,
                    });
                    entry.hardware.get_or_insert(mac);
                    note(entry, Source::Neighbour);
                }
            }
            Err(e) => report.skipped.push(format!("neighbour table: {e}")),
        }
    }

    // ── 3. Sweep the subnet for Bifrost ─────────────────────────────────────
    if options.sweep_subnet {
        for interface in usable_interfaces() {
            let prefix = prefix_len(interface.netmask);
            if prefix < MIN_SWEEPABLE_PREFIX {
                report.skipped.push(format!(
                    "{} is a /{prefix}, too large to sweep; using passive sources only",
                    interface.name
                ));
                continue;
            }

            let addresses = subnet_addresses(interface.ip, interface.netmask);
            if addresses.is_empty() {
                continue;
            }
            report.swept.push(format!(
                "{}/{prefix} ({} addresses)",
                interface.ip,
                addresses.len()
            ));

            for ip in probe_many(&addresses, options.bifrost_port).await {
                let key = ip.to_string();
                let entry = found.entry(key.clone()).or_insert_with(|| FoundDevice {
                    address: key,
                    name: None,
                    hardware: None,
                    sources: Vec::new(),
                    details: Vec::new(),
                    running_bifrost: false,
                    sharing_compute: false,
                });
                entry.running_bifrost = true;
                note(entry, Source::Port);
            }
        }
    }

    // ── 4. mDNS ─────────────────────────────────────────────────────────────
    if options.use_mdns {
        match mdns_sweep().await {
            Ok(answers) => {
                for (ip, services) in answers {
                    let key = ip.to_string();
                    let entry = found.entry(key.clone()).or_insert_with(|| FoundDevice {
                        address: key,
                        name: None,
                        hardware: None,
                        sources: Vec::new(),
                        details: Vec::new(),
                        running_bifrost: false,
                        sharing_compute: false,
                    });
                    note(entry, Source::Mdns);
                    for service in services {
                        if entry.name.is_none() {
                            entry.name = friendly_name(&service);
                        }
                        if !entry.details.contains(&service) {
                            entry.details.push(service);
                        }
                    }
                }
            }
            Err(e) => report.skipped.push(format!("mDNS: {e}")),
        }
    }

    // ── 5. Bluetooth ────────────────────────────────────────────────────────
    if options.use_bluetooth {
        match bluetooth_devices().await {
            Ok(devices) => {
                for (address, name) in devices {
                    let entry = found.entry(address.clone()).or_insert_with(|| FoundDevice {
                        address: address.clone(),
                        name: None,
                        hardware: Some(address.clone()),
                        sources: Vec::new(),
                        details: Vec::new(),
                        running_bifrost: false,
                        sharing_compute: false,
                    });
                    entry.name.get_or_insert(name);
                    note(entry, Source::Bluetooth);
                }
            }
            Err(e) => report.skipped.push(format!("Bluetooth: {e}")),
        }
    }

    // Bifrost nodes, then everything with an address on this network, then
    // Bluetooth — which is a different kind of thing and belongs at the end
    // rather than interleaved by the accident of a MAC sorting before a "1".
    let mut devices: Vec<FoundDevice> = found.into_values().collect();
    devices.sort_by(|a, b| {
        b.running_bifrost
            .cmp(&a.running_bifrost)
            .then(sort_key(a).cmp(&sort_key(b)))
    });

    report.devices = devices;
    report.scanned_for_secs = started.elapsed().as_secs_f64();
    report
}

/// Orders addresses the way they read: IPv4 numerically, then everything else
/// alphabetically after it.
///
/// Sorting `192.168.29.119` and `192.168.29.46` as text puts them the wrong
/// way round, because "1" precedes "4".
fn sort_key(device: &FoundDevice) -> (u8, u32, String) {
    match device.address.parse::<Ipv4Addr>() {
        Ok(ip) => (0, u32::from_be_bytes(ip.octets()), String::new()),
        Err(_) => (1, 0, device.address.clone()),
    }
}

fn note(device: &mut FoundDevice, source: Source) {
    if !device.sources.contains(&source) {
        device.sources.push(source);
        device.sources.sort();
    }
}

// ─── The subnet sweep ────────────────────────────────────────────────────────

fn prefix_len(netmask: Ipv4Addr) -> u32 {
    u32::from_be_bytes(netmask.octets()).count_ones()
}

/// Every host address in the interface's subnet, excluding network, broadcast
/// and the interface itself.
fn subnet_addresses(ip: Ipv4Addr, netmask: Ipv4Addr) -> Vec<Ipv4Addr> {
    let address = u32::from_be_bytes(ip.octets());
    let mask = u32::from_be_bytes(netmask.octets());
    let network = address & mask;
    let broadcast = network | !mask;

    if broadcast.saturating_sub(network) < 2 {
        return Vec::new();
    }

    ((network + 1)..broadcast)
        .filter(|candidate| *candidate != address)
        .map(|candidate| Ipv4Addr::from(candidate.to_be_bytes()))
        .collect()
}

/// Connect to `port` on every address, bounded, and return what answered.
///
/// A `JoinSet` topped back up to the concurrency limit, rather than one task
/// per address: a /24 is 253 sockets, and opening them all at once is how you
/// make a laptop's Wi-Fi driver start dropping frames mid-scan.
async fn probe_many(addresses: &[Ipv4Addr], port: u16) -> Vec<Ipv4Addr> {
    let mut answered = Vec::new();
    let mut running = tokio::task::JoinSet::new();
    let mut queue = addresses.iter().copied();

    let probe = |ip: Ipv4Addr| async move {
        let target = SocketAddr::new(IpAddr::V4(ip), port);
        match timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(target)).await {
            Ok(Ok(_)) => Some(ip),
            _ => None,
        }
    };

    for ip in queue.by_ref().take(PROBE_CONCURRENCY) {
        running.spawn(probe(ip));
    }

    while let Some(finished) = running.join_next().await {
        if let Ok(Some(ip)) = finished {
            answered.push(ip);
        }
        if let Some(next) = queue.next() {
            running.spawn(probe(next));
        }
    }

    answered.sort();
    answered
}

// ─── mDNS ────────────────────────────────────────────────────────────────────

/// Ask the network what it is advertising, and listen briefly.
///
/// One standard query for `_services._dns-sd._udp.local`, which is how every
/// mDNS implementation asks "what service types exist here?". Answers name
/// the service; the source address names the device.
async fn mdns_sweep() -> std::io::Result<Vec<(Ipv4Addr, Vec<String>)>> {
    const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
    const MDNS_PORT: u16 = 5353;

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_multicast_ttl_v4(255)?;

    let query = build_query("_services._dns-sd._udp.local");
    socket
        .send_to(&query, SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT))
        .await?;

    // Also ask for the two service types that name ordinary computers, since
    // plenty of stacks answer those and ignore the meta-query.
    for service in ["_workstation._tcp.local", "_device-info._tcp.local"] {
        let _ = socket
            .send_to(
                &build_query(service),
                SocketAddr::new(IpAddr::V4(MDNS_GROUP), MDNS_PORT),
            )
            .await;
    }

    let mut answers: BTreeMap<Ipv4Addr, Vec<String>> = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + MDNS_WINDOW;
    let mut buffer = vec![0u8; 4096];

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, socket.recv_from(&mut buffer)).await {
            Ok(Ok((length, from))) => {
                let IpAddr::V4(ip) = from.ip() else { continue };
                for name in parse_answer_names(&buffer[..length]) {
                    let entry = answers.entry(ip).or_default();
                    if !entry.contains(&name) {
                        entry.push(name);
                    }
                }
            }
            // A malformed or unreachable answer is not a reason to abandon the
            // whole sweep; the window simply runs out.
            Ok(Err(_)) => continue,
            Err(_) => break,
        }
    }

    Ok(answers.into_iter().collect())
}

/// A standard DNS query packet for one PTR record.
fn build_query(name: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(64);
    packet.extend_from_slice(&[0, 0]); // id: 0, as mDNS ignores it
    packet.extend_from_slice(&[0, 0]); // flags: a standard query
    packet.extend_from_slice(&[0, 1]); // one question
    packet.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // no answers, authority, extra

    for label in name.split('.').filter(|l| !l.is_empty()) {
        // A label is length-prefixed with one byte, so it cannot exceed 63.
        let bytes = &label.as_bytes()[..label.len().min(63)];
        packet.push(bytes.len() as u8);
        packet.extend_from_slice(bytes);
    }
    packet.push(0); // end of name
    packet.extend_from_slice(&[0, 12]); // PTR
                                        // Class IN with the top bit set: RFC 6762's "unicast reply, please".
                                        //
                                        // Without it, responders answer to the multicast group, and a socket on an
                                        // ephemeral port that has not joined that group never sees a word of it —
                                        // which is exactly how this first scanned a live network and reported no
                                        // mDNS at all.
    packet.extend_from_slice(&[0x80, 1]);
    packet
}

/// Pull the record names out of a DNS response.
///
/// Only enough parsing to read names: the questions are skipped, then each
/// record's name is read and its body stepped over. Compression pointers are
/// followed, with a hard limit, because a packet can point at itself.
fn parse_answer_names(packet: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if packet.len() < 12 {
        return names;
    }

    let counts: Vec<usize> = (1..=3)
        .map(|i| u16::from_be_bytes([packet[i * 2 + 2], packet[i * 2 + 3]]) as usize)
        .collect();
    let questions = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let records: usize = counts.iter().sum();

    let mut cursor = 12;

    for _ in 0..questions {
        let Some(next) = skip_name(packet, cursor) else {
            return names;
        };
        cursor = next + 4; // type and class
        if cursor > packet.len() {
            return names;
        }
    }

    for _ in 0..records.min(64) {
        let Some((name, after)) = read_name(packet, cursor) else {
            break;
        };
        if after + 10 > packet.len() {
            break;
        }
        let length = u16::from_be_bytes([packet[after + 8], packet[after + 9]]) as usize;
        let body = after + 10;

        // A PTR record's body is itself a name, and that is the interesting
        // one: it names the instance rather than the service type.
        if u16::from_be_bytes([packet[after], packet[after + 1]]) == 12 {
            if let Some((target, _)) = read_name(packet, body) {
                if !target.is_empty() && !names.contains(&target) {
                    names.push(target);
                }
            }
        } else if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }

        cursor = body + length;
        if cursor > packet.len() {
            break;
        }
    }

    names
}

/// Step over a name without decoding it.
fn skip_name(packet: &[u8], mut cursor: usize) -> Option<usize> {
    loop {
        let length = *packet.get(cursor)? as usize;
        if length == 0 {
            return Some(cursor + 1);
        }
        if length & 0xC0 == 0xC0 {
            return Some(cursor + 2); // a pointer ends the name
        }
        cursor += 1 + length;
    }
}

/// Decode a name, following compression pointers.
fn read_name(packet: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut cursor = start;
    let mut after = None;
    // Bounded so a packet whose pointer loops cannot hang the scan.
    let mut jumps = 0;

    loop {
        let length = *packet.get(cursor)? as usize;

        if length == 0 {
            let end = after.unwrap_or(cursor + 1);
            return Some((labels.join("."), end));
        }

        if length & 0xC0 == 0xC0 {
            let low = *packet.get(cursor + 1)? as usize;
            let target = ((length & 0x3F) << 8) | low;
            after.get_or_insert(cursor + 2);
            jumps += 1;
            if jumps > 16 || target >= packet.len() {
                return Some((labels.join("."), after.unwrap_or(cursor + 2)));
            }
            cursor = target;
            continue;
        }

        let from = cursor + 1;
        let to = from + length;
        let label = packet.get(from..to)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        cursor = to;
    }
}

/// `"Kitchen Echo._googlecast._tcp.local"` → `"Kitchen Echo"`.
fn friendly_name(service: &str) -> Option<String> {
    let instance = service.split("._").next()?.trim();
    if instance.is_empty() || instance.starts_with('_') {
        return None;
    }
    Some(instance.replace("\\032", " "))
}

// ─── The platform's own tables ───────────────────────────────────────────────

/// IP and hardware address of everything this machine has recently talked to.
async fn neighbours() -> anyhow::Result<Vec<(String, String)>> {
    let output = if cfg!(target_os = "linux") {
        run("ip", &["neigh", "show"])
            .await
            .or_else(|_| Ok::<String, anyhow::Error>(String::new()))?
    } else {
        String::new()
    };

    let text = if output.trim().is_empty() {
        run("arp", &["-a"]).await?
    } else {
        output
    };

    Ok(parse_neighbours(&text))
}

/// Parses both `ip neigh` and `arp -a`, which differ per platform.
///
/// Rather than three parsers, this looks for the two things every format
/// contains: something shaped like an IPv4 address, and something shaped like
/// a MAC. That survives the differences between Windows' table, BSD's
/// `host (1.2.3.4) at aa:bb:...` and Linux's `1.2.3.4 dev eth0 lladdr aa:bb`.
fn parse_neighbours(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();

    for line in text.lines() {
        let mut ip = None;
        let mut mac = None;

        for token in line.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            let token = token.trim_matches(|c: char| c == ',' || c == ';');
            if ip.is_none() && looks_like_ipv4(token) {
                ip = Some(token.to_string());
            } else if mac.is_none() && looks_like_mac(token) {
                mac = Some(token.to_ascii_lowercase().replace('-', ":"));
            }
        }

        if let (Some(ip), Some(mac)) = (ip, mac) {
            // Broadcast and multicast entries are not devices.
            if mac == "ff:ff:ff:ff:ff:ff" || ip.ends_with(".255") {
                continue;
            }
            if ip.starts_with("224.") || ip.starts_with("239.") {
                continue;
            }
            out.push((ip, mac));
        }
    }

    out.sort();
    out.dedup();
    out
}

fn looks_like_ipv4(token: &str) -> bool {
    token.parse::<Ipv4Addr>().is_ok()
}

fn looks_like_mac(token: &str) -> bool {
    let parts: Vec<&str> = token.split([':', '-']).collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Bluetooth devices the platform already knows about.
async fn bluetooth_devices() -> anyhow::Result<Vec<(String, String)>> {
    #[cfg(target_os = "windows")]
    {
        // Win32_PnPEntity is the only place a plain shell can see paired
        // devices; the friendly name and the address both live in its id.
        let script = "Get-CimInstance Win32_PnPEntity | \
             Where-Object { $_.PNPClass -eq 'Bluetooth' -and $_.DeviceID -like '*DEV_*' } | \
             ForEach-Object { \"$($_.DeviceID)|$($_.Name)\" }";
        let text = run("powershell", &["-NoProfile", "-Command", script]).await?;
        return Ok(parse_windows_bluetooth(&text));
    }

    #[cfg(target_os = "linux")]
    {
        let text = run("bluetoothctl", &["devices"]).await?;
        return Ok(parse_bluetoothctl(&text));
    }

    #[cfg(target_os = "macos")]
    {
        let text = run("system_profiler", &["SPBluetoothDataType"]).await?;
        return Ok(parse_system_profiler(&text));
    }

    #[allow(unreachable_code)]
    Ok(Vec::new())
}

/// `...DEV_A1B2C3D4E5F6\...|Someone's Headphones`
#[cfg(any(target_os = "windows", test))]
fn parse_windows_bluetooth(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((id, name)) = line.split_once('|') else {
            continue;
        };
        let Some(start) = id.to_ascii_uppercase().find("DEV_") else {
            continue;
        };
        let hex: String = id[start + 4..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() != 12 {
            continue;
        }
        let address = hex
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(":");
        out.push((address, name.trim().to_string()));
    }
    out.sort();
    out.dedup();
    out
}

/// `Device AA:BB:CC:DD:EE:FF Some Speaker`
#[cfg(any(target_os = "linux", test))]
fn parse_bluetoothctl(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Device ")?;
            let (address, name) = rest.split_once(' ')?;
            looks_like_mac(address).then(|| (address.to_ascii_lowercase(), name.trim().to_string()))
        })
        .collect()
}

/// An indented block per device, keyed by its name, with an Address field.
///
/// `system_profiler` nests headings — `Bluetooth:` → `Connected:` → the
/// device's own name — so the heading that names a device is simply the last
/// one before its `Address:`. A heading is a line ending in a colon with no
/// value after it; a line like `Minor Type: Speaker` is a field, not a name.
#[cfg(any(target_os = "macos", test))]
fn parse_system_profiler(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut heading: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        if let Some(address) = trimmed.strip_prefix("Address:") {
            let address = address.trim().to_ascii_lowercase().replace('-', ":");
            if looks_like_mac(&address) {
                if let Some(name) = heading.take() {
                    out.push((address, name));
                }
            }
        } else if trimmed.ends_with(':') && !trimmed.contains(": ") {
            heading = Some(trimmed.trim_end_matches(':').trim().to_string());
        }
    }
    out
}

/// Run a command and return its stdout, with a bound on how long it may take.
async fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    use anyhow::Context;

    let mut command = tokio::process::Command::new(program);
    command.args(args);
    command.stdin(std::process::Stdio::null());

    #[cfg(windows)]
    {
        // A console window flashing up during a scan would be alarming.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        tokio::process::Command::creation_flags(&mut command, CREATE_NO_WINDOW);
    }

    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| anyhow::anyhow!("{program} did not finish in time"))?
        .with_context(|| format!("could not run {program}"))?;

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subnet_excludes_its_network_broadcast_and_this_machine() {
        let addresses = subnet_addresses(
            Ipv4Addr::new(192, 168, 1, 24),
            Ipv4Addr::new(255, 255, 255, 0),
        );

        assert_eq!(addresses.len(), 253, "254 hosts, minus this machine");
        assert!(!addresses.contains(&Ipv4Addr::new(192, 168, 1, 0)));
        assert!(!addresses.contains(&Ipv4Addr::new(192, 168, 1, 255)));
        assert!(!addresses.contains(&Ipv4Addr::new(192, 168, 1, 24)));
        assert!(addresses.contains(&Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn a_point_to_point_subnet_has_nothing_to_sweep() {
        let addresses = subnet_addresses(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(255, 255, 255, 254),
        );
        assert!(addresses.is_empty());
    }

    #[test]
    fn every_platforms_neighbour_table_parses() {
        // Windows
        let windows = "\nInterface: 192.168.1.24 --- 0xa\n  \
                       Internet Address      Physical Address      Type\n  \
                       192.168.1.1           a4-2b-b0-11-22-33     dynamic\n  \
                       192.168.1.31          00-1a-2b-3c-4d-5e     dynamic\n  \
                       192.168.1.255         ff-ff-ff-ff-ff-ff     static\n";
        let parsed = parse_neighbours(windows);
        assert_eq!(parsed.len(), 2, "the broadcast entry is not a device");
        assert!(parsed.contains(&("192.168.1.1".into(), "a4:2b:b0:11:22:33".into())));

        // BSD and macOS
        let bsd = "? (192.168.1.1) at a4:2b:b0:11:22:33 on en0 ifscope [ethernet]\n\
                   ? (192.168.1.44) at 0:1a:2b:3c:4d:5e on en0 [ethernet]\n";
        let parsed = parse_neighbours(bsd);
        assert!(parsed.contains(&("192.168.1.1".into(), "a4:2b:b0:11:22:33".into())));

        // Linux
        let linux = "192.168.1.1 dev wlan0 lladdr a4:2b:b0:11:22:33 REACHABLE\n\
                     192.168.1.77 dev wlan0 FAILED\n";
        let parsed = parse_neighbours(linux);
        assert_eq!(
            parsed.len(),
            1,
            "an entry with no hardware address is not usable"
        );
    }

    #[test]
    fn a_dns_query_is_well_formed() {
        let query = build_query("_services._dns-sd._udp.local");

        assert_eq!(&query[4..6], &[0, 1], "exactly one question");
        assert_eq!(query[12], 9, "the first label is _services");
        assert_eq!(&query[13..22], b"_services");
        assert_eq!(
            &query[query.len() - 4..],
            &[0, 12, 0x80, 1],
            "PTR, class IN, unicast reply requested"
        );
    }

    #[test]
    fn a_response_yields_the_instance_names() {
        // One question, one PTR answer pointing at an instance, using a
        // compression pointer for the service suffix — as real responders do.
        let mut packet = vec![0, 0, 0x84, 0, 0, 1, 0, 1, 0, 0, 0, 0];
        let question_at = packet.len();
        for label in ["_workstation", "_tcp", "local"] {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&[0, 12, 0, 1]);

        // Answer: name is a pointer back to the question.
        packet.extend_from_slice(&[0xC0, question_at as u8]);
        packet.extend_from_slice(&[0, 12, 0, 1]); // PTR, IN
        packet.extend_from_slice(&[0, 0, 0, 60]); // TTL

        let mut body = Vec::new();
        body.push(9u8);
        body.extend_from_slice(b"kitchen-1");
        body.extend_from_slice(&[0xC0, question_at as u8]);
        packet.extend_from_slice(&(body.len() as u16).to_be_bytes());
        packet.extend_from_slice(&body);

        let names = parse_answer_names(&packet);
        assert_eq!(names, vec!["kitchen-1._workstation._tcp.local".to_string()]);
    }

    #[test]
    fn a_packet_whose_pointer_loops_does_not_hang() {
        // A name at offset 12 that points at itself.
        let mut packet = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        packet.extend_from_slice(&[0xC0, 12]);
        packet.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 0]);

        let _ = parse_answer_names(&packet);
    }

    #[test]
    fn an_instance_name_is_read_off_a_service() {
        assert_eq!(
            friendly_name("Kitchen\\032Speaker._googlecast._tcp.local"),
            Some("Kitchen Speaker".to_string())
        );
        assert_eq!(friendly_name("_workstation._tcp.local"), None);
    }

    #[test]
    fn each_platforms_bluetooth_listing_parses() {
        let windows = "BTHENUM\\DEV_A1B2C3D4E5F6\\7&x|Someone's Headphones\n\
                       BTHENUM\\{guid}\\junk|Not a device\n";
        assert_eq!(
            parse_windows_bluetooth(windows),
            vec![(
                "a1:b2:c3:d4:e5:f6".to_string(),
                "Someone's Headphones".to_string()
            )]
        );

        let linux = "Device AA:BB:CC:DD:EE:FF Desk Speaker\nnot a device line\n";
        assert_eq!(
            parse_bluetoothctl(linux),
            vec![("aa:bb:cc:dd:ee:ff".to_string(), "Desk Speaker".to_string())]
        );

        let macos = "Bluetooth:\n      Connected:\n        Studio Display:\n          \
                     Address: AA-BB-CC-DD-EE-01\n          Minor Type: Speaker\n";
        assert_eq!(
            parse_system_profiler(macos),
            vec![(
                "aa:bb:cc:dd:ee:01".to_string(),
                "Studio Display".to_string()
            )]
        );
    }

    #[test]
    fn the_machine_running_the_scan_appears_in_it() {
        // The sweep skips this machine's own address, so without being told,
        // a scan lists every device on the network except the one you are on.
        let this = ThisDevice {
            address: "192.168.1.24".into(),
            name: "workshop-nuc".into(),
            sharing_compute: true,
        };
        let options = ScanOptions {
            sweep_subnet: false,
            use_neighbours: false,
            use_mdns: false,
            use_bluetooth: false,
            ..Default::default()
        };

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let report = runtime.block_on(scan(&[], Some(&this), &options));

        let me = report
            .devices
            .iter()
            .find(|d| d.address == "192.168.1.24")
            .expect("this device is in its own scan");
        assert!(me.running_bifrost);
        assert_eq!(me.summary(), "running Bifrost, sharing its hardware");
        assert_eq!(report.running_bifrost(), 1);
        assert_eq!(report.candidates(), 0);
    }

    #[tokio::test]
    async fn a_probe_finds_what_is_listening_and_not_what_is_not() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();

        // 127.0.0.2 is also loopback on every platform Bifrost targets, and
        // nothing is bound there — so it must not answer.
        let answered = probe_many(&[Ipv4Addr::LOCALHOST, Ipv4Addr::new(127, 0, 0, 2)], port).await;

        assert_eq!(answered, vec![Ipv4Addr::LOCALHOST]);
    }

    #[test]
    fn addresses_sort_numerically_and_bluetooth_comes_last() {
        let device = |address: &str| FoundDevice {
            address: address.to_string(),
            name: None,
            hardware: None,
            sources: Vec::new(),
            details: Vec::new(),
            running_bifrost: false,
            sharing_compute: false,
        };

        let mut devices = [
            device("aa:bb:cc:dd:ee:ff"),
            device("192.168.29.119"),
            device("192.168.29.46"),
            device("192.168.29.9"),
        ];
        devices.sort_by_key(sort_key);

        let order: Vec<&str> = devices.iter().map(|d| d.address.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "192.168.29.9",
                "192.168.29.46",
                "192.168.29.119",
                "aa:bb:cc:dd:ee:ff"
            ]
        );
    }

    #[test]
    fn a_scan_with_everything_switched_off_still_reports_known_peers() {
        let options = ScanOptions {
            sweep_subnet: false,
            use_neighbours: false,
            use_mdns: false,
            use_bluetooth: false,
            ..Default::default()
        };

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let report = runtime.block_on(scan(&[], None, &options));

        assert!(report.devices.iter().all(|d| !d.running_bifrost));
        assert_eq!(report.running_bifrost(), 0);
    }
}
