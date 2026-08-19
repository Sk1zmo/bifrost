#!/usr/bin/env bash
# Cross-compile the Bifrost core for Android and drop the result into the
# Flutter project's jniLibs.
#
#   ANDROID_NDK_HOME=~/Android/Sdk/ndk/26.1.10909125 ./scripts/build-android.sh
#
# Builds all four ABIs by default. Pass ABIs as arguments to narrow it, e.g.
#   ./scripts/build-android.sh arm64-v8a

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
JNI_LIBS="$ROOT/clients/flutter/android/app/src/main/jniLibs"
API_LEVEL="${ANDROID_API_LEVEL:-24}"   # Android 7.0, matching Flutter's floor

# ABI → (rust target, toolchain prefix)
ALL_ABIS=(arm64-v8a armeabi-v7a x86_64 x86)
ABIS=("${@:-${ALL_ABIS[@]}}")

rust_target_for() {
    case "$1" in
        arm64-v8a)   echo aarch64-linux-android ;;
        armeabi-v7a) echo armv7-linux-androideabi ;;
        x86_64)      echo x86_64-linux-android ;;
        x86)         echo i686-linux-android ;;
        *) echo "unknown ABI: $1" >&2; exit 1 ;;
    esac
}

# The clang wrapper name differs from the Rust target for 32-bit ARM.
clang_for() {
    case "$1" in
        arm64-v8a)   echo "aarch64-linux-android${API_LEVEL}-clang" ;;
        armeabi-v7a) echo "armv7a-linux-androideabi${API_LEVEL}-clang" ;;
        x86_64)      echo "x86_64-linux-android${API_LEVEL}-clang" ;;
        x86)         echo "i686-linux-android${API_LEVEL}-clang" ;;
    esac
}

if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    echo "ANDROID_NDK_HOME is not set." >&2
    echo "Install the NDK through Android Studio, then point it at e.g." >&2
    echo "  export ANDROID_NDK_HOME=\$HOME/Android/Sdk/ndk/26.1.10909125" >&2
    exit 1
fi

case "$(uname -s)" in
    Linux)  HOST_TAG=linux-x86_64 ;;
    Darwin) HOST_TAG=darwin-x86_64 ;;   # correct for Apple silicon too
    *)      HOST_TAG=windows-x86_64 ;;
esac

TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$HOST_TAG/bin"
[ -d "$TOOLCHAIN" ] || { echo "No NDK toolchain at $TOOLCHAIN" >&2; exit 1; }

echo "NDK      $ANDROID_NDK_HOME"
echo "API      $API_LEVEL"
echo "ABIs     ${ABIS[*]}"
echo

for abi in "${ABIS[@]}"; do
    target=$(rust_target_for "$abi")
    clang="$TOOLCHAIN/$(clang_for "$abi")"

    echo "==> $abi ($target)"
    rustup target add "$target" >/dev/null 2>&1 || true

    # Cargo reads the linker from an env var named after the target, upper-cased
    # with hyphens replaced by underscores.
    linker_var="CARGO_TARGET_$(echo "$target" | tr '[:lower:]-' '[:upper:]_')_LINKER"
    export "$linker_var=$clang"
    export "CC_$(echo "$target" | tr '-' '_')=$clang"
    export "AR_$(echo "$target" | tr '-' '_')=$TOOLCHAIN/llvm-ar"

    ( cd "$ROOT" && cargo build --release --target "$target" -p bifrost-core )

    mkdir -p "$JNI_LIBS/$abi"
    cp "$ROOT/target/$target/release/libbifrost_core.so" "$JNI_LIBS/$abi/"
    echo "    -> $JNI_LIBS/$abi/libbifrost_core.so"
done

echo
echo "Done. Build the app with:"
echo "    cd clients/flutter && flutter build apk --release"
