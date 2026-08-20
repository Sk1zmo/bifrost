# Bifrost installer for Windows.
#
#   irm https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.ps1 | iex
#
# Installs a compiler toolchain and Rust if needed, builds Bifrost, puts it on
# PATH, and adds firewall rules. Administrator rights are only needed for the
# firewall step; everything else installs per-user.
#
# This file is deliberately pure ASCII. Windows PowerShell 5.1 reads a .ps1
# without a byte-order mark as ANSI, so any UTF-8 character in the source is
# rendered as mojibake ("--" becoming "a EUR"") in the user's console.

#Requires -Version 5.1
[CmdletBinding()]
param(
    # Where to install the executable.
    [string]$Prefix = "$env:LOCALAPPDATA\Programs\Bifrost",
    [string]$Repo = 'https://github.com/Sk1zmo/bifrost',
    [string]$Branch = 'main',
    # Skip the firewall step even when running elevated.
    [switch]$NoFirewall,
    # Allow inbound on Public networks too. Needed when Windows has filed your
    # home Wi-Fi as Public, which it does more often than not.
    [switch]$AllProfiles
)

$ErrorActionPreference = 'Stop'

function Write-Step($text) { Write-Host "==> $text" -ForegroundColor White }
function Write-Ok($text)   { Write-Host "  [ok] $text" -ForegroundColor Green }
function Write-Warn($text) { Write-Host "  [!]  $text" -ForegroundColor Yellow }
function Write-Fail($text) { Write-Host "  [x]  $text" -ForegroundColor Red; exit 1 }
function Test-Command($name) { $null -ne (Get-Command $name -ErrorAction SilentlyContinue) }

# Run a native executable without PowerShell turning its stderr into a
# terminating error.
#
# Windows PowerShell 5.1 wraps every stderr line from a native command in an
# ErrorRecord, and with $ErrorActionPreference = 'Stop' that aborts the script.
# git writes ordinary progress ("Cloning into ...") to stderr, so a plain
# `git clone` would kill this installer on a completely successful clone.
# Exit code is the only reliable success signal.
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$Command,
        [string[]]$Arguments = @()
    )

    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        # `| Out-Host` matters: without it the command's stdout joins this
        # function's output, so `$code` becomes an array of every line printed
        # *plus* the exit code -- and `$code -ne 0` on an array filters rather
        # than compares, yielding a non-empty array, which is truthy. A tool
        # that succeeded while printing anything to stdout then reads as
        # failed. Out-Host writes to the console and returns nothing.
        & $Command @Arguments | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
}

Write-Host ''
Write-Host '  B I F R O S T' -ForegroundColor White
Write-Host '  Connect every device on your network' -ForegroundColor DarkGray
Write-Host ''

# --- 1. System --------------------------------------------------------------
Write-Step 'Checking this system'

$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Ok "Windows $([Environment]::OSVersion.Version) on $env:PROCESSOR_ARCHITECTURE"
if ($isAdmin) {
    Write-Ok 'Running elevated'
} else {
    Write-Warn 'Not elevated, so the firewall step will be skipped'
}
Write-Ok "Install location: $Prefix"

# --- 2. Rust ----------------------------------------------------------------
Write-Step 'Checking Rust'

if (Test-Command 'cargo') {
    Write-Ok "Rust $((rustc --version).Split(' ')[1]) is already installed"
} else {
    Write-Warn 'Rust not found, installing via rustup'
    $installer = Join-Path $env:TEMP 'rustup-init.exe'
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }

    Invoke-WebRequest -Uri "https://static.rust-lang.org/rustup/dist/$arch-pc-windows-msvc/rustup-init.exe" `
        -OutFile $installer -UseBasicParsing

    # -y accepts the defaults; the MSVC toolchain needs the Visual Studio build
    # tools, which rustup offers to fetch on its own.
    $code = Invoke-Native $installer @('-y', '--default-toolchain', 'stable')
    Remove-Item $installer -ErrorAction SilentlyContinue
    if ($code -ne 0) { Write-Fail 'rustup failed' }

    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    if (-not (Test-Command 'cargo')) {
        Write-Fail 'Rust installed but cargo is not on PATH. Open a new terminal and re-run.'
    }
    Write-Ok 'Rust installed'
}

# --- 3. Source --------------------------------------------------------------
Write-Step 'Fetching Bifrost'

$cleanup = $null
if ((Test-Path 'Cargo.toml') -and (Test-Path 'crates\core')) {
    $source = (Get-Location).Path
    Write-Ok 'Building from this directory'
} else {
    if (-not (Test-Command 'git')) { Write-Fail 'git is required to fetch the source' }

    $source = Join-Path $env:TEMP "bifrost-$(Get-Random)"
    $cleanup = $source

    $code = Invoke-Native 'git' @('clone', '--quiet', '--depth', '1', '--branch', $Branch, $Repo, $source)
    if ($code -ne 0) {
        Write-Host ''
        Write-Host '  Could not clone the repository.' -ForegroundColor Red
        Write-Host "  URL: $Repo"
        Write-Host ''
        Write-Host '  If it is private, authenticate first:' -ForegroundColor Yellow
        Write-Host '      gh auth login          (GitHub CLI)'
        Write-Host '      gh auth setup-git'
        Write-Host ''
        Write-Host '  Or run this script from inside a checkout, which skips cloning:'
        Write-Host '      cd path\to\bifrost'
        Write-Host '      .\install.ps1'
        Write-Host ''
        exit 1
    }
    Write-Ok "Cloned into $source"
}

# --- 4. Build ---------------------------------------------------------------
Write-Step 'Building (this takes a few minutes the first time)'

# The GNU toolchain shells out to MinGW's dlltool, which cannot handle a build
# path containing a space: it splits "C:\Users\Firstname Lastname\..." at the
# space and fails with "can't create C:\Users\Firstname: Permission denied".
#
# The MSVC toolchain has no such problem, so this only applies to GNU. Note
# that only the *target* directory matters, not the source checkout.
$isGnu = $false
try {
    $isGnu = ((rustc -vV) -join "`n") -match 'host:\s*\S*windows-gnu'
} catch {
    # If rustc cannot be interrogated, assume the safe path.
    $isGnu = $true
}

$targetDir = Join-Path $source 'target'

if ($isGnu) {
    Write-Warn 'GNU toolchain detected; using a space-free build directory'

    # Try each candidate and take the first that is genuinely space-free.
    # %LOCALAPPDATA% is NOT safe on its own -- it sits under the user profile,
    # which is exactly where the space usually is.
    $candidates = @()

    # Windows 8.3 short names exist precisely for this; the folder must exist
    # before it has one.
    $localAppData = $env:LOCALAPPDATA
    if ($localAppData) {
        $probe = Join-Path $localAppData 'bifrost-build'
        New-Item -ItemType Directory -Force -Path $probe | Out-Null
        try {
            $fso = New-Object -ComObject Scripting.FileSystemObject
            $candidates += $fso.GetFolder($probe).ShortPath
        } catch {
            # 8.3 name generation can be disabled system-wide.
        }
    }

    # A folder on the system drive root, which never contains a space.
    $candidates += (Join-Path $env:SystemDrive 'bifrost-build')

    $chosen = $null
    foreach ($candidate in $candidates) {
        if ([string]::IsNullOrWhiteSpace($candidate) -or $candidate.Contains(' ')) { continue }
        try {
            New-Item -ItemType Directory -Force -Path $candidate -ErrorAction Stop | Out-Null
            $chosen = $candidate
            break
        } catch {
            # Not writable (the drive root often needs elevation); try the next.
        }
    }

    if ($chosen) {
        $targetDir = $chosen
        $env:CARGO_TARGET_DIR = $targetDir
        Write-Ok "Building in $targetDir"
    } elseif ($source.Contains(' ')) {
        Write-Host ''
        Write-Host '  Cannot build here.' -ForegroundColor Red
        Write-Host "  The path contains a space ($source) and no space-free"
        Write-Host '  build directory could be created, which the GNU toolchain cannot handle.'
        Write-Host ''
        Write-Host '  Either switch to the MSVC toolchain, which is unaffected:' -ForegroundColor Yellow
        Write-Host '      rustup toolchain install stable-x86_64-pc-windows-msvc'
        Write-Host '      rustup default stable-x86_64-pc-windows-msvc'
        Write-Host ''
        Write-Host '  Or move the checkout somewhere without a space, e.g. C:\src\bifrost.'
        Write-Host ''
        exit 1
    }
}

Push-Location $source
try {
    $code = Invoke-Native 'cargo' @('build', '--release', '-p', 'bifrost-cli')
    if ($code -ne 0) { Write-Fail 'Build failed' }
} finally {
    Pop-Location
}
Write-Ok 'Built'

# --- 5. Install -------------------------------------------------------------
Write-Step 'Installing'

$built = Join-Path $targetDir 'release\bifrost.exe'
if (-not (Test-Path $built)) { Write-Fail "The build produced no executable at $built" }

New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
Copy-Item $built (Join-Path $Prefix 'bifrost.exe') -Force
Write-Ok "Installed to $Prefix\bifrost.exe"

$userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($userPath -notlike "*$Prefix*") {
    [Environment]::SetEnvironmentVariable('PATH', "$Prefix;$userPath", 'User')
    $env:PATH = "$Prefix;$env:PATH"
    Write-Ok 'Added to your PATH (open a new terminal for it to take effect)'
} else {
    Write-Ok 'Already on your PATH'
}

# --- 6. Firewall ------------------------------------------------------------
Write-Step 'Firewall'

if ($NoFirewall) {
    Write-Ok 'Skipped by request'
} elseif (-not $isAdmin) {
    Write-Warn 'Not elevated, skipping. To add the rules later, run as Administrator:'
    Write-Host  '      netsh advfirewall firewall add rule name="Bifrost UDP" dir=in action=allow protocol=UDP localport=9001,9005'
    Write-Host  '      netsh advfirewall firewall add rule name="Bifrost TCP" dir=in action=allow protocol=TCP localport=9000,9002,9003,9004'
} else {
    # Windows Firewall matches on program path, so the rule has to name the
    # binary we just installed. A rule left from a different build elsewhere
    # does nothing for this one.
    $program = Join-Path $Prefix 'bifrost.exe'

    # Which profiles to cover. Private and Domain are the safe default: a LAN
    # tool has no business accepting connections on a coffee-shop network.
    # But Windows files a great many home networks as Public, and a
    # Private-only rule then never applies -- the node runs, the dashboard
    # answers on localhost, and a phone's browser hangs forever with no error
    # because the inbound SYN is dropped rather than refused.
    $publicNetworks = @()
    try {
        $publicNetworks = @(Get-NetConnectionProfile |
            Where-Object { $_.NetworkCategory -eq 'Public' } |
            Where-Object { (Get-NetFirewallProfile -Name Public).Enabled -eq 'True' })
    } catch {
        # Older builds without the NetConnection cmdlets; carry on.
    }

    $profiles = if ($AllProfiles) { 'Any' } else { 'Private,Domain' }

    try {
        # Remove any rule from a previous install so re-running does not stack
        # duplicates in the firewall list.
        Remove-NetFirewallRule -DisplayName 'Bifrost*' -ErrorAction SilentlyContinue

        New-NetFirewallRule -DisplayName 'Bifrost UDP' -Direction Inbound -Action Allow `
            -Protocol UDP -LocalPort 9001,9005 -Profile $profiles `
            -Program $program | Out-Null
        New-NetFirewallRule -DisplayName 'Bifrost TCP' -Direction Inbound -Action Allow `
            -Protocol TCP -LocalPort 9000,9002,9003,9004 -Profile $profiles `
            -Program $program | Out-Null

        Write-Ok "Firewall rules added for: $profiles"

        if ($publicNetworks.Count -gt 0 -and -not $AllProfiles) {
            $names = ($publicNetworks | ForEach-Object { $_.InterfaceAlias }) -join ', '
            Write-Host ''
            Write-Warn "Windows has filed your active network as Public: $names"
            Write-Host '       The rules just added do NOT cover Public, so other devices'
            Write-Host '       will not reach this node. Pick one:'
            Write-Host ''
            Write-Host '       1. Mark it Private (correct for a home or office network):' -ForegroundColor Yellow
            foreach ($p in $publicNetworks) {
                Write-Host "          Set-NetConnectionProfile -InterfaceAlias '$($p.InterfaceAlias)' -NetworkCategory Private"
            }
            Write-Host ''
            Write-Host '       2. Or re-run this installer with -AllProfiles.' -ForegroundColor Yellow
            Write-Host ''
        }
    } catch {
        Write-Warn "Could not add firewall rules: $_"
    }
}

# --- 7. Service -------------------------------------------------------------
Write-Step 'Background service'

if ([Environment]::UserInteractive -and -not $env:CI) {
    $answer = Read-Host '  Start Bifrost automatically at sign-in? [y/N]'
    if ($answer -match '^[Yy]') {
        Invoke-Native (Join-Path $Prefix 'bifrost.exe') @('service', 'install') | Out-Null
        Write-Ok 'Service installed'
    } else {
        Write-Ok 'Skipped, run it yourself with: bifrost'
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
