//! `bifrost doctor` — diagnose why peers are not appearing.
//!
//! "Peers not appearing" is the entry that dominated both legacy trees'
//! troubleshooting docs, and every cause is checkable from the machine itself:
//! no usable interface, a port already taken, a firewall, or an AP that drops
//! broadcast. This runs those checks and says which one it is, with the fix.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use bifrost_core::net::iface;
use bifrost_core::net::protocol::{PortSet, DISCOVERY_MULTICAST_V4};
use bifrost_core::Config;

/// A single check's verdict.
enum Verdict {
    Pass(String),
    Warn(String, String),
    Fail(String, String),
}

impl Verdict {
    fn print(&self, name: &str) {
        match self {
            Self::Pass(detail) => println!("  [ ok ]  {name:<26} {detail}"),
            Self::Warn(detail, fix) => {
                println!("  [warn]  {name:<26} {detail}");
                println!("          └─ {fix}");
            }
            Self::Fail(detail, fix) => {
                println!("  [FAIL]  {name:<26} {detail}");
                println!("          └─ {fix}");
            }
        }
    }

    fn is_bad(&self) -> bool {
        matches!(self, Self::Fail(_, _))
    }
}

pub async fn run(config_path: &Path) -> Result<()> {
    println!();
    println!("  Bifrost diagnostics");
    println!("  ───────────────────────────────────────────────────────────");
    println!();

    let config = Config::load_or_create(config_path)?;
    let ports = config.ports;
    let mut verdicts = Vec::new();

    // ── Configuration ──
    println!("  Configuration");
    let identity = Verdict::Pass(format!(
        "{} · {}",
        config.identity.username, config.identity.node_id
    ));
    identity.print("identity");

    let writable = check_writable(&config);
    writable.print("directories");
    verdicts.push(writable);
    println!();

    // ── Network interfaces ──
    println!("  Network");
    let interfaces = iface::usable_interfaces();
    let iface_verdict = if interfaces.is_empty() {
        Verdict::Fail(
            "no usable IPv4 interface".into(),
            "Connect to Wi-Fi or plug in an Ethernet cable. Bifrost is LAN-only \
             and cannot work with no network."
                .into(),
        )
    } else {
        Verdict::Pass(format!("{} interface(s)", interfaces.len()))
    };
    iface_verdict.print("interfaces");
    verdicts.push(iface_verdict);

    for interface in &interfaces {
        println!(
            "          {:<16} {:<15} {} → broadcast {}",
            interface.name,
            interface.ip,
            interface.kind(),
            interface.broadcast
        );
    }

    let chosen = iface::local_ipv4();
    Verdict::Pass(format!("{chosen}")).print("advertised address");

    if chosen.is_loopback() {
        Verdict::Warn(
            "only loopback is available".into(),
            "Peers on other machines will not find this node. Join a network first.".into(),
        )
        .print("reachability");
    }

    // Several interfaces on different subnets is the classic VPN / WSL /
    // Docker case, where peers see a node they cannot connect back to.
    let subnets: std::collections::HashSet<Ipv4Addr> =
        interfaces.iter().map(|i| i.broadcast).collect();
    if subnets.len() > 1 {
        Verdict::Warn(
            format!("{} separate subnets", subnets.len()),
            "A VPN, WSL or Docker adapter can shadow the real LAN. If peers appear \
             but will not connect, disable the extra adapter and retry."
                .into(),
        )
        .print("multiple subnets");
    }
    println!();

    // ── Ports ──
    println!("  Ports");
    let port_checks: [(&str, u16, bool); 6] = [
        ("discovery (UDP)", ports.discovery, false),
        ("chat (TCP)", ports.messaging, true),
        ("files (TCP)", ports.files, true),
        ("voice signal (TCP)", ports.voice_signal, true),
        ("voice audio (UDP)", ports.voice_audio, false),
        ("dashboard (TCP)", ports.web, true),
    ];

    for (name, port, is_tcp) in port_checks {
        let verdict = check_port(port, is_tcp).await;
        verdict.print(name);
        verdicts.push(verdict);
    }
    println!();

    // ── Live discovery probe ──
    println!("  Discovery");
    let probe = probe_broadcast(ports).await;
    probe.print("beacon delivery");
    verdicts.push(probe);
    println!();

    // ── Firewall ──
    println!("  Firewall");
    #[cfg(windows)]
    {
        let verdict = check_windows_firewall().await;
        verdict.print("inbound rules");
        verdicts.push(verdict);
        println!();
    }
    println!("  {}", firewall_hint(ports));
    println!();

    // ── Summary ──
    println!("  ───────────────────────────────────────────────────────────");
    let failures = verdicts.iter().filter(|v| v.is_bad()).count();
    if failures == 0 {
        println!("  Everything checks out. Run `bifrost` here and on another");
        println!("  machine on the same network; they should see each other");
        println!("  within about five seconds.");
    } else {
        println!("  {failures} check(s) failed — fix those first.");
    }
    println!();

    Ok(())
}

fn check_writable(config: &Config) -> Verdict {
    for dir in [&config.download_dir, &config.data_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Verdict::Fail(
                format!("cannot create {}: {e}", dir.display()),
                "Check the permissions on that path, or point download_dir and \
                 data_dir somewhere writable in config.json."
                    .into(),
            );
        }
    }
    Verdict::Pass("downloads and data are writable".into())
}

/// Try to bind a port, to find out whether something already holds it.
async fn check_port(port: u16, is_tcp: bool) -> Verdict {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));

    let result: std::io::Result<()> = if is_tcp {
        tokio::net::TcpListener::bind(addr).await.map(|_| ())
    } else {
        tokio::net::UdpSocket::bind(addr).await.map(|_| ())
    };

    match result {
        Ok(()) => Verdict::Pass(format!("{port} is free")),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // For UDP this is expected and fine: discovery binds with
            // SO_REUSEADDR precisely so several nodes can share the port.
            if is_tcp {
                Verdict::Fail(
                    format!("{port} is already in use"),
                    format!(
                        "Another Bifrost node is probably running. Use it, stop it, \
                         or start this one with `--port-offset 10`. To see what holds \
                         the port: {}",
                        port_owner_hint(port)
                    ),
                )
            } else {
                Verdict::Warn(
                    format!("{port} is in use"),
                    "Harmless if that is another Bifrost node — discovery shares the \
                     port deliberately."
                        .into(),
                )
            }
        }
        Err(e) => Verdict::Fail(
            format!("{port} cannot be bound: {e}"),
            "Check for a firewall or security product blocking the bind.".into(),
        ),
    }
}

fn port_owner_hint(port: u16) -> String {
    if cfg!(windows) {
        format!("netstat -ano | findstr :{port}")
    } else if cfg!(target_os = "macos") {
        format!("lsof -nP -i :{port}")
    } else {
        format!("ss -lptn 'sport = :{port}'")
    }
}

/// Send a beacon-shaped datagram to every target and listen for it coming back.
///
/// This is the check that matters: it proves the machine can actually put a
/// broadcast on the wire and hear it, which is exactly what discovery needs.
async fn probe_broadcast(ports: PortSet) -> Verdict {
    use socket2::{Domain, Protocol, Socket, Type};

    let listener = match (|| -> std::io::Result<std::net::UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_broadcast(true)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, ports.discovery)).into())?;
        socket.set_nonblocking(true)?;
        Ok(socket.into())
    })() {
        Ok(socket) => socket,
        Err(e) => {
            return Verdict::Fail(
                format!("cannot listen on {}: {e}", ports.discovery),
                "Allow UDP on the discovery port through the firewall.".into(),
            )
        }
    };

    let listener = match tokio::net::UdpSocket::from_std(listener) {
        Ok(socket) => socket,
        Err(e) => return Verdict::Fail(format!("listener setup failed: {e}"), String::new()),
    };
    let _ = listener.join_multicast_v4(
        Ipv4Addr::from(DISCOVERY_MULTICAST_V4),
        Ipv4Addr::UNSPECIFIED,
    );

    let sender = match tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(socket) => socket,
        Err(e) => return Verdict::Fail(format!("cannot open a sender: {e}"), String::new()),
    };
    let _ = sender.set_broadcast(true);
    let _ = sender.set_multicast_ttl_v4(1);

    // A nonce, so a real node's beacons on the same LAN are not mistaken for
    // our own probe coming back.
    let nonce = format!("BIFROST_DOCTOR_PROBE_{}", std::process::id());

    let mut targets = iface::beacon_targets();
    targets.push(std::net::IpAddr::V4(Ipv4Addr::from(DISCOVERY_MULTICAST_V4)));

    let mut sent = 0usize;
    for target in &targets {
        if sender
            .send_to(nonce.as_bytes(), SocketAddr::new(*target, ports.discovery))
            .await
            .is_ok()
        {
            sent += 1;
        }
    }

    if sent == 0 {
        return Verdict::Fail(
            "no beacon could be sent at all".into(),
            "The firewall is blocking outbound UDP broadcast. Allow bifrost through it.".into(),
        );
    }

    // Wait briefly for the probe to loop back.
    let mut buf = vec![0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    let mut heard = false;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, listener.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) if buf[..len].starts_with(nonce.as_bytes()) => {
                heard = true;
                break;
            }
            Ok(Ok(_)) => continue, // other traffic, including real beacons
            Ok(Err(_)) | Err(_) => break,
        }
    }

    if heard {
        Verdict::Pass(format!("sent to {sent} target(s) and heard back"))
    } else {
        Verdict::Warn(
            format!("sent to {sent} target(s), heard nothing back"),
            "Not necessarily broken — some systems do not loop broadcast back to \
             the sender. If peers still do not appear, the access point is probably \
             dropping broadcast: try a wired connection or a phone hotspot to confirm."
                .into(),
        )
    }
}

/// Does Windows Firewall actually let other devices reach *this* executable?
///
/// This is the check that catches the most confusing failure Bifrost has. The
/// symptom is a phone whose browser hangs forever on the dashboard address
/// while `curl` from the same machine works perfectly, because loopback never
/// touches the firewall. A dropped inbound SYN produces no response at all,
/// so the browser waits instead of failing.
///
/// Two things have to hold, and both are easy to get wrong:
///
///  * there is an enabled inbound Allow rule for **this exact binary** —
///    Windows Firewall matches on program path, so a rule left over from a
///    different build, or from a copy in another folder, does nothing here
///  * that rule covers the profile the *active network* is using — Windows
///    routinely files a home Wi-Fi under Public, and a Private-only rule then
///    never applies
#[cfg(windows)]
async fn check_windows_firewall() -> Verdict {
    let Ok(exe) = std::env::current_exe() else {
        return Verdict::Warn(
            "cannot determine this executable's path".into(),
            "Skipping the firewall check.".into(),
        );
    };
    let exe = exe.display().to_string();

    // Query by application filter rather than enumerating every rule.
    // `Get-NetFirewallRule | Where-Object { Get-NetFirewallApplicationFilter }`
    // reads plausibly but is quadratic in practice: a typical Windows install
    // has thousands of inbound rules and each filter lookup is its own COM
    // round trip, which takes minutes. Filtering at the source is instant.
    let script = format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
$exe = '{}'
$allowed = @(Get-NetFirewallApplicationFilter -Program $exe |
    Get-NetFirewallRule |
    Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' -and $_.Enabled -eq 'True' }})
$profiles = ($allowed | ForEach-Object {{ $_.Profile }} | Sort-Object -Unique) -join ','
"RULES`t$($allowed.Count)`t$profiles"
foreach ($p in Get-NetConnectionProfile) {{
    $on = (Get-NetFirewallProfile -Name $p.NetworkCategory).Enabled
    "NET`t$($p.InterfaceAlias)`t$($p.NetworkCategory)`t$on"
}}"#,
        exe.replace('\'', "''")
    );

    // A diagnostic that hangs is worse than one that gives up, so the child
    // process is bounded.
    let child = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();

    let output = match tokio::time::timeout(Duration::from_secs(20), child).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            return Verdict::Warn(
                "could not query Windows Firewall".into(),
                "Check the rules by hand with: wf.msc".into(),
            )
        }
        Err(_) => {
            return Verdict::Warn(
                "the firewall query timed out".into(),
                "Check the rules by hand with: wf.msc".into(),
            )
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut rule_count = 0usize;
    let mut rule_profiles = String::new();
    let mut public_networks = Vec::new();

    for line in text.lines() {
        let fields: Vec<&str> = line.trim().split('\t').collect();
        match fields.as_slice() {
            ["RULES", count, profiles] => {
                rule_count = count.parse().unwrap_or(0);
                rule_profiles = profiles.to_string();
            }
            // A firewall profile that is switched off filters nothing, so a
            // network on it is reachable regardless of the rules.
            ["NET", alias, category, enabled] if *category == "Public" && *enabled == "True" => {
                public_networks.push(alias.to_string());
            }
            _ => {}
        }
    }

    if rule_count == 0 {
        return Verdict::Fail(
            "no inbound rule for this executable".into(),
            format!(
                "Other devices cannot reach this node; their connections are dropped \
                 silently, so a phone's browser just hangs. Note that Windows Firewall \
                 matches on program path, so a rule for a copy of bifrost.exe elsewhere \
                 does not apply here.\n          \
                 Fix it by running install.ps1 as Administrator, or:\n          \
                 New-NetFirewallRule -DisplayName 'Bifrost TCP' -Direction Inbound \
                 -Action Allow -Protocol TCP -LocalPort 9000,9002,9003,9004 -Program '{exe}'"
            ),
        );
    }

    // A rule exists, but it may not cover the network actually in use. The
    // Profile property reads "Any", or a comma-separated list.
    let covers_public = rule_profiles.contains("Any") || rule_profiles.contains("Public");
    if !public_networks.is_empty() && !covers_public {
        return Verdict::Fail(
            format!("rules do not cover {}", public_networks.join(", ")),
            format!(
                "Windows has filed that network as Public, and this node's rules only \
                 cover {rule_profiles}. Either mark it Private, which is correct for a \
                 home or office network:\n          \
                 Set-NetConnectionProfile -InterfaceAlias '{}' -NetworkCategory Private\n          \
                 or re-run install.ps1 -AllProfiles as Administrator.",
                public_networks[0]
            ),
        );
    }

    Verdict::Pass(format!(
        "{rule_count} rule(s) for this binary ({rule_profiles})"
    ))
}

fn firewall_hint(ports: PortSet) -> String {
    let PortSet {
        discovery,
        messaging,
        files,
        voice_signal,
        voice_audio,
        web,
    } = ports;

    if cfg!(windows) {
        format!(
            "Windows: allow bifrost.exe on private networks, or run as Administrator:\n    \
             netsh advfirewall firewall add rule name=\"Bifrost\" dir=in action=allow \
             protocol=UDP localport={discovery},{voice_audio}\n    \
             netsh advfirewall firewall add rule name=\"Bifrost TCP\" dir=in action=allow \
             protocol=TCP localport={messaging},{files},{voice_signal},{web}"
        )
    } else if cfg!(target_os = "macos") {
        "macOS: System Settings → Network → Firewall → Options → allow incoming \
         connections for bifrost."
            .to_string()
    } else {
        format!(
            "Linux (ufw):\n    \
             sudo ufw allow {discovery}/udp && sudo ufw allow {voice_audio}/udp\n    \
             sudo ufw allow {messaging}/tcp && sudo ufw allow {files}/tcp \
             && sudo ufw allow {voice_signal}/tcp && sudo ufw allow {web}/tcp\n  \
             Linux (firewalld):\n    \
             sudo firewall-cmd --add-port={discovery}/udp --add-port={voice_audio}/udp \
             --add-port={messaging}/tcp --add-port={files}/tcp \
             --add-port={voice_signal}/tcp --add-port={web}/tcp --permanent \
             && sudo firewall-cmd --reload"
        )
    }
}
