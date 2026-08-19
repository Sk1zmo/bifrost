#!/usr/bin/env sh
# Bifrost installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/OWNER/bifrost/main/install.sh | sh
#
# Deliberately POSIX sh, not bash: Alpine, OpenWrt and a fair number of
# minimal container images ship no bash, and a LAN tool that will not install
# on a Raspberry Pi or an Alpine box is not "plug and play".
#
# What it does:
#   1. detects the distribution and installs a compiler if one is missing
#   2. installs Rust through rustup if cargo is absent
#   3. builds and installs `bifrost`
#   4. opens the firewall ports
#   5. optionally installs a background service
#
# Set BIFROST_PREFIX to change the install location (default /usr/local/bin,
# or ~/.local/bin when not running as root).

set -eu

REPO_URL="${BIFROST_REPO:-https://github.com/OWNER/bifrost}"
BRANCH="${BIFROST_BRANCH:-main}"
PREFIX="${BIFROST_PREFIX:-}"

# ── Output ───────────────────────────────────────────────────────────────────
# Colour only when stdout is a terminal, so piping into a log stays readable.
if [ -t 1 ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m'); RESET=$(printf '\033[0m')
else
    BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; RESET=''
fi

say()  { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$1"; }
ok()   { printf '  %s✓%s %s\n' "$GREEN" "$RESET" "$1"; }
warn() { printf '  %s!%s %s\n' "$YELLOW" "$RESET" "$1"; }
die()  { printf '  %s✗%s %s\n' "$RED" "$RESET" "$1" >&2; exit 1; }
has()  { command -v "$1" >/dev/null 2>&1; }

# Run a command with sudo only when we are not already root.
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif has sudo; then
        sudo "$@"
    else
        warn "skipped (needs root and sudo is unavailable): $*"
        return 1
    fi
}

printf '\n'
printf '  %s⬡  B I F R O S T%s\n' "$BOLD" "$RESET"
printf '  %sConnect every device on your network%s\n\n' "$DIM" "$RESET"

# ── 1. Platform ──────────────────────────────────────────────────────────────
say "Checking this system"

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
    Linux|Darwin) ok "$OS on $ARCH" ;;
    *) die "$OS is not supported by this script. On Windows use install.ps1." ;;
esac

DISTRO=""
if [ "$OS" = "Linux" ] && [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    DISTRO=$(. /etc/os-release && echo "${ID:-}")
    [ -n "$DISTRO" ] && ok "Distribution: $DISTRO"
fi

if [ -z "$PREFIX" ]; then
    if [ "$(id -u)" -eq 0 ]; then PREFIX=/usr/local/bin; else PREFIX="$HOME/.local/bin"; fi
fi
ok "Install location: $PREFIX"

# ── 2. Build dependencies ────────────────────────────────────────────────────
say "Checking build dependencies"

install_packages() {
    case "$DISTRO" in
        ubuntu|debian|raspbian|linuxmint|pop|elementary)
            as_root apt-get update -qq && as_root apt-get install -y -qq "$@" ;;
        fedora|rhel|centos|rocky|almalinux)
            as_root dnf install -y -q "$@" ;;
        arch|manjaro|endeavouros)
            as_root pacman -Sy --noconfirm --quiet "$@" ;;
        opensuse*|sles)
            as_root zypper install -y "$@" ;;
        alpine)
            as_root apk add --quiet "$@" ;;
        *)
            return 1 ;;
    esac
}

if [ "$OS" = "Linux" ]; then
    if has cc || has gcc || has clang; then
        ok "A C compiler is present"
    else
        warn "No C compiler found; installing one"
        case "$DISTRO" in
            ubuntu|debian|raspbian|linuxmint|pop|elementary) install_packages build-essential curl ;;
            fedora|rhel|centos|rocky|almalinux) install_packages gcc curl ;;
            arch|manjaro|endeavouros) install_packages base-devel curl ;;
            opensuse*|sles) install_packages gcc curl ;;
            alpine) install_packages build-base curl ;;
            *) die "Unknown distribution. Install a C compiler and curl, then re-run." ;;
        esac
        ok "Compiler installed"
    fi
elif ! has cc; then
    die "Xcode command line tools are missing. Run: xcode-select --install"
fi

# ── 3. Rust ──────────────────────────────────────────────────────────────────
say "Checking Rust"

if has cargo; then
    ok "Rust $(rustc --version 2>/dev/null | cut -d' ' -f2) is already installed"
else
    warn "Rust not found; installing via rustup"
    has curl || die "curl is required to install Rust"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
    ok "Rust installed"
fi

has cargo || die "cargo is still not on PATH. Open a new shell and re-run."

# ── 4. Source ────────────────────────────────────────────────────────────────
say "Fetching Bifrost"

# Run from a checkout if there is one, otherwise clone into a temp directory.
if [ -f "Cargo.toml" ] && [ -d "crates/core" ]; then
    SOURCE_DIR=$(pwd)
    CLEANUP=""
    ok "Building from this directory"
else
    has git || die "git is required to fetch the source"
    SOURCE_DIR=$(mktemp -d)
    CLEANUP="$SOURCE_DIR"
    git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$SOURCE_DIR" >/dev/null 2>&1 \
        || die "Could not clone $REPO_URL"
    ok "Cloned into $SOURCE_DIR"
fi
# shellcheck disable=SC2064
[ -n "$CLEANUP" ] && trap "rm -rf '$CLEANUP'" EXIT INT TERM

# ── 5. Build ─────────────────────────────────────────────────────────────────
say "Building (this takes a few minutes the first time)"

# cargo cannot link through MinGW when the path contains a space; irrelevant
# on Unix, but keeping the target dir explicit also survives a read-only
# source tree.
( cd "$SOURCE_DIR" && cargo build --release -p bifrost-cli ) || die "Build failed"
ok "Built"

# ── 6. Install ───────────────────────────────────────────────────────────────
say "Installing"

mkdir -p "$PREFIX"
install -m 755 "$SOURCE_DIR/target/release/bifrost" "$PREFIX/bifrost" \
    || cp "$SOURCE_DIR/target/release/bifrost" "$PREFIX/bifrost"
chmod 755 "$PREFIX/bifrost"
ok "Installed to $PREFIX/bifrost"

case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        warn "$PREFIX is not on your PATH. Add this to your shell profile:"
        printf '      export PATH="%s:$PATH"\n' "$PREFIX"
        ;;
esac

# ── 7. Firewall ──────────────────────────────────────────────────────────────
say "Opening firewall ports"

PORTS_TCP="9000 9002 9003 9004"
PORTS_UDP="9001 9005"

if [ "$OS" = "Linux" ]; then
    if has ufw && as_root ufw status >/dev/null 2>&1; then
        for p in $PORTS_TCP; do as_root ufw allow "$p"/tcp >/dev/null 2>&1 || true; done
        for p in $PORTS_UDP; do as_root ufw allow "$p"/udp >/dev/null 2>&1 || true; done
        ok "ufw rules added"
    elif has firewall-cmd; then
        for p in $PORTS_TCP; do as_root firewall-cmd --permanent --add-port="$p"/tcp >/dev/null 2>&1 || true; done
        for p in $PORTS_UDP; do as_root firewall-cmd --permanent --add-port="$p"/udp >/dev/null 2>&1 || true; done
        as_root firewall-cmd --reload >/dev/null 2>&1 || true
        ok "firewalld rules added"
    else
        warn "No recognised firewall. If peers do not appear, allow TCP $PORTS_TCP and UDP $PORTS_UDP."
    fi
else
    warn "macOS may prompt to allow incoming connections the first time — choose Allow."
fi

# ── 8. Service ───────────────────────────────────────────────────────────────
say "Background service"

# Only offer it interactively; a piped install must not block waiting on stdin.
if [ -t 0 ]; then
    printf '  Start Bifrost automatically at login? [y/N] '
    read -r answer
    case "$answer" in
        [Yy]*) "$PREFIX/bifrost" service install && ok "Service installed" ;;
        *) ok "Skipped — run it yourself with: bifrost" ;;
    esac
else
    ok "Skipped (non-interactive). Install later with: bifrost service install"
fi

# ── Done ─────────────────────────────────────────────────────────────────────
printf '\n'
printf '  %sInstalled.%s\n\n' "$GREEN$BOLD" "$RESET"
printf '  Start it:            %sbifrost%s\n' "$BOLD" "$RESET"
printf '  Check the network:   %sbifrost doctor%s\n' "$BOLD" "$RESET"
printf '  Run headless:        %sbifrost serve%s\n\n' "$BOLD" "$RESET"
printf '  Then run the same on another machine on this network.\n'
printf '  Open the printed dashboard address on your phone.\n\n'
