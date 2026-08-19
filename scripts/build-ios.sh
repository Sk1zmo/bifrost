#!/usr/bin/env bash
# Cross-compile the Bifrost core for iOS and assemble an XCFramework.
#
#   ./scripts/build-ios.sh
#
# Produces clients/ios-swift/BifrostCore.xcframework, containing a device
# slice and a simulator slice. macOS with Xcode is required — Apple's linker
# and SDKs are not available elsewhere.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/clients/ios-swift"
FRAMEWORK="$OUT/BifrostCore.xcframework"

[ "$(uname -s)" = "Darwin" ] || { echo "iOS builds require macOS with Xcode." >&2; exit 1; }
command -v xcodebuild >/dev/null || { echo "xcodebuild not found. Install Xcode." >&2; exit 1; }

DEVICE=aarch64-apple-ios
SIM_ARM=aarch64-apple-ios-sim
SIM_X86=x86_64-apple-ios

echo "==> Adding targets"
for target in "$DEVICE" "$SIM_ARM" "$SIM_X86"; do
    rustup target add "$target" >/dev/null 2>&1 || true
done

cd "$ROOT"
for target in "$DEVICE" "$SIM_ARM" "$SIM_X86"; do
    echo "==> Building $target"
    cargo build --release --target "$target" -p bifrost-core
done

# The simulator slice must be one fat archive covering both simulator
# architectures; an XCFramework rejects two slices for the same platform.
echo "==> Merging the simulator slices"
SIM_DIR="$ROOT/target/ios-simulator"
mkdir -p "$SIM_DIR"
lipo -create \
    "$ROOT/target/$SIM_ARM/release/libbifrost_core.a" \
    "$ROOT/target/$SIM_X86/release/libbifrost_core.a" \
    -output "$SIM_DIR/libbifrost_core.a"

echo "==> Assembling the XCFramework"
HEADERS="$ROOT/target/ios-headers"
rm -rf "$HEADERS" "$FRAMEWORK"
mkdir -p "$HEADERS"
cp "$OUT/Bifrost/Bridge/bifrost_core.h" "$HEADERS/"

# A module map lets Swift import it directly, with no bridging header.
cat > "$HEADERS/module.modulemap" <<'MODULE'
module BifrostCore {
    header "bifrost_core.h"
    export *
}
MODULE

xcodebuild -create-xcframework \
    -library "$ROOT/target/$DEVICE/release/libbifrost_core.a" -headers "$HEADERS" \
    -library "$SIM_DIR/libbifrost_core.a" -headers "$HEADERS" \
    -output "$FRAMEWORK"

echo
echo "Built $FRAMEWORK"
echo
echo "In Xcode:"
echo "  1. Drag BifrostCore.xcframework into the project"
echo "  2. Target > General > Frameworks: set it to 'Embed & Sign'"
echo "  3. Add 'import BifrostCore' where the C API is used"
echo "  4. Confirm Info.plist has NSLocalNetworkUsageDescription — without it"
echo "     iOS silently drops every discovery packet"
