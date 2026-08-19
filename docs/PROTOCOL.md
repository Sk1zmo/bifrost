# Bifrost protocol — version 2

Everything two nodes say to each other. Implemented by `crates/core/src/net/`.

Protocol v2 merges the two legacy schemes: `Bifrost v2` carried chat over libp2p gossipsub, `Bifrost v3` used plain sockets with a partly-different message set. v2 keeps v3's shape — direct sockets, no gossip layer — with v2's efficient binary file streaming, and fixes what both got wrong.

**Compatibility.** A v2 node still parses v1 beacons, so an un-upgraded `Bifrost v3` node remains visible with real telemetry. Every field added since v1 carries `#[serde(default)]`.

---

## 1. Transports

| Port | Proto | Purpose |
|---|---|---|
| 9000 | TCP | Web dashboard and REST API (not part of the peer protocol) |
| 9001 | UDP | Discovery beacons |
| 9002 | TCP | Chat and control |
| 9003 | TCP | File transfer |
| 9004 | TCP | Voice signalling |
| 9005 | UDP | Voice audio |

Every port except discovery may be shifted per node. The discovery port is where nodes agree to meet, and each beacon advertises the sender's full port set — so a node never assumes a peer's ports.

---

## 2. Framing

Every TCP message is:

```
┌────────────────────┬──────────────────────────────┐
│  length: u32 LE    │  JSON payload, `length` bytes │
└────────────────────┴──────────────────────────────┘
```

**All integers everywhere in this protocol are little-endian.** Both legacy trees documented little-endian and then called tokio's `write_u32`/`write_u64`, which are big-endian. Here the code and the specification agree.

A reader must reject a length above `MAX_FRAME_BYTES` (8 MiB) *before* allocating. `Bifrost v2` trusted the length word, which let a hostile or corrupt peer request a multi-gigabyte allocation.

A frame is a complete unit. `Bifrost v3`'s messaging service read into a fixed buffer with a single `read()`, so any message split across TCP segments was silently truncated, and two messages arriving in one segment lost the second.

---

## 3. Discovery — UDP 9001

### The beacon

Sent every **2 seconds** to *all* of:

- `255.255.255.255:9001` — limited broadcast
- each interface's subnet-directed broadcast, e.g. `192.168.1.255:9001`
- `239.255.42.99:9001` — multicast

Sending to all three is the single most important reliability decision in the protocol. Limited broadcast alone — what both legacy trees used — is dropped by many Wi‑Fi drivers and access points. Subnet-directed broadcast survives more of them. Enterprise equipment usually drops both and forwards multicast. Together they cost well under 1 KiB every two seconds.

Beacons carry **TTL 1** and never leave the local segment.

```json
{
  "magic": "BIFROST_DISCOVERY",
  "version": 2,
  "node_id": "550e8400-e29b-41d4-a716-446655440000",
  "username": "workshop-nuc",
  "device_type": "linux-server",
  "os": "Ubuntu 24.04 (x86_64)",
  "app_version": "1.0.0",
  "ip": "192.168.1.24",
  "ports": {
    "web": 9000, "discovery": 9001, "messaging": 9002,
    "files": 9003, "voice_signal": 9004, "voice_audio": 9005
  },
  "telemetry": {
    "cpu_usage": 12.4, "cpu_cores": 8,
    "ram_used_mb": 9216, "ram_total_mb": 16384,
    "disk_used_gb": 380.2, "disk_total_gb": 500.0,
    "net_rx_kbps": 12.1, "net_tx_kbps": 3.4,
    "uptime_secs": 614572,
    "collected_at": "2026-01-15T10:30:00Z"
  },
  "shared": {
    "enabled": true, "cpu_offered_pct": 21.9,
    "ram_offered_mb": 1792, "disk_offered_gb": 29.9
  },
  "timestamp": "2026-01-15T10:30:00Z",

  "cpu_usage": 12.4,
  "ram_available_mb": 7168
}
```

The last two fields are flat duplicates kept for v1 readers; a v2 reader uses `telemetry`.

### Rules

- Ignore any datagram whose `magic` is not `BIFROST_DISCOVERY`, or whose `version` is neither 1 nor 2.
- Ignore a beacon whose `node_id` matches your own — you will receive your own broadcasts.
- **Use the UDP source address, not the `ip` field.** A multi-homed sender routinely advertises the wrong interface; the source address is where the packet demonstrably came from. Both legacy trees trusted the claimed field and so listed peers that could never be connected to.
- When the same peer is seen at several addresses, prefer the one whose matching local subnet has the **longest prefix**. A `/24` Wi‑Fi match beats a `/8` VPN overlay; without this rule, whichever beacon arrived last would win and connections would fail whenever the overlay does not route.
- A peer with no beacon for **12 s** is `offline`. A peer offline for **15 min** is forgotten. The two are separate so a laptop that sleeps keeps the alias its peers gave it.

### Anti-requirements

There is no bootstrap node, no rendezvous server, no DHT, and no NAT traversal. Bifrost is LAN-only by design.

---

## 4. Messaging — TCP 9002

One connection may carry any number of framed messages, in either direction.

```json
{ "type": "<Variant>", "payload": { … } }
```

### Chat

```json
{
  "type": "Chat",
  "payload": {
    "message_id": "d3b0…",
    "sender_id":  "550e8400-…",
    "sender_name": "workshop-nuc",
    "recipient_id": null,
    "content": "build finished",
    "timestamp": "2026-01-15T10:30:00Z"
  }
}
```

`recipient_id` is `null` for a broadcast, or a node id for a private message. A receiver **must** discard a message addressed to a third party rather than displaying it.

`message_id` exists for de-duplication. A broadcast opens one connection per peer, and a retry after a half-open TCP connection can deliver the same message twice; `Bifrost v2` got this for free from gossipsub, and sending directly means doing it explicitly.

### Ping / Pong

```json
{ "type": "Ping" }
{ "type": "Pong" }
```

The round trip is the latency shown per peer.

### Presence

```json
{
  "type": "Presence",
  "payload": {
    "node_id": "550e8400-…",
    "username": "workshop-nuc",
    "device_type": "linux-server",
    "status": "offline",
    "timestamp": "2026-01-15T10:30:00Z"
  }
}
```

`status` is `online`, `away`, `busy` or `offline`. A node sends `offline` to every peer as it shuts down, so it disappears immediately instead of after the 12-second timeout.

### Telemetry, FileOffer

`Telemetry` pushes metrics out of band; `FileOffer` announces an imminent transfer. Both are advisory — beacons already carry telemetry, and the file service works without a prior announcement.

---

## 5. File transfer — TCP 9003

Four steps. Steps 1, 2 and 4 are JSON frames; step 3 is a raw binary stream.

```
sender                                    receiver
   │  ── FileOffer ──────────────────────►  │
   │  ◄──────────────────────── FileAck ──  │
   │  ── chunk stream ──────────────────►   │
   │  ◄────────────────────── FileResult ── │
```

### Step 1 — FileOffer

```json
{
  "transfer_id": "tid-123abc",
  "sender_id": "550e8400-…",
  "sender_name": "workshop-nuc",
  "filename": "report.pdf",
  "file_size": 1048576,
  "total_chunks": 16,
  "chunk_size": 65536,
  "checksum": "ba7816bf8f01cfea…",
  "timestamp": "2026-01-15T10:30:00Z"
}
```

`checksum` is the lowercase hex SHA‑256 of the whole file.

### Step 2 — FileAck

```json
{ "transfer_id": "tid-123abc", "accepted": true, "reason": null }
```

On `accepted: false` the sender must close without sending chunks.

### Step 3 — the chunk stream

Raw binary, repeated until the terminator:

```
┌──────────────┬───────────────┬──────────────────┐
│ index: u64 LE│ length: u32 LE│ length bytes     │
└──────────────┴───────────────┴──────────────────┘
```

terminated by `index = 0xFFFF_FFFF_FFFF_FFFF`, `length = 0`.

Default chunk size 64 KiB; the last chunk may be smaller.

Chunks are raw bytes, not JSON. `Bifrost v3` serialised each chunk as a JSON array of numbers, inflating every byte to roughly four on the wire and forcing the receiver to parse a 64 KiB JSON array per chunk.

A receiver **must** stop if the running total exceeds the declared `file_size`, rather than writing an unbounded amount to disk.

### Step 4 — FileResult

```json
{
  "transfer_id": "tid-123abc",
  "success": true,
  "bytes_received": 1048576,
  "saved_as": "/home/me/Downloads/Bifrost/report.pdf",
  "error": null
}
```

The receiver hashes what it actually received and compares against the offer. A mismatch is a failure and the data is discarded.

### Receiver obligations

- **Sanitise the filename.** The sender chooses it, so it is untrusted input. Reduce it to a single path component; neutralise `..`, path separators, control characters and Windows-reserved names. `Bifrost v2` only prefixed a transfer id, which does not neutralise the `..` that follows it.
- **Never overwrite.** Append ` (1)`, ` (2)`, …
- **Write to a temporary file** and rename only after verification, so an interrupted transfer never leaves a truncated file under the real name.

---

## 6. Voice — TCP 9004 signalling, UDP 9005 audio

### Signalling

```json
{
  "kind": "call_request",
  "session_id": "sess-uuid",
  "caller_id": "node-A",
  "callee_id": "node-B",
  "audio_port": 9005,
  "data": null,
  "timestamp": "2026-01-15T10:30:00Z"
}
```

`kind` is one of `call_request`, `call_accept`, `call_reject`, `call_end`, `sdp_offer`, `sdp_answer`, `ice_candidate`.

```
caller                                     callee
   │  ── call_request ─────────────────────►  │
   │  ◄──────────────────────── call_accept ─ │
   │  ⇄ audio on UDP 9005 ⇄                   │
   │  ── call_end ─────────────────────────►  │
```

The callee sends audio to the **source address of the signalling connection** combined with the `audio_port` the caller advertised — not to any address the caller claims. Same reasoning as discovery.

`data` carries SDP or ICE candidates untouched, so a client running a real WebRTC stack can negotiate through the same exchange.

### Audio

Each UDP datagram is a 16-byte header followed by payload:

```
0     2  3     4     5   7  8            15  16 ...
┌───────┬─────┬───────┬─────┬──────────────┬─────────┐
│ "BFA" │ ver │ codec │ rsv │ sequence u64 │ payload │
│  3 B  │  1  │   1   │  3  │      LE      │         │
└───────┴─────┴───────┴─────┴──────────────┴─────────┘
```

`codec` is `0` for signed 16-bit little-endian PCM, `1` for Opus. Sample rate is fixed at **16 kHz mono**, so it needs no header field — changing it is a version bump. A 20 ms frame is 320 samples, 640 bytes.

A receiver buffers at most 50 frames (one second) per session and drops the **oldest** beyond that. For live audio a late frame is worse than a missing one.

Bifrost carries audio; it does not capture it. Each client feeds frames in from its own microphone — the browser's `getUserMedia`, or the phone's audio session.

---

## 7. Constants

| Name | Value | Meaning |
|---|---|---|
| `DISCOVERY_MAGIC` | `"BIFROST_DISCOVERY"` | Beacon identifier |
| `PROTOCOL_VERSION` | `2` | This document |
| `DISCOVERY_MULTICAST_V4` | `239.255.42.99` | Multicast group |
| `BEACON_INTERVAL_SECS` | `2` | Beacon cadence |
| `PEER_STALE_SECS` | `12` | Silence before offline |
| `PEER_FORGET_SECS` | `900` | Offline before forgotten |
| `CHUNK_SIZE` | `65536` | File chunk, 64 KiB |
| `MAX_FRAME_BYTES` | `8388608` | Largest framed message |
| `AUDIO_SAMPLE_RATE` | `16000` | Mono |
| `AUDIO_FRAME_SAMPLES` | `320` | 20 ms |

---

## 8. Extending it

`Envelope` is an externally-tagged enum, so a new variant is invisible to an older node rather than a parse failure. Phase 2's `ComputeTask` and `ComputeResult` slot in without a version bump; `SharedResources` already advertises what each node will contribute.
