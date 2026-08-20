# Bifrost

**Connect every device on your network — chat, files, live system stats and voice. No setup, no accounts, no internet.**

Run one command on two machines on the same Wi‑Fi and they find each other in about five seconds. Open the printed address on your phone and you have the full interface there too, with nothing to install.

```
  ┌──────────────────────────────────────────────────────┐
  │  ⬡  B I F R O S T                                    │
  └──────────────────────────────────────────────────────┘

  Node      workshop-nuc (linux-server)
  Address   192.168.1.24
  Dashboard http://192.168.1.24:9000

  Scan with a phone camera to open the dashboard:

      █▀▀▀▀▀█ █▀▄ ▄█  ▄ █▀▀▀▀▀█
      █ ███ █ ▄▄▀▄▄█  █ █ ███ █
      █ ▀▀▀ █ ▀█ ▄▀▀▄▀▀ █ ▀▀▀ █
      ▀▀▀▀▀▀▀ ▀▄▀ █ ▀▄▀ ▀▀▀▀▀▀▀
```

---

## What it does

| | |
|---|---|
| **Finds every device** | UDP broadcast **and** subnet-directed broadcast **and** multicast, so discovery works on networks where one alone fails |
| **Chat** | Broadcast or private, with history that survives a restart |
| **File transfer** | Chunked, resumable-friendly, SHA‑256 verified end to end |
| **Live telemetry** | CPU, memory, disk and network for every device, pooled across the network |
| **Voice** | LAN calls — signalling plus a direct UDP audio path |
| **Two dashboards** | A terminal UI, and a web UI any browser on the network can open |
| **Phones with no app** | A phone joins from its browser as its own named device, visible to every machine |
| **Resource sharing** | Each node advertises spare capacity for Phase 2's scheduler |

Everything stays on your LAN. There is no relay server, no cloud account, and no internet dependency of any kind.

---

## Install

### Linux & macOS

```sh
curl -fsSL https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.sh | sh
```

### Windows

```powershell
irm https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.ps1 | iex
```

### From source (any platform)

```sh
git clone https://github.com/Sk1zmo/bifrost
cd bifrost
cargo build --release -p bifrost-cli
./target/release/bifrost
```

Rust 1.75 or newer is the only requirement; the installers fetch it if it is missing.

### Docker

```sh
docker compose up -d
```

`--network host` is required and already set in the compose file — Bifrost discovers peers by broadcasting, and Docker's bridge network has no path to your real LAN.

---

## Use it

```sh
bifrost                    # node + web dashboard + terminal dashboard
bifrost serve              # headless, for a server or a service
bifrost qr                 # print the dashboard address + a QR code for your phone
bifrost doctor             # diagnose why peers are not appearing
bifrost peers              # list devices, from a script
bifrost chat "deploy done" # broadcast, from a script
bifrost send bob report.pdf
bifrost service install    # start automatically at login
```

Inside either dashboard:

| Command | Does |
|---|---|
| *(any text)* | Broadcast to every device |
| `/msg <peer> <text>` | Private message |
| `/peers` | List discovered devices |
| `/sendfile <peer> <path>` | Send a file |
| `/call <peer>` | Start a voice call |
| `/alias <peer> <name>` | Nickname a device |
| `/ping <peer>` | Measure round-trip time |
| `/share on\|off` | Toggle resource sharing |
| `/help` | Everything else |

`<peer>` accepts a node id, an id prefix, a username or an alias — `/sendfile 550e report.pdf` works.

---

## Your phone

**No app needed.** Run `bifrost qr`, point your phone's camera at the code, and the dashboard opens. The first time, it offers to **join the network** — name the device, and the laptop starts a real Bifrost node on its behalf.

That is not a nickname on someone else's session. The phone gets its own node id, its own ports and its own discovery beacons, so every other machine sees it as a separate device:

```
$ bifrost peers
● other-laptop      192.168.1.31    windows-pc   CPU 12.4%   9.1 GB free
● Kailosh iPhone    192.168.1.24    iphone       CPU  0.0%   0.0 GB free
```

Messages from it come *from it*. `/sendfile "Kailosh iPhone" report.pdf` works from any machine, and the file appears under **Received files** on the phone.

Honest limits, since it needs nothing installed:

- its sockets live in the host laptop's process, so it is on the network only while that laptop runs Bifrost
- files sent to it land on the host, and the phone pulls them over HTTP
- it reports no CPU or memory, because a browser cannot see them

**As a fully independent peer.** When those limits matter — the phone discovering on its own, receiving files directly, working with every laptop asleep — build a native client:

```sh
./scripts/build-android.sh                       # then: cd clients/flutter && flutter run
./scripts/build-ios.sh                           # then open clients/ios-swift in Xcode
```

Both link the same Rust core the desktop runs. See [docs/MOBILE.md](docs/MOBILE.md).

---

## How it is built

```
┌──────────────────────────────────────────────────────────────┐
│  Terminal UI  │  Web dashboard  │  Flutter app  │  SwiftUI    │
│   (ratatui)   │   (any browser) │ (Android+iOS) │   (iOS)     │
└───────┬───────┴────────┬────────┴───────┬───────┴──────┬──────┘
        │                │                │              │
        └────────────────┴────────┬───────┴──────────────┘
                                  │
                      ┌───────────▼────────────┐
                      │      bifrost-core      │
                      │   one Rust engine      │
                      └───────────┬────────────┘
     ┌──────────┬──────────┬──────┴─────┬────────────┬──────────┐
     │ Discovery│ Messaging│ File xfer  │   Voice    │ Telemetry│
     │ UDP 9001 │ TCP 9002 │  TCP 9003  │ 9004/9005  │  sysinfo │
     └──────────┴──────────┴────────────┴────────────┴──────────┘
```

One engine, five front ends, one wire protocol. A phone and a server are the same kind of peer.

| Crate | What it is |
|---|---|
| `crates/core` | The engine. Also builds as `cdylib`/`staticlib` for Android and iOS |
| `crates/web` | The web dashboard, compiled into the binary |
| `crates/cli` | The `bifrost` command and the terminal dashboard |

Full detail in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the wire format is specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

---

## Ports

| Port | Proto | Purpose |
|---|---|---|
| 9000 | TCP | Web dashboard and REST API |
| 9001 | UDP | Peer discovery (broadcast + multicast `239.255.42.99`) |
| 9002 | TCP | Chat and control |
| 9003 | TCP | File transfer |
| 9004 | TCP | Voice signalling |
| 9005 | UDP | Voice audio |

The installers open these automatically. `bifrost doctor` prints the exact commands if you need them by hand.

Run several nodes on one machine with `--port-offset 10`; discovery deliberately stays on 9001 so they still see each other. Use `--discovery-port` to create a genuinely separate network.

---

## Documentation

| | |
|---|---|
| [**HOW-TO.md**](docs/HOW-TO.md) | Step-by-step guide, start to finish |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | How it works inside |
| [PROTOCOL.md](docs/PROTOCOL.md) | The wire protocol, v2 |
| [MOBILE.md](docs/MOBILE.md) | Building the Android and iOS clients |
| [TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | When peers do not appear |

---

## Security

Bifrost is a LAN tool for a network you already trust. Anyone who can reach a node can chat with it, send it files and open its dashboard — the same trust model as a shared printer.

What that does **not** mean is that it is careless with the boundaries it does have:

- filenames from a peer are sanitised before they touch the filesystem — `../../.ssh/authorized_keys` becomes `authorized_keys`
- an incoming file never overwrites an existing one, and lands via a temporary file so an interrupted transfer leaves nothing half-written
- every transfer is SHA‑256 verified, and a mismatch is discarded
- the dashboard's download route refuses any path outside the download directory
- framed reads are bounded, so a hostile length word cannot force a large allocation
- beacons carry TTL 1 and never leave the local segment

It has no encryption and no authentication. Do not run it on a network where that matters.

---

## Contributing

```sh
make check     # fmt + clippy + the full test suite, what CI runs
make test
```

113 tests cover the protocol, framing, discovery, transfers, the FFI boundary and two-node end-to-end behaviour over real sockets.

---

## History

This merges three earlier prototypes — `Bifrost`, `Bifrost v2` and `Bifrost v3` — into one working system. v2's real networking and FFI, v3's module structure and documentation, and a great deal of repair: v3 did not compile, both trees mis-framed their TCP reads, both trusted a peer's self-reported address, both sent discovery only to `255.255.255.255`, and neither mobile client was wired to anything.

## License

MIT
