//! Bifrost wire protocol — version 2.
//!
//! Merges the `Bifrost v2` (libp2p-era) and `Bifrost v3` (plain socket)
//! protocols into one JSON-over-UDP/TCP scheme with no libp2p dependency, so
//! the exact same bytes work on Windows, Linux, macOS, Android and iOS.
//!
//! Transports
//! ----------
//! | Port | Proto | Purpose                                        |
//! |------|-------|------------------------------------------------|
//! | 9000 | TCP   | Web dashboard + REST API (see `bifrost-web`)   |
//! | 9001 | UDP   | Discovery: broadcast + multicast beacons        |
//! | 9002 | TCP   | Messaging / control (framed JSON)               |
//! | 9003 | TCP   | File transfer (framed header + binary chunks)   |
//! | 9004 | TCP   | Voice signalling (framed JSON)                  |
//! | 9005 | UDP   | Voice audio frames (binary header + PCM)        |
//!
//! Framing
//! -------
//! Every TCP message is a `u32` little-endian byte length followed by that
//! many bytes of UTF-8 JSON. v3's messaging service used a bare `read()` with
//! no framing, which silently corrupted any message split across TCP segments;
//! framing everywhere fixes that.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Identifies a Bifrost discovery datagram. Kept byte-identical to v2/v3 so a
/// master node still sees beacons from legacy nodes.
pub const DISCOVERY_MAGIC: &str = "BIFROST_DISCOVERY";

/// Wire protocol version. v1 = legacy (`Bifrost v3`), v2 = this document.
pub const PROTOCOL_VERSION: u8 = 2;

/// Legacy protocol version still accepted on the discovery port.
pub const LEGACY_PROTOCOL_VERSION: u8 = 1;

pub const DEFAULT_WEB_PORT: u16 = 9000;
pub const DEFAULT_DISCOVERY_PORT: u16 = 9001;
pub const DEFAULT_MESSAGING_PORT: u16 = 9002;
pub const DEFAULT_FILE_PORT: u16 = 9003;
pub const DEFAULT_VOICE_SIGNAL_PORT: u16 = 9004;
pub const DEFAULT_VOICE_AUDIO_PORT: u16 = 9005;
pub const DEFAULT_COMPUTE_PORT: u16 = 9006;

/// Administratively-scoped IPv4 multicast group (RFC 2365) used alongside
/// plain broadcast. Many consumer APs and virtually every enterprise AP drop
/// `255.255.255.255` but forward multicast — sending to both is why discovery
/// works "plug and play" on networks where v2/v3 found nothing.
pub const DISCOVERY_MULTICAST_V4: [u8; 4] = [239, 255, 42, 99];

/// Beacon interval. 2s gives a peer list that feels live without meaningful
/// traffic (a beacon is well under 1 KiB).
pub const BEACON_INTERVAL_SECS: u64 = 2;

/// A peer with no beacon for this long is marked offline (not forgotten).
pub const PEER_STALE_SECS: i64 = 12;

/// A peer offline this long is dropped from the registry entirely.
pub const PEER_FORGET_SECS: i64 = 900;

/// File-transfer chunk size (64 KiB), matching the documented v1 spec.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Hard ceiling on a single framed message, to bound attacker-controlled
/// allocation on the read path.
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// Largest UDP datagram accepted on the discovery port.
pub const MAX_DATAGRAM: usize = 8 * 1024;

// ─── Ports advertised by a node ──────────────────────────────────────────────

/// The port set a node listens on, carried in every beacon so peers never have
/// to assume defaults. This is what lets several nodes share one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSet {
    pub web: u16,
    pub discovery: u16,
    pub messaging: u16,
    pub files: u16,
    pub voice_signal: u16,
    pub voice_audio: u16,
    /// Shared compute. Defaulted so a beacon from an older node still parses.
    #[serde(default = "default_compute_port")]
    pub compute: u16,
}

fn default_compute_port() -> u16 {
    DEFAULT_COMPUTE_PORT
}

impl Default for PortSet {
    fn default() -> Self {
        Self {
            web: DEFAULT_WEB_PORT,
            discovery: DEFAULT_DISCOVERY_PORT,
            messaging: DEFAULT_MESSAGING_PORT,
            files: DEFAULT_FILE_PORT,
            voice_signal: DEFAULT_VOICE_SIGNAL_PORT,
            voice_audio: DEFAULT_VOICE_AUDIO_PORT,
            compute: DEFAULT_COMPUTE_PORT,
        }
    }
}

impl PortSet {
    /// Shift every port except discovery, so a second node on the same host
    /// can run with `--port-offset 10` and not collide.
    ///
    /// Discovery deliberately stays put. Its socket binds with `SO_REUSEADDR`
    /// precisely so several nodes can share it, and every beacon carries the
    /// sender's whole [`PortSet`] — so two shifted nodes on one machine still
    /// find each other and still connect to the right ports. Moving discovery
    /// too would put them on separate networks that never meet.
    pub fn offset_by(self, offset: u16) -> Self {
        Self {
            web: self.web + offset,
            discovery: self.discovery,
            messaging: self.messaging + offset,
            files: self.files + offset,
            voice_signal: self.voice_signal + offset,
            voice_audio: self.voice_audio + offset,
            compute: self.compute + offset,
        }
    }
}

// ─── Telemetry ───────────────────────────────────────────────────────────────

/// A point-in-time system metrics sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Telemetry {
    pub cpu_usage: f32,
    pub cpu_cores: usize,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub disk_used_gb: f64,
    pub disk_total_gb: f64,
    pub net_rx_kbps: f64,
    pub net_tx_kbps: f64,
    /// Seconds since boot, or 0 where the platform does not report it.
    pub uptime_secs: u64,
    pub collected_at: DateTime<Utc>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            cpu_usage: 0.0,
            cpu_cores: 0,
            ram_used_mb: 0,
            ram_total_mb: 0,
            disk_used_gb: 0.0,
            disk_total_gb: 0.0,
            net_rx_kbps: 0.0,
            net_tx_kbps: 0.0,
            uptime_secs: 0,
            collected_at: DateTime::<Utc>::MIN_UTC,
        }
    }
}

impl Telemetry {
    pub fn ram_available_mb(&self) -> u64 {
        self.ram_total_mb.saturating_sub(self.ram_used_mb)
    }

    pub fn disk_available_gb(&self) -> f64 {
        (self.disk_total_gb - self.disk_used_gb).max(0.0)
    }
}

/// What a node offers to the pool. Phase 2's scheduler reads this; Phase 1
/// only advertises it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedResources {
    pub enabled: bool,
    pub cpu_offered_pct: f32,
    pub ram_offered_mb: u64,
    pub disk_offered_gb: f64,
}

impl Default for SharedResources {
    fn default() -> Self {
        Self {
            enabled: true,
            cpu_offered_pct: 0.0,
            ram_offered_mb: 0,
            disk_offered_gb: 0.0,
        }
    }
}

// ─── Discovery ───────────────────────────────────────────────────────────────

/// The UDP beacon broadcast every [`BEACON_INTERVAL_SECS`].
///
/// Every field added since v1 is `#[serde(default)]` so a v2 node still parses
/// a v1 beacon from an un-upgraded machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beacon {
    pub magic: String,
    pub version: u8,
    pub node_id: String,
    pub username: String,
    pub device_type: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub app_version: String,
    pub ip: String,
    #[serde(default)]
    pub ports: PortSet,
    #[serde(default)]
    pub telemetry: Telemetry,
    #[serde(default)]
    pub shared: SharedResources,
    /// What this node will run for others. Defaulted, so a node that predates
    /// shared compute still parses as one that offers none.
    #[serde(default)]
    pub compute: crate::compute::protocol::Capabilities,
    pub timestamp: DateTime<Utc>,

    // ── v1 compatibility ──
    // `Bifrost v3` beacons carried these two flat fields and no `telemetry`
    // object. Retained so a legacy node still shows real numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usage: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_available_mb: Option<u64>,
}

impl Beacon {
    /// True if this datagram is a Bifrost beacon we understand.
    pub fn is_valid(&self) -> bool {
        self.magic == DISCOVERY_MAGIC
            && (self.version == PROTOCOL_VERSION || self.version == LEGACY_PROTOCOL_VERSION)
    }

    /// Telemetry from the beacon, reconstructed from the flat v1 fields when
    /// the sender is a legacy node.
    pub fn effective_telemetry(&self) -> Telemetry {
        let mut t = self.telemetry.clone();
        if self.version == LEGACY_PROTOCOL_VERSION {
            if let Some(cpu) = self.cpu_usage {
                t.cpu_usage = cpu;
            }
            // v1 sent *available* RAM with no total; model it as a node whose
            // total is at least what it says is free, so the UI stays sane.
            if let Some(avail) = self.ram_available_mb {
                if t.ram_total_mb == 0 {
                    t.ram_total_mb = avail;
                    t.ram_used_mb = 0;
                }
            }
        }
        t
    }
}

// ─── Messaging (TCP 9002) ────────────────────────────────────────────────────

/// Top-level envelope for everything on the messaging port.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Envelope {
    Chat(ChatPayload),
    Presence(PresencePayload),
    Telemetry(TelemetryPayload),
    /// Sender announces it is about to open a file-transfer connection.
    FileOffer(FileOffer),
    Ping,
    Pong,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatPayload {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    /// `None` = broadcast to everyone; `Some(node_id)` = direct message.
    #[serde(default)]
    pub recipient_id: Option<String>,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    Online,
    Away,
    Busy,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresencePayload {
    pub node_id: String,
    pub username: String,
    pub device_type: String,
    pub status: PresenceStatus,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryPayload {
    pub node_id: String,
    #[serde(flatten)]
    pub telemetry: Telemetry,
    pub shared: SharedResources,
}

// ─── File transfer (TCP 9003) ────────────────────────────────────────────────

/// Frame 1 of a transfer, sender → receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOffer {
    pub transfer_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub filename: String,
    pub file_size: u64,
    pub total_chunks: u64,
    pub chunk_size: u32,
    /// Lowercase hex SHA-256 of the whole file.
    pub checksum: String,
    pub timestamp: DateTime<Utc>,
}

/// Frame 2, receiver → sender.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAck {
    pub transfer_id: String,
    pub accepted: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Final frame, receiver → sender, after checksum validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResult {
    pub transfer_id: String,
    pub success: bool,
    pub bytes_received: u64,
    #[serde(default)]
    pub saved_as: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Terminator written in the `index` slot of the binary chunk stream.
pub const CHUNK_STREAM_END: u64 = u64::MAX;

// ─── Voice (TCP 9004 signalling, UDP 9005 audio) ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSignalKind {
    CallRequest,
    CallAccept,
    CallReject,
    CallEnd,
    SdpOffer,
    SdpAnswer,
    IceCandidate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceSignal {
    pub kind: VoiceSignalKind,
    pub session_id: String,
    pub caller_id: String,
    pub callee_id: String,
    /// UDP port the sender wants to receive audio on.
    pub audio_port: u16,
    /// SDP blob or ICE candidate JSON, for clients running a real WebRTC stack.
    #[serde(default)]
    pub data: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Audio frame format carried in the UDP payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    /// Signed 16-bit little-endian PCM.
    Pcm16Le = 0,
    /// Opus packets, for clients that have an encoder.
    Opus = 1,
}

impl AudioCodec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Pcm16Le),
            1 => Some(Self::Opus),
            _ => None,
        }
    }
}

/// Fixed 16-byte header prefixing every UDP audio datagram.
///
/// ```text
/// 0     2  3     4     5   7  8            15  16 ...
/// ┌───────┬─────┬───────┬─────┬──────────────┬─────────┐
/// │ "BFA" │ ver │ codec │ rsv │ sequence u64 │ payload │
/// │  3 B  │  1  │   1   │  3  │      LE      │         │
/// └───────┴─────┴───────┴─────┴──────────────┴─────────┘
/// ```
/// Sample rate is fixed at [`AUDIO_SAMPLE_RATE`] and mono, so it needs no
/// header field; changing either is a protocol version bump.
pub const AUDIO_MAGIC: [u8; 3] = *b"BFA";
pub const AUDIO_HEADER_LEN: usize = 16;
pub const AUDIO_SAMPLE_RATE: u32 = 16_000;
/// 20 ms of mono 16 kHz audio = 320 samples = 640 bytes.
pub const AUDIO_FRAME_SAMPLES: usize = 320;

/// Serialise an audio header into a 16-byte array.
pub fn audio_header(codec: AudioCodec, sequence: u64) -> [u8; AUDIO_HEADER_LEN] {
    let mut out = [0u8; AUDIO_HEADER_LEN];
    out[0..3].copy_from_slice(&AUDIO_MAGIC);
    out[3] = PROTOCOL_VERSION;
    out[4] = codec as u8;
    // out[5..8] reserved, kept zero.
    out[8..16].copy_from_slice(&sequence.to_le_bytes());
    out
}

/// Parse an audio datagram into `(codec, sequence, payload)`.
pub fn parse_audio_datagram(buf: &[u8]) -> Option<(AudioCodec, u64, &[u8])> {
    if buf.len() < AUDIO_HEADER_LEN || buf[0..3] != AUDIO_MAGIC {
        return None;
    }
    let codec = AudioCodec::from_u8(buf[4])?;
    let seq = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    Some((codec, seq, &buf[AUDIO_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_header_roundtrips() {
        let mut dgram = audio_header(AudioCodec::Pcm16Le, 42).to_vec();
        dgram.extend_from_slice(&[1, 2, 3, 4]);

        let (codec, seq, payload) = parse_audio_datagram(&dgram).expect("valid datagram");
        assert_eq!(codec, AudioCodec::Pcm16Le);
        assert_eq!(seq, 42);
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn audio_parser_rejects_foreign_traffic() {
        assert!(parse_audio_datagram(b"").is_none());
        assert!(parse_audio_datagram(b"not-bifrost-audio-data").is_none());
        // Right magic, unknown codec id.
        let mut bad = audio_header(AudioCodec::Pcm16Le, 1);
        bad[4] = 99;
        assert!(parse_audio_datagram(&bad).is_none());
    }

    #[test]
    fn port_offset_moves_everything_but_discovery() {
        let p = PortSet::default().offset_by(10);

        assert_eq!(p.web, 9010);
        assert_eq!(p.messaging, 9012);
        assert_eq!(p.compute, 9016);
        assert_eq!(p.files, 9013);
        assert_eq!(p.voice_signal, 9014);
        assert_eq!(p.voice_audio, 9015);

        // Two offset nodes on one machine must still meet on the same
        // discovery port, or they would never see each other.
        assert_eq!(p.discovery, DEFAULT_DISCOVERY_PORT);
        assert_eq!(PortSet::default().offset_by(200).discovery, p.discovery);
    }

    #[test]
    fn legacy_v1_beacon_parses_and_keeps_its_metrics() {
        // Exactly the shape `Bifrost v3` put on the wire.
        let raw = r#"{
            "magic": "BIFROST_DISCOVERY",
            "version": 1,
            "node_id": "n1",
            "username": "alice",
            "device_type": "linux-laptop",
            "ip": "192.168.1.10",
            "cpu_usage": 23.5,
            "ram_available_mb": 6144,
            "timestamp": "2025-01-15T10:30:00Z"
        }"#;

        let beacon: Beacon = serde_json::from_str(raw).expect("v1 beacon must parse");
        assert!(beacon.is_valid());
        assert_eq!(beacon.ports, PortSet::default());

        let telemetry = beacon.effective_telemetry();
        assert_eq!(telemetry.cpu_usage, 23.5);
        assert_eq!(telemetry.ram_available_mb(), 6144);
    }

    #[test]
    fn foreign_magic_is_rejected() {
        let raw = r#"{
            "magic": "SOMETHING_ELSE", "version": 2, "node_id": "n1",
            "username": "a", "device_type": "d", "ip": "1.2.3.4",
            "timestamp": "2025-01-15T10:30:00Z"
        }"#;
        let beacon: Beacon = serde_json::from_str(raw).unwrap();
        assert!(!beacon.is_valid());
    }
}
