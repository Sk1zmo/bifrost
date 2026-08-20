# How to use Bifrost

A guide from nothing installed to every device on your network connected.

**Contents**

1. [Install it](#1-install-it)
2. [Start your first node](#2-start-your-first-node)
3. [Connect a second machine](#3-connect-a-second-machine)
4. [Use your phone](#4-use-your-phone)
5. [Chat](#5-chat)
6. [Send files](#6-send-files)
7. [Voice calls](#7-voice-calls)
8. [Watch the whole network](#8-watch-the-whole-network)
9. [Keep it running](#9-keep-it-running)
10. [Put a phone on the network as a real peer](#10-put-a-phone-on-the-network-as-a-real-peer)
11. [Scripting and automation](#11-scripting-and-automation)
12. [Settings](#12-settings)
13. [When something is wrong](#13-when-something-is-wrong)

---

## 1. Install it

Pick your platform. Each takes a few minutes the first time, mostly compiling.

### Linux (any distribution) and macOS

```sh
curl -fsSL https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.sh | sh
```

The script detects your distribution, installs a compiler and Rust if they are missing, builds Bifrost, puts it on your `PATH` and opens the firewall. It is plain POSIX `sh`, so it works on Alpine and Raspberry Pi OS as well as Ubuntu and Fedora.

### Windows

Open PowerShell and run:

```powershell
irm https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.ps1 | iex
```

Run PowerShell **as Administrator** if you want the firewall rules added automatically. Without that, everything else still installs and you can add the rules later — `bifrost doctor` prints them.

### From source, anywhere

```sh
git clone https://github.com/Sk1zmo/bifrost
cd bifrost
cargo build --release -p bifrost-cli
```

The binary lands at `target/release/bifrost`. Copy it wherever you like — it has no runtime dependencies and no data files.

> **Windows note.** If you use the GNU Rust toolchain and your checkout path contains a space, the build fails inside MinGW's `dlltool`. Either use the MSVC toolchain, or build into a space-free directory:
> ```powershell
> $env:CARGO_TARGET_DIR = "C:\bifrost-build"
> cargo build --release -p bifrost-cli
> ```

### Docker, for a Linux server

```sh
git clone https://github.com/Sk1zmo/bifrost
cd bifrost
docker compose up -d
```

`network_mode: host` is already set and is **not optional** — Bifrost finds peers by broadcasting, and Docker's default bridge network cannot reach your real LAN. Host networking is limited on Docker Desktop for macOS and Windows; run Bifrost natively there.

---

## 2. Start your first node

```sh
bifrost
```

You get the terminal dashboard, and the web dashboard starts alongside it.

```
┌─ ⬡ Bifrost ───────────────────────────────────────────────────────────┐
│  workshop-nuc · linux-server · 192.168.1.24    dashboard http://…:9000 │
├─────────────────────────┬─────────────────────────────────────────────┤
│ This device             │ Messages                                    │
│  CPU  ███░░░░░░░  12.4% │  10:23:01  Bifrost node started             │
│  RAM  ██████░░░░  9/16G │                                             │
│  Disk ████████░░ 380/500│                                             │
│  net ↓12 ↑3 KB/s        │                                             │
├─────────────────────────┤                                             │
│ Devices (0)             │                                             │
│  searching…             │                                             │
├─────────────────────────┤                                             │
│ Transfers               │                                             │
│  none                   │                                             │
├─────────────────────────┴─────────────────────────────────────────────┤
│ > _                                                                   │
└───────────────────────────────────────────────────────────────────────┘
```

On first run Bifrost generates an identity and writes it to `~/.bifrost/config.json`. That identity is stable, which is why the name a friend gives your machine survives your rebooting it.

**Keys**

| Key | Does |
|---|---|
| Type + Enter | Send a message, or run a `/command` |
| ↑ / ↓ | Previous commands |
| Ctrl‑↑ / Ctrl‑↓ | Scroll the message pane |
| Ctrl‑L | Clear the pane |
| Esc or Ctrl‑C | Quit |

Prefer no terminal UI? `bifrost serve` runs headless and prints the dashboard address and a QR code.

---

## 3. Connect a second machine

Install Bifrost on another computer on the **same Wi‑Fi or switch**, and run `bifrost` there too.

Within about five seconds each appears in the other's Devices panel:

```
│ Devices (1)             │
│  ● alice-laptop   14%  192.168.1.31 │
```

That is the entire setup. There is no pairing step, no code to type and no account.

Confirm from either side:

```
/peers
```

Give the other machine a friendlier name — it is local to you and it sticks:

```
/alias alice-laptop Alice's ThinkPad
```

> **Nothing appeared?** Run `bifrost doctor`. It checks your interfaces, ports and firewall, sends a real test beacon, and tells you exactly what is wrong. See [§13](#13-when-something-is-wrong).

### Two nodes on one machine

Useful for trying it out before you involve a second computer:

```sh
bifrost --port-offset 10 --name test-node
```

Everything shifts by 10 except discovery, which deliberately stays on 9001 so the two nodes still find each other.

---

## 4. Use your phone

**This is the part with no installation.**

Start a node if one is not already running, then in a second terminal:

```sh
bifrost qr
```

```
  Dashboard  http://192.168.1.24:9000

      █▀▀▀▀▀█ █▀▄ ▄█  ▄ █▀▀▀▀▀█
      █ ███ █ ▄▄▀▄▄█  █ █ ███ █
      █ ▀▀▀ █ ▀█ ▄▀▀▄▀▀ █ ▀▀▀ █
      ▀▀▀▀▀▀▀ ▀▄▀ █ ▀▄▀ ▀▀▀▀▀▀▀

  Scan with your phone's camera, on the same Wi-Fi.
```

Open your phone's camera app, point it at the code, and tap the notification that appears. No QR app is needed — iOS and Android both scan from the camera.

`bifrost serve` prints the same thing at startup. `bifrost` on its own does not, because the terminal dashboard takes over the screen immediately — which is exactly what `bifrost qr` is for. You can run it at any time, in any terminal, whether or not a node is running; it tells you if nothing is answering yet.

If you would rather type it, the address is also in the terminal dashboard's header bar, and under **Settings → Connection** in the web dashboard.

Either way, you get the whole interface:

- every device with live CPU and memory
- chat, broadcast or private
- upload a file to any device, download anything received
- network throughput over the last two minutes
- push-to-talk voice
- settings

It works on iPhone, Android, a tablet, another laptop, or a Linux box with no desktop environment. **Add to Home Screen** makes it open full-screen like an app.

The dashboard drives *that node*, so anything you do from the phone happens as if you were sitting at that machine.

---

## 5. Chat

Type and press Enter to reach every device:

```
> anyone got the build passing?
delivered to 3 peers
```

Privately:

```
> /msg alice can you look at the deploy?
Sent to Alice's ThinkPad
```

In the web dashboard use the **Send to** dropdown, then type normally.

History lives in `~/.bifrost/data/chat.jsonl` and is restored when you restart, so a reboot does not lose the conversation.

---

## 6. Send files

**From the terminal:**

```
> /sendfile alice ~/Downloads/report.pdf
Sending "report.pdf" to Alice's ThinkPad
```

**From the web dashboard:** Files tab → choose a device → drag a file onto the drop zone, or tap it to pick one.

**From a script:**

```sh
bifrost send alice ./build/release.tar.gz
```

Files arrive in `~/Downloads/Bifrost` (configurable). What happens on the way:

1. the sender hashes the file and offers it
2. the receiver accepts and streams it in 64 KiB chunks
3. the receiver hashes what it got and compares

A mismatch is discarded rather than saved. A transfer is written to a temporary file and renamed only once verified, so an interrupted transfer never leaves a truncated file wearing the real name. An existing file is never overwritten — you get `report (1).pdf`.

Received files can be pulled down from any phone through the dashboard's **Received files** list.

---

## 7. Voice calls

```
> /call alice
In a call with Alice's ThinkPad (session b6a617ed)
```

Then, in the web dashboard's Calls tab, hold **Talk** to transmit. Audio from the other side plays automatically.

To end it:

```
> /hangup
```

Calls are direct, device to device, over your LAN. Nothing is routed through the internet.

> Voice capture comes from whichever client you are using — the browser's microphone, or the phone app's. The terminal dashboard can start and end calls but has no microphone of its own; use the web dashboard or a phone to actually talk.

---

## 8. Watch the whole network

The web dashboard's **Overview** leads with the number that matters — how many devices are on your network — then pools their resources:

- **CPU cores** across every device
- **Memory free** and **Disk free**, summed
- per-device CPU, memory and disk meters
- **Network throughput** over the last two minutes, received and sent, with a hover readout and a table view

The terminal dashboard shows the same information compactly, refreshed twice a second.

---

## 9. Keep it running

To start Bifrost automatically at login:

```sh
bifrost service install
```

This installs a **per-user** service — a systemd user unit on Linux, a launchd agent on macOS, a Startup entry on Windows. None of them needs administrator rights.

On Linux, enable it:

```sh
systemctl --user daemon-reload
systemctl --user enable --now bifrost

# to keep it running when you are not logged in:
sudo loginctl enable-linger $USER
```

Check or remove it:

```sh
bifrost service status
bifrost service uninstall
```

---

## 10. Put a phone on the network as a real peer

The web dashboard makes your phone a *client* of a node. To make the phone a *node* — beaconing, discovering and transferring on its own — build one of the native apps. Both use the same Rust core.

### Android

```sh
export ANDROID_NDK_HOME=~/Android/Sdk/ndk/26.1.10909125
./scripts/build-android.sh

cd clients/flutter
flutter pub get
flutter run          # or: flutter build apk --release
```

### iOS

Requires macOS with Xcode.

```sh
./scripts/build-ios.sh          # produces BifrostCore.xcframework
```

Either open `clients/ios-swift` in Xcode for the native SwiftUI app, or run the same Flutter project:

```sh
cd clients/flutter && flutter run -d ios
```

> **iOS needs one permission or discovery silently fails.** `NSLocalNetworkUsageDescription` must be in `Info.plist`, and the user must allow the local-network prompt. Without it iOS drops every broadcast packet without an error. It is already set in the included `Info.plist`.

Full detail in [MOBILE.md](MOBILE.md).

---

## 11. Scripting and automation

The one-shot subcommands talk to the node already running on your machine over its local API, so they are fast and do not start a second node.

```sh
bifrost peers                          # list devices
bifrost chat "nightly build finished"  # broadcast
bifrost send alice ./artifact.tar.gz   # send a file
```

Useful in a CI hook or a cron job:

```sh
#!/bin/sh
if make test; then
    bifrost chat "✅ tests passed on $(hostname)"
else
    bifrost chat "❌ tests failed on $(hostname)"
fi
```

Everything is also available over HTTP on port 9000:

```sh
curl localhost:9000/api/snapshot | jq .pool
curl -X POST localhost:9000/api/command \
     -H 'Content-Type: application/json' \
     -d '{"line":"/peers"}'
```

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/snapshot` | GET | Everything, in one consistent view |
| `/api/events` | GET | Server-sent event stream |
| `/api/command` | POST | Run any slash command |
| `/api/chat` | POST | Send a message |
| `/api/upload?peer=&name=` | POST | Send a file (raw body) |
| `/api/downloads` | GET | List received files |
| `/health` | GET | Liveness |

---

## 12. Settings

Everything lives in `~/.bifrost/config.json` (`%USERPROFILE%\.bifrost\` on Windows). See it with:

```sh
bifrost config
```

```json
{
  "identity": {
    "node_id": "550e8400-e29b-41d4-a716-446655440000",
    "username": "workshop-nuc",
    "device_type": "linux-server"
  },
  "ports": { "web": 9000, "discovery": 9001, "messaging": 9002 },
  "download_dir": "/home/me/Downloads/Bifrost",
  "aliases": { "660f…": "Alice's ThinkPad" },
  "sharing_enabled": true,
  "share_fraction": 0.25,
  "auto_accept_files": true,
  "web_lan_accessible": true
}
```

Worth knowing:

| Setting | Effect |
|---|---|
| `auto_accept_files` | `false` declines every incoming file |
| `max_incoming_file_bytes` | Refuse anything larger |
| `web_lan_accessible` | `false` restricts the dashboard to this machine only |
| `share_fraction` | How much spare capacity to advertise (default a quarter) |

Change your display name at any time — peers see it on the next beacon:

```
> /me kitchen-pi
```

Environment variables:

| Variable | Effect |
|---|---|
| `BIFROST_LOG` | `trace`, `debug`, `info`, `warn`, `error` |
| `BIFROST_HOME` | Move the whole config directory |

---

## 13. When something is wrong

**Always start here:**

```sh
bifrost doctor
```

It checks your interfaces, whether every port is free, and sends a real beacon to see whether it comes back — then prints the exact fix for anything that failed.

### Peers do not appear

In order of likelihood:

1. **The devices are on different networks.** Guest Wi‑Fi and the main network are separate; so are the 2.4 GHz and 5 GHz bands on some routers. Check that both addresses share a prefix — `192.168.1.x` and `192.168.1.y` are fine, `192.168.1.x` and `10.0.0.y` are not.

2. **A firewall is blocking it.** `bifrost doctor` prints the exact command for your system.

3. **Client isolation is on.** Many routers, and most guest networks, block devices from talking to each other at all. Look for "AP isolation", "client isolation" or "guest mode" in your router's settings. To confirm quickly, make a phone hotspot and connect both machines to it — if they find each other there, it is your router.

4. **A VPN is in the way.** Tailscale, WireGuard, Radmin and similar create extra adapters. Bifrost prefers the most specific matching subnet, which usually picks the real LAN correctly, but a full-tunnel VPN can capture everything. `bifrost doctor` warns when it sees several subnets.

### "Port already in use"

Another node is running. Use it, stop it, or start this one elsewhere:

```sh
bifrost --port-offset 10
```

### A file did not arrive

Check the Transfers panel for the reason. `Failed: checksum mismatch` means the data was corrupted in flight — retry. `Declined` means the other node has `auto_accept_files` off, or the file exceeded its size limit.

### The terminal is left in a strange state

It should not happen — the dashboard restores your terminal even if it crashes — but if it does, `reset` fixes it.

### Nothing else worked

Turn up the logging and read what it says:

```sh
BIFROST_LOG=debug bifrost serve
```

With the terminal dashboard, logs go to `~/.bifrost/bifrost.log` instead of being drawn over the UI.

More in [TROUBLESHOOTING.md](TROUBLESHOOTING.md).
