# Bifrost installer for Windows.
#
#   irm https://raw.githubusercontent.com/OWNER/bifrost/main/install.ps1 | iex
#
# Installs a compiler toolchain and Rust if needed, builds Bifrost, puts it on
# PATH, and adds firewall rules. Administrator rights are only needed for the
# firewall step; everything else installs per-user.

#Requires -Version 5.1
[CmdletBinding()]
param(
    # Where to install the executable.
    [string]$Prefix = "$env:LOCALAPPDATA\Programs\Bifrost",
    [string]$Repo = 'https://github.com/OWNER/bifrost',
    [string]$Branch = 'main',
    # Skip the firewall step even when running elevated.
    [switch]$NoFirewall
)

$ErrorActionPreference = 'Stop'

function Write-Step($text) { Write-Host "==> $text" -ForegroundColor White }
function Write-Ok($text)   { Write-Host "  [ok] $text" -ForegroundColor Green }
function Write-Warn($text) { Write-Host "  [!]  $text" -ForegroundColor Yellow }
function Write-Fail($text) { Write-Host "  [x]  $text" -ForegroundColor Red; exit 1 }
function Test-Command($name) { $null -ne (Get-Command $name -ErrorAction SilentlyContinue) }

Write-Host ''
Write-Host '  B I F R O S T' -ForegroundColor White
Write-Host '  Connect every device on your network' -ForegroundColor DarkGray
Write-Host ''

# ── 1. System ────────────────────────────────────────────────────────────────
Write-Step 'Checking this system'

$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Ok "Windows $([Environment]::OSVersion.Version) on $env:PROCESSOR_ARCHITECTURE"
if ($isAdmin) { Write-Ok 'Running elevated' } else { Write-Warn 'Not elevated — the firewall step will be skipped' }
Write-Ok "Install location: $Prefix"

# ── 2. Rust ──────────────────────────────────────────────────────────────────
Write-Step 'Checking Rust'

if (Test-Command 'cargo') {
    Write-Ok "Rust $((rustc --version).Split(' ')[1]) is already installed"
} else {
    Write-Warn 'Rust not found; installing via rustup'
    $installer = Join-Path $env:TEMP 'rustup-init.exe'
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }

    Invoke-WebRequest -Uri "https://static.rust-lang.org/rustup/dist/$arch-pc-windows-msvc/rustup-init.exe" `
        -OutFile $installer -UseBasicParsing

    # -y accepts defaults; the MSVC toolchain needs the Visual Studio build
    # tools, which rustup offers to fetch on its own.
    & $installer -y --default-toolchain stable | Out-Host
    Remove-Item $installer -ErrorAction SilentlyContinue

    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    if (-not (Test-Command 'cargo')) {
        Write-Fail 'Rust installed but cargo is not on PATH. Open a new terminal and re-run.'
    }
    Write-Ok 'Rust installed'
}

# ── 3. Source ────────────────────────────────────────────────────────────────
Write-Step 'Fetching Bifrost'

$cleanup = $null
if ((Test-Path 'Cargo.toml') -and (Test-Path 'crates\core')) {
    $source = (Get-Location).Path
    Write-Ok 'Building from this directory'
} else {
    if (-not (Test-Command 'git')) { Write-Fail 'git is required to fetch the source' }
    $source = Join-Path $env:TEMP "bifrost-$(Get-Random)"
    $cleanup = $source
    git clone --depth 1 --branch $Branch $Repo $source 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Fail "Could not clone $Repo" }
    Write-Ok "Cloned into $source"
}

# ── 4. Build ─────────────────────────────────────────────────────────────────
Write-Step 'Building (this takes a few minutes the first time)'

# MinGW's dlltool cannot handle a path containing a space, and the default
# checkout location often has one ("C:\Users\Firstname Lastname\..."). Building
# into a space-free directory avoids the failure entirely on the GNU toolchain.
$targetDir = Join-Path $env:LOCALAPPDATA 'bifrost-build'
New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
$env:CARGO_TARGET_DIR = $targetDir

Push-Location $source
try {
    cargo build --release -p bifrost-cli
    if ($LASTEXITCODE -ne 0) { Write-Fail 'Build failed' }
} finally {
    Pop-Location
}
Write-Ok 'Built'

# ── 5. Install ───────────────────────────────────────────────────────────────
Write-Step 'Installing'

New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
Copy-Item (Join-Path $targetDir 'release\bifrost.exe') (Join-Path $Prefix 'bifrost.exe') -Force
Write-Ok "Installed to $Prefix\bifrost.exe"

$userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($userPath -notlike "*$Prefix*") {
    [Environment]::SetEnvironmentVariable('PATH', "$Prefix;$userPath", 'User')
    $env:PATH = "$Prefix;$env:PATH"
    Write-Ok 'Added to your PATH (open a new terminal for it to take effect)'
} else {
    Write-Ok 'Already on your PATH'
}

# ── 6. Firewall ──────────────────────────────────────────────────────────────
Write-Step 'Firewall'

if ($NoFirewall) {
    Write-Ok 'Skipped by request'
} elseif (-not $isAdmin) {
    Write-Warn 'Not elevated — skipping. To add the rules later, run as Administrator:'
    Write-Host  '      netsh advfirewall firewall add rule name="Bifrost UDP" dir=in action=allow protocol=UDP localport=9001,9005'
    Write-Host  '      netsh advfirewall firewall add rule name="Bifrost TCP" dir=in action=allow protocol=TCP localport=9000,9002,9003,9004'
} else {
    try {
        # Remove any rule from a previous install so re-running does not stack
        # duplicates in the firewall list.
        Remove-NetFirewallRule -DisplayName 'Bifrost*' -ErrorAction SilentlyContinue

        New-NetFirewallRule -DisplayName 'Bifrost UDP' -Direction Inbound -Action Allow `
            -Protocol UDP -LocalPort 9001,9005 -Profile Private,Domain `
            -Program (Join-Path $Prefix 'bifrost.exe') | Out-Null
        New-NetFirewallRule -DisplayName 'Bifrost TCP' -Direction Inbound -Action Allow `
            -Protocol TCP -LocalPort 9000,9002,9003,9004 -Profile Private,Domain `
            -Program (Join-Path $Prefix 'bifrost.exe') | Out-Null

        # Private and Domain only: a LAN tool has no business accepting
        # connections on a public coffee-shop network.
        Write-Ok 'Firewall rules added for private and domain networks'
    } catch {
        Write-Warn "Could not add firewall rules: $_"
    }
}

# ── 7. Service ───────────────────────────────────────────────────────────────
Write-Step 'Background service'

if ([Environment]::UserInteractive -and -not $env:CI) {
    $answer = Read-Host '  Start Bifrost automatically at sign-in? [y/N]'
    if ($answer -match '^[Yy]') {
        & (Join-Path $Prefix 'bifrost.exe') service install
        Write-Ok 'Service installed'
    } else {
        Write-Ok 'Skipped — run it yourself with: bifrost'
    }
} else {
    Write-Ok 'Skipped (non-interactive). Install later with: bifrost service install'
}

if ($cleanup) { Remove-Item -Recurse -Force $cleanup -ErrorAction SilentlyContinue }

Write-Host ''
Write-Host '  Installed.' -ForegroundColor Green
Write-Host ''
Write-Host '  Start it:          bifrost'
Write-Host '  Check the network: bifrost doctor'
Write-Host '  Run headless:      bifrost serve'
Write-Host ''
Write-Host '  Then run the same on another machine on this network.'
Write-Host '  Open the printed dashboard address on your phone.'
Write-Host ''
