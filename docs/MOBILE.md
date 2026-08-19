# Android and iOS

There are two ways to put a phone on a Bifrost network. Pick the one that fits.

| | Web dashboard | Native app |
|---|---|---|
| Install | none — open a URL | build and sideload |
| The phone is | a client of another node | a node of its own |
| Discovers peers itself | no | yes |
| Receives files directly | via the node it controls | yes, to the phone |
| Works with every node asleep | no | yes |
| Microphone | HTTPS/localhost only | yes |

**Most people want the web dashboard.** Open the address a node prints and you
get peers, chat, files, telemetry and push-to-talk with nothing installed.
Everything below is for making the phone a real peer.

---

## Which client

Two are included and both link the same Rust core:

- **`clients/flutter`** — one codebase, builds for **Android and iOS**. The
  recommended path.
- **`clients/ios-swift`** — native SwiftUI, iOS only, for anyone who would
  rather ship pure Swift.

---

## Android

### Prerequisites

- [Flutter](https://docs.flutter.dev/get-started/install) 3.3+
- Android Studio, with the **NDK** installed (SDK Manager → SDK Tools → NDK)
- Rust with the Android targets

### Build the core

```sh
export ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/26.1.10909125
./scripts/build-android.sh
```

This builds `libbifrost_core.so` for all four ABIs and copies each into
`clients/flutter/android/app/src/main/jniLibs/<abi>/`. Narrow it if you only
need one:

```sh
./scripts/build-android.sh arm64-v8a
```

| ABI | Rust target | Covers |
|---|---|---|
| `arm64-v8a` | `aarch64-linux-android` | Every modern phone |
| `armeabi-v7a` | `armv7-linux-androideabi` | Older 32-bit devices |
| `x86_64` | `x86_64-linux-android` | Emulators |
| `x86` | `i686-linux-android` | Old emulators |

### Build the app

```sh
cd clients/flutter
flutter pub get
flutter run                    # to a connected device
flutter build apk --release    # a distributable APK
```

The APK lands in `build/app/outputs/flutter-apk/app-release.apk`.

### Permissions

Add these to `android/app/src/main/AndroidManifest.xml`:

```xml
<uses-permission android:name="android.permission.INTERNET" />
<uses-permission android:name="android.permission.ACCESS_NETWORK_STATE" />
<uses-permission android:name="android.permission.ACCESS_WIFI_STATE" />
<uses-permission android:name="android.permission.CHANGE_WIFI_MULTICAST_STATE" />
<uses-permission android:name="android.permission.RECORD_AUDIO" />

<!-- Android 13+ requires this to see devices on the local network. -->
<uses-permission android:name="android.permission.NEARBY_WIFI_DEVICES"
                 android:usesPermissionFlags="neverForLocation" />
```

Bifrost sends plain broadcast alongside multicast, which usually reaches
Android without a `MulticastLock`. If discovery is unreliable on a particular
device, acquiring one in the host activity is the fix.

### Cleartext HTTP

The web dashboard is plain HTTP. If the app ever loads a peer's dashboard in a
webview, Android 9+ blocks cleartext unless you allow it for private ranges via
a `network_security_config.xml`. Opening the dashboard in the system browser —
what `SettingsScreen` does — needs no such exception.

---

## iOS

Requires macOS with Xcode.

### Build the core

```sh
./scripts/build-ios.sh
```

This produces `clients/ios-swift/BifrostCore.xcframework`, containing a device
slice (`aarch64-apple-ios`) and a simulator slice (arm64 + x86_64 merged with
`lipo`, since an XCFramework rejects two slices for the same platform).

### Native SwiftUI app

1. Create an iOS App target in Xcode, or open `clients/ios-swift`.
2. Add the Swift sources under `Bifrost/`.
3. Drag `BifrostCore.xcframework` into the project; set it to **Embed & Sign**
   under Target → General → Frameworks.
4. Either `import BifrostCore`, or add `bifrost_core.h` to a bridging header.
5. Use the provided `Info.plist`, or copy the keys below into yours.

### Flutter on iOS

```sh
cd clients/flutter
flutter run -d ios
```

Link the static library into the Runner target the same way. `BifrostFfi.open()`
already uses `DynamicLibrary.process()` on iOS, because a statically linked
library's symbols live in the app binary and there is nothing to `dlopen`.

### The permission that decides whether it works at all

```xml
<key>NSLocalNetworkUsageDescription</key>
<string>Bifrost finds the other devices on your Wi-Fi so they can chat, share files and call each other. Nothing leaves your local network.</string>
<key>NSBonjourServices</key>
<array>
    <string>_bifrost._udp</string>
    <string>_bifrost._tcp</string>
</array>
<key>NSMicrophoneUsageDescription</key>
<string>Bifrost uses the microphone for voice calls to other devices on your network.</string>
```

**Without `NSLocalNetworkUsageDescription`, iOS silently drops every broadcast
and multicast packet.** No error, no exception — discovery simply finds
nothing. If a user declines the prompt the same thing happens, and the only fix
is Settings → Bifrost → Local Network.

This is the single most common reason an otherwise-correct iOS build sees no
peers.

### Background

iOS suspends a backgrounded app's sockets, so a phone stops beaconing and its
peers mark it offline after 12 seconds. The included `Info.plist` requests the
`voip` background mode, which keeps it alive during a call. For general
background presence, iOS offers no mechanism a LAN tool can legitimately use —
the phone reappears the moment the app is foregrounded.

---

## What the FFI looks like

Both clients talk to the same C ABI, specified in
[`clients/ios-swift/Bifrost/Bridge/bifrost_core.h`](../clients/ios-swift/Bifrost/Bridge/bifrost_core.h)
and implemented in `crates/core/src/ffi.rs`.

```c
bifrost_start("/path/to/documents/bifrost/config.json");

char *json = bifrost_snapshot_json();     /* everything, in one view */
/* … render … */
bifrost_string_free(json);                /* the caller owns it */

bifrost_send_chat("hello", NULL);         /* NULL recipient = broadcast */
bifrost_execute("/sendfile bob report.pdf");

bifrost_stop();
```

Three rules:

1. **Every returned string is yours to free** with `bifrost_string_free`.
   `NULL` means failure — ask `bifrost_last_error()`.
2. **No panic escapes.** Unwinding across an FFI boundary is undefined
   behaviour, so every entry point catches and reports instead.
3. **Events are polled**, via `bifrost_poll_events()`, from whatever timer the
   UI already runs.

Pass an explicit config path on both platforms: neither gives an app a writable
working directory, so the Rust default would fail on device. The Dart and Swift
wrappers both use the app's documents directory.

---

## Checking it works

1. Run `bifrost` on a laptop on the same Wi-Fi.
2. Launch the app.
3. The laptop should appear in Devices within about five seconds, with live CPU
   and memory.
4. Send a message each way.
5. Send a file to the phone; it lands in the app's documents directory.
6. Run `bifrost peers` on the laptop — the phone should be listed with
   `device_type` `android-phone` or `iphone`.

If step 3 fails, see [TROUBLESHOOTING.md § Mobile](TROUBLESHOOTING.md#mobile).
On iOS, check the local network permission first — it is nearly always that.
