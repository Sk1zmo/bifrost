#!/usr/bin/env bash
# Build release binaries for every desktop platform this host can target.
#
#   ./scripts/build-all.sh
#
# Skips targets whose linker is missing rather than failing the whole run, so
# it produces whatever this machine can actually build.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"
VERSION=$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)

mkdir -p "$DIST"
echo "Building Bifrost $VERSION into $DIST"
echo

build() {
    local target="$1" label="$2" binary="${3:-bifrost}"

    echo "==> $label ($target)"
    rustup target add "$target" >/dev/null 2>&1 || true

    if ( cd "$ROOT" && cargo build --release -p bifrost-cli --target "$target" ) 2>/dev/null; then
        local out="$DIST/bifrost-$VERSION-$label"
        [ "$binary" = "bifrost.exe" ] && out="$out.exe"
        cp "$ROOT/target/$target/release/$binary" "$out"
        echo "    -> $(basename "$out")"
    else
        echo "    skipped (no linker for this target on this host)"
    fi
}

case "$(uname -s)" in
    Linux)
        build x86_64-unknown-linux-gnu linux-x86_64
        build aarch64-unknown-linux-gnu linux-arm64
        build x86_64-pc-windows-gnu windows-x86_64 bifrost.exe
        ;;
    Darwin)
        build aarch64-apple-darwin macos-arm64
        build x86_64-apple-darwin macos-x86_64
        ;;
    *)
        build x86_64-pc-windows-msvc windows-x86_64 bifrost.exe
        ;;
esac

echo
echo "Artifacts:"
ls -lh "$DIST" 2>/dev/null | tail -n +2 | awk '{printf "  %-42s %s\n", $9, $5}'
