# Architecture

## The shape of it

One Rust engine, five front ends, one wire protocol.

```
┌──────────────────────────────────────────────────────────────┐
│  Terminal UI  │  Web dashboard  │  Flutter app  │  SwiftUI    │
│   (ratatui)   │   (any browser) │ (Android+iOS) │   (iOS)     │
└───────┬───────┴────────┬────────┴───────┬───────┴──────┬──────┘
        │                │                │              │
   direct calls      HTTP + SSE       dart:ffi      C interop
        │                │                │              │
        └────────────────┴────────┬───────┴──────────────┘
                                  │
                      ┌───────────▼────────────┐
                      │        Node            │
                      │  owns every service,   │
                      │  one execute() entry   │
                      └───────────┬────────────┘
     ┌──────────┬──────────┬──────┴─────┬────────────┬──────────┐
     │ Discovery│ Messaging│ File xfer  │   Voice    │ Telemetry│
     │ UDP 9001 │ TCP 9002 │  TCP 9003  │ 9004/9005  │  sysinfo │
     └──────────┴──────────┴────────────┴────────────┴──────────┘
```

The single most consequential decision is that every front end drives the same
`Node::execute`. `/sendfile bob report.pdf` behaves identically typed into a
terminal, tapped on a phone, or POSTed to the REST API — because it is the same
code path, not four parallel implementations that drift.

---

## Crates

| Crate | Contains | Also builds as |
|---|---|---|
| `bifrost-core` | The engine: protocol, discovery, messaging, transfers, voice, telemetry, storage, FFI | `cdylib` for Android, `staticlib` for iOS |
| `bifrost-web` | The web dashboard and REST API | — |
| `bifrost-cli` | The `bifrost` command, the terminal dashboard, `doctor`, QR, service install | binary |

`bifrost-core` has no dependency on either UI crate, which is what lets a phone
link it without dragging in a terminal library or an HTTP server.

---

## Module map

```
crates/core/src/
├── node.rs           Node — owns everything, one execute() entry point
├── events.rs         EventBus — one broadcast channel every UI subscribes to
├── ffi.rs            C ABI for Android and iOS
├── net/
│   ├── protocol.rs   Wire types, ports, constants
│   ├── frame.rs      Length-prefixed JSON framing
│   ├── iface.rs      Interface enumeration, broadcast targets, reachability
│   ├── discovery.rs  UDP beacons: broadcast + multicast
│   ├── messaging.rs  TCP chat and control
│   └── peers.rs      PeerRegistry
├── chat/
│   ├── history.rs    ChatStore with de-duplication
│   ├── commands.rs   Slash-command parser
│   ├── presence.rs   Online / away / busy
│   └── voice.rs      Signalling + UDP audio transport
├── files/transfer.rs Chunked, checksummed transfer
├── resources/
│   ├── telemetry.rs  sysinfo sampling
│   └── sharing.rs    Resource advertisement
├── store/data_store.rs  Chat log + key-value state
└── util/             Config, identity, logging
```

---

## Concurrency

Everything is Tokio tasks over shared state. `Node` is `Clone` and cheap to
clone — every field is an `Arc` or a channel — so handing it to an axum handler
or a UI loop costs nothing.

```
Node::start
  ├── spawn  TelemetryCollector::run        every 2 s
  ├── spawn  SharingService::run            on telemetry or policy change
  ├── spawn  discovery::run                 beacon out, listen in, prune
  ├── spawn  messaging::run_server          accept loop
  │            └── spawn per connection
  ├── spawn  transfer::run_server           accept loop
  │            └── spawn per transfer
  ├── spawn  voice::run_signal_server       accept loop
  ├── spawn  voice::run_audio_listener      UDP recv loop
  ├── spawn  chat persistence               drains the event bus to disk
  └── spawn  housekeeping                   presence, transfer pruning, log rotation
```

Shared state uses three patterns, each chosen for a reason:

| Pattern | Used for | Why |
|---|---|---|
| `Arc<RwLock<T>>` | Peer registry, chat, transfers, calls | Read-heavy; many readers, rare writers |
| `watch::channel` | Telemetry, shared resources, identity | Latest-value-wins; a slow reader must never block the sampler |
| `broadcast::channel` | The event bus | Fan-out to every UI; a slow subscriber lags rather than blocking the node |

One consequence worth knowing: `watch::Ref` wraps a std `RwLockReadGuard` and
is therefore `!Send`. Holding one across an `.await` makes the enclosing future
`!Send`, which axum rejects at compile time. `Node::snapshot` reads every watch
channel into a local before it awaits anything.

---

## The snapshot

Rendering reads `NodeSnapshot` — identity, telemetry, peers, chat, transfers,
calls and pool totals captured together. One struct rather than a dozen
accessors, because a UI that samples fields at different moments draws a frame
that never actually existed: a peer count from before a disconnection beside
telemetry from after it.

It serialises straight to JSON, so the same struct feeds `/api/snapshot`,
`bifrost_snapshot_json()` for mobile, and the terminal dashboard.

---

## The event bus

One `broadcast` channel carries chat, transfers, peer arrivals and departures,
telemetry ticks and log notices. Every UI subscribes:

- the terminal dashboard drains it each frame
- the web dashboard forwards it as Server-Sent Events
- mobile buffers it for `bifrost_poll_events()`

A subscriber that falls behind receives `Lagged` and re-syncs from a snapshot.
Losing log lines is acceptable; blocking the node is not.

---

## Discovery, and why it works

The reliability of the whole system rests here. Both legacy trees broadcast
only to `255.255.255.255`, which is why their documentation had a
"peers not appearing" section. Three changes fixed it:

1. **Send everywhere.** Limited broadcast, each interface's subnet-directed
   broadcast, *and* multicast `239.255.42.99`. Different networks drop
   different ones; almost none drop all three.

2. **Trust the source address.** A multi-homed sender routinely advertises the
   wrong interface. The UDP source address is where the packet demonstrably
   came from.

3. **Rank addresses by subnet specificity.** With a VPN present the same peer
   is seen at several addresses. Preferring the longest matching local prefix
   picks the `/24` Wi-Fi over the `/8` overlay deterministically, instead of
   letting whichever beacon arrived last win.

The listener binds with `SO_REUSEADDR` (plus `SO_REUSEPORT` on Unix) so several
nodes share a machine — which is why `--port-offset` moves every port *except*
discovery.

---

## Trust boundaries

Bifrost has no authentication by design. It does have boundaries where
untrusted data enters, and each is handled at the point of entry:

| Input | Handling |
|---|---|
| Peer-supplied filename | Reduced to one path component; `..`, separators, control characters and Windows-reserved names neutralised |
| Framed message length | Rejected above 8 MiB before allocating |
| File transfer size | Aborted the moment the stream exceeds the declared size |
| Dashboard download path | Structurally refused unless it is a single normal component |
| Beacon contents | Parsed leniently, but the source address wins over any claim |
| FFI arguments | Null-checked, UTF-8 validated, panics caught |

---

## Mobile

```
Flutter / SwiftUI
      │  dart:ffi  /  C interop
      ▼
libbifrost_core   (.so on Android, .a on iOS)
      │
      ▼
the same tokio runtime, the same sockets
```

The FFI surface is deliberately narrow: strings in, JSON out. Every extra type
crossing the boundary is a struct layout to mirror in two more languages and
keep in step forever, so instead there is one `bifrost_snapshot_json()` and one
`bifrost_execute()`.

Three rules hold throughout:

- every returned string is owned by the caller and freed with
  `bifrost_string_free`
- no panic crosses the boundary — unwinding through FFI is undefined behaviour,
  so every entry point catches and reports
- events are polled, not pushed — a callback from a Rust thread would need
  isolate ports or a run loop, and polling from a timer the UI already has
  cannot deadlock

---

## Phase 2

Phase 1 is connectivity. Phase 2 adds distributed compute, and the hooks exist:

- `SharedResources` already advertises what each node will contribute
- `Envelope` is externally tagged, so `ComputeTask` and `ComputeResult` are
  invisible to an older node rather than a parse failure
- `DataStore` persists arbitrary keyed state, ready for a task queue
- `PoolTotals` already aggregates the capacity a scheduler would place work on
