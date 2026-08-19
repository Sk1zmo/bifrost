# Troubleshooting

Run this first. It checks everything below automatically and prints the fix:

```sh
bifrost doctor
```

---

## Peers do not appear

The most common problem, and almost always the network rather than Bifrost.

### 1. Are both devices actually on the same network?

Compare the addresses each node prints. `192.168.1.24` and `192.168.1.31` share a network; `192.168.1.24` and `10.0.0.5` do not.

Common causes of a split you did not intend:

- **Guest Wi-Fi.** A separate network by design.
- **2.4 GHz and 5 GHz as separate SSIDs.** Some routers bridge them, some do not.
- **A mesh system or range extender** in client-isolation mode.
- **Ethernet and Wi-Fi on different subnets.**

### 2. Client isolation

Many routers — and nearly every guest network, hotel and café — block devices from reaching each other at all. Nothing running on the device can work around it.

Look for **AP isolation**, **client isolation**, **station isolation** or **guest mode** in your router's settings.

**To confirm in one minute:** turn on a phone hotspot, connect both machines to it, run `bifrost` on each. If they find each other there, it is your router.

### 3. Firewall

`bifrost doctor` prints the exact command. In short:

```sh
# Linux, ufw
sudo ufw allow 9001/udp && sudo ufw allow 9005/udp
sudo ufw allow 9000/tcp && sudo ufw allow 9002/tcp
sudo ufw allow 9003/tcp && sudo ufw allow 9004/tcp

# Linux, firewalld
sudo firewall-cmd --permanent \
  --add-port=9001/udp --add-port=9005/udp \
  --add-port=9000/tcp --add-port=9002/tcp \
  --add-port=9003/tcp --add-port=9004/tcp
sudo firewall-cmd --reload
```

```powershell
# Windows, as Administrator
netsh advfirewall firewall add rule name="Bifrost UDP" dir=in action=allow protocol=UDP localport=9001,9005
netsh advfirewall firewall add rule name="Bifrost TCP" dir=in action=allow protocol=TCP localport=9000,9002,9003,9004
```

On macOS, System Settings → Network → Firewall → Options → allow incoming connections for `bifrost`.

**Windows specifically:** the first run usually raises a Windows Defender prompt. If you dismissed it, Windows remembers the block. Delete the rule and let it ask again:

```powershell
Remove-NetFirewallRule -DisplayName "*bifrost*"
```

### 4. A VPN is capturing your traffic

Tailscale, WireGuard, ZeroTier, Radmin and corporate VPN clients all add network interfaces. Bifrost prefers the most specific matching subnet, which normally picks your real LAN correctly, but a full-tunnel VPN can capture everything.

`bifrost doctor` warns when it sees several subnets and lists them. If peers appear but you cannot connect to them, disconnect the VPN and retry.

### 5. Docker or WSL adapters

Same shape of problem. They usually sit on their own subnets and are harmless, but they can confuse a network stack. `bifrost doctor` lists every interface it found.

### 6. Broadcast is being dropped

Some Wi-Fi drivers and access points drop broadcast to save airtime. Bifrost sends to limited broadcast, subnet-directed broadcast *and* multicast specifically to survive this, but a sufficiently aggressive network can drop all three.

Test with a wired connection or a phone hotspot. If it works there and not on your Wi-Fi, the access point is the problem.

---

## "Address already in use"

Another Bifrost node is running on this machine.

Find it:

```sh
# Linux
ss -lptn 'sport = :9002'
# macOS
lsof -nP -i :9002
# Windows
netstat -ano | findstr :9002
```

Or just run a second node beside it:

```sh
bifrost --port-offset 10
```

Discovery deliberately stays on 9001, so the two still see each other.

Note that "9001 is in use" for UDP is **normal and harmless** — the discovery socket is shared on purpose.

---

## Files

### A transfer failed with "checksum mismatch"

The data was corrupted in flight. The file was discarded rather than saved, which is the intended behaviour. Retry. Repeated failures on the same network point at faulty hardware or a very lossy Wi-Fi link.

### "Transfer declined"

The other node has `auto_accept_files` set to `false`, or the file exceeds its `max_incoming_file_bytes`. Both live in that node's `~/.bifrost/config.json`.

### Where did my file go?

`~/Downloads/Bifrost` by default. Check with `bifrost config`, or look at the Transfers panel — a completed transfer shows the full path it was saved to.

If a file with that name already existed, yours arrived as `report (1).pdf`.

---

## The dashboard

### It will not load from my phone

- Use the address Bifrost printed, not `localhost` — `localhost` on your phone means your phone.
- Confirm the phone is on the same Wi-Fi, not mobile data.
- Check that port 9000 is open in the firewall.
- If `web_lan_accessible` is `false` in the config, the dashboard is bound to `127.0.0.1` and is deliberately unreachable from anywhere else.

### It loads but shows nothing

Open it again — a hard refresh clears a stale cached copy after an upgrade. If the connection dot reads "Reconnecting…", the node stopped.

### Voice does not work in the browser

Browsers only allow microphone access over HTTPS **or** on `localhost`. On a phone, an `http://192.168.x.x` page is neither, and Chrome and Safari will refuse.

Two ways round it:

- Open the dashboard on the machine itself at `http://localhost:9000`, where microphone access is allowed.
- Use the phone app, which uses the platform microphone directly and is unaffected.

---

## The terminal dashboard

### It is drawing over itself

Your terminal is too small. Bifrost needs about 80×24. Resize and it redraws.

### My terminal is broken after a crash

It should not be — the dashboard restores your terminal on every exit path including a panic. If it happens anyway:

```sh
reset
```

### I cannot see log messages

That is deliberate: logs would be drawn over the UI. They go to `~/.bifrost/bifrost.log`.

```sh
tail -f ~/.bifrost/bifrost.log
```

---

## Building

### Windows: "dlltool could not create import library"

The MinGW toolchain cannot handle a path containing a space, and your checkout is somewhere like `C:\Users\First Last\bifrost`.

Either build somewhere without a space, or redirect just the build directory:

```powershell
$env:CARGO_TARGET_DIR = "C:\bifrost-build"
cargo build --release -p bifrost-cli
```

Or switch to the MSVC toolchain, which is unaffected:

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

### Linux: "linker `cc` not found"

Install a compiler:

```sh
sudo apt install build-essential      # Debian, Ubuntu
sudo dnf install gcc                  # Fedora, RHEL
sudo pacman -S base-devel             # Arch
sudo apk add build-base               # Alpine
```

### macOS: "xcrun: error"

```sh
xcode-select --install
```

---

## Docker

### Peers do not appear from a container

`network_mode: host` is required. Docker's default bridge network puts the container on its own subnet, where no broadcast reaches your real LAN.

Host networking is limited on Docker Desktop for macOS and Windows — run Bifrost natively there. The container is intended for Linux servers.

---

## Mobile

### Android: the app starts but finds nothing

- The native library must be present. Check `clients/flutter/android/app/src/main/jniLibs/<abi>/libbifrost_core.so` exists; run `./scripts/build-android.sh` if not.
- Android 13+ needs `NEARBY_WIFI_DEVICES` granted.
- Some Android builds block multicast unless a `MulticastLock` is held; Bifrost also sends plain broadcast, which usually gets through regardless.

### iOS: the app starts but finds nothing

Almost always the local network permission. `NSLocalNetworkUsageDescription` must be in `Info.plist`, and the user must have accepted the prompt. **iOS drops every broadcast packet without any error if it was refused.**

Check Settings → Bifrost → Local Network. If the toggle is off, turn it on. If the prompt never appeared, delete and reinstall the app.

---

## Still stuck

Turn up the logging and read what it actually says:

```sh
BIFROST_LOG=debug bifrost serve
```

Look for:

- `discovery listening` — the listener bound successfully
- `beacon sent` at `trace` level — beacons are going out
- `discovered a new peer` — a beacon arrived
- `messaging listening`, `file transfer listening` — the TCP services bound

If you see `discovery listening` and no `discovered a new peer` on either machine after a minute, it is the network, not Bifrost — go back to client isolation and firewalls above.
