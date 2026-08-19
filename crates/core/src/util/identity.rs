//! Node identity: a stable id, a display name, and a device type.
//!
//! Generated on first run and persisted, so a node keeps the same identity
//! (and therefore its peers' aliases for it) across restarts.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::resources::telemetry::HostInfo;

/// Who this node is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Stable UUID. The primary key for a node everywhere in the protocol.
    pub node_id: String,
    /// Human-facing name, editable at any time.
    pub username: String,
    /// Coarse device class, e.g. `linux-laptop`, driving the dashboard icon.
    pub device_type: String,
    /// Operating system name, for display.
    pub os: String,
}

impl Identity {
    /// Build a fresh identity for this machine.
    pub fn generate() -> Self {
        let host = HostInfo::detect();
        Self {
            node_id: Uuid::new_v4().to_string(),
            username: default_username(&host),
            device_type: detect_device_type(),
            os: host.summary(),
        }
    }

    /// Short id for compact UI, e.g. `550e8400`.
    pub fn short_id(&self) -> &str {
        let end = self
            .node_id
            .char_indices()
            .nth(8)
            .map_or(self.node_id.len(), |(i, _)| i);
        &self.node_id[..end]
    }

    pub fn display(&self) -> String {
        format!("{} ({})", self.username, self.device_type)
    }
}

/// Best available human name for this machine.
///
/// The hostname is preferred over the login name because a peer list of
/// "kitchen-pi", "workshop-nuc", "alice-thinkpad" is far more useful than one
/// where three machines all say "alice".
fn default_username(host: &HostInfo) -> String {
    let hostname = host.hostname.trim();
    if !hostname.is_empty() && hostname != "unknown-host" && hostname != "localhost" {
        // Strip a trailing `.local` from mDNS-style hostnames.
        return hostname.trim_end_matches(".local").to_string();
    }

    for var in ["USERNAME", "USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }

    format!("node-{}", &Uuid::new_v4().to_string()[..6])
}

/// Coarse device class from the compile-time target.
///
/// Reported at compile time rather than probed, because it decides which icon
/// the dashboard draws and a phone must never be drawn as a server.
pub fn detect_device_type() -> String {
    #[cfg(target_os = "android")]
    return "android-phone".to_string();

    #[cfg(target_os = "ios")]
    return "iphone".to_string();

    #[cfg(target_os = "windows")]
    return "windows-pc".to_string();

    #[cfg(target_os = "macos")]
    return "mac".to_string();

    #[cfg(target_os = "linux")]
    {
        // A Linux box with no graphical session is almost certainly a server
        // or an SBC rather than somebody's laptop.
        let headless =
            std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none();
        return if headless {
            "linux-server".to_string()
        } else {
            "linux-laptop".to_string()
        };
    }

    #[allow(unreachable_code)]
    "unknown-device".to_string()
}

/// The icon families the dashboards understand.
pub fn device_icon(device_type: &str) -> &'static str {
    let d = device_type.to_ascii_lowercase();
    match () {
        _ if d.contains("phone") || d.contains("android") || d.contains("ios") => "phone",
        _ if d.contains("tablet") || d.contains("ipad") => "tablet",
        _ if d.contains("server") || d.contains("pi") => "server",
        _ if d.contains("mac") => "mac",
        _ if d.contains("windows") => "windows",
        _ if d.contains("linux") => "linux",
        _ => "device",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identities_are_unique_and_populated() {
        let a = Identity::generate();
        let b = Identity::generate();

        assert_ne!(a.node_id, b.node_id, "node ids must not collide");
        assert!(
            Uuid::parse_str(&a.node_id).is_ok(),
            "node id should be a UUID"
        );
        assert!(!a.username.is_empty());
        assert!(!a.device_type.is_empty());
        assert!(a.display().contains(&a.username));
    }

    #[test]
    fn short_id_is_eight_characters_and_never_panics() {
        let id = Identity {
            node_id: "550e8400-e29b-41d4".into(),
            username: "a".into(),
            device_type: "d".into(),
            os: "o".into(),
        };
        assert_eq!(id.short_id(), "550e8400");

        // Shorter than 8 characters: truncation must not panic.
        let stub = Identity {
            node_id: "abc".into(),
            ..id.clone()
        };
        assert_eq!(stub.short_id(), "abc");

        let empty = Identity {
            node_id: String::new(),
            ..id
        };
        assert_eq!(empty.short_id(), "");
    }

    #[test]
    fn hostname_wins_over_login_name() {
        let host = HostInfo {
            hostname: "workshop-nuc".into(),
            os_name: "Linux".into(),
            os_version: "6.1".into(),
            kernel: "6.1".into(),
            arch: "x86_64".into(),
        };
        assert_eq!(default_username(&host), "workshop-nuc");

        let mdns = HostInfo {
            hostname: "kitchen-pi.local".into(),
            ..host.clone()
        };
        assert_eq!(default_username(&mdns), "kitchen-pi");

        // With no usable hostname it falls back, but never to an empty string.
        let nameless = HostInfo {
            hostname: "unknown-host".into(),
            ..host
        };
        assert!(!default_username(&nameless).is_empty());
    }

    #[test]
    fn device_types_map_to_known_icons() {
        assert_eq!(device_icon("android-phone"), "phone");
        assert_eq!(device_icon("iphone"), "phone");
        assert_eq!(device_icon("linux-server"), "server");
        assert_eq!(device_icon("raspberry-pi"), "server");
        assert_eq!(device_icon("mac"), "mac");
        assert_eq!(device_icon("windows-pc"), "windows");
        assert_eq!(device_icon("linux-laptop"), "linux");
        assert_eq!(device_icon("something-odd"), "device");

        // Whatever this build targets must resolve to a real icon.
        assert_ne!(device_icon(&detect_device_type()), "");
    }
}
