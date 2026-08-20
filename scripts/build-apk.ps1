# Build the Bifrost Android APK from a clean checkout, on Windows.
#
#   .\scripts\build-apk.ps1
#
# Cross-compiles the Rust core for both ARM ABIs, drops the libraries into the
# app, and builds a signed, installable APK. It downloads the Android SDK, NDK
# and Gradle on first run (about 4 GB) and reuses them afterwards.
#
# The result lands in dist\bifrost-<version>-android.apk.

[CmdletBinding()]
param(
    # Where the Android SDK lives, or should be installed.
    [string]$SdkRoot = "$env:LOCALAPPDATA\Android\Sdk",
    # Where Gradle lives, or should be installed.
    [string]$GradleRoot = "$env:LOCALAPPDATA\gradle",
    # 'release' (smaller, signed with the debug key) or 'debug'.
    [ValidateSet('release', 'debug')]
    [string]$Variant = 'release',
    # Skip the Rust step when only the Kotlin has changed.
    [switch]$SkipNative
)

$ErrorActionPreference = 'Stop'

$NdkVersion = '26.1.10909125'
$GradleVersion = '8.11.1'
$BuildTools = '34.0.0'
$Platform = 'android-34'
# Must match minSdk in app/build.gradle.kts.
$ApiLevel = 24

function Step($t) { Write-Host "==> $t" -ForegroundColor White }
function Ok($t)   { Write-Host "  [ok] $t" -ForegroundColor Green }
function Warn($t) { Write-Host "  [!]  $t" -ForegroundColor Yellow }
function Fail($t) { Write-Host "  [x]  $t" -ForegroundColor Red; exit 1 }

# Run a native tool without PowerShell turning its stderr into a terminating
# error. Exit code is the only reliable success signal for these.
function Invoke-Native {
    param([string]$Command, [string[]]$Arguments = @())
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Command @Arguments; return $LASTEXITCODE }
    finally { $ErrorActionPreference = $previous }
}

$root = Split-Path -Parent $PSScriptRoot
$android = Join-Path $root 'clients\android'

Write-Host ''
Write-Host '  Building Bifrost for Android' -ForegroundColor White
Write-Host ''

# --- 1. JDK -----------------------------------------------------------------
Step 'Checking the JDK'

if (-not (Get-Command javac -ErrorAction SilentlyContinue)) {
    Fail 'No JDK found. Install JDK 17 or newer, e.g. from https://adoptium.net'
}
$javaHome = $env:JAVA_HOME
if (-not $javaHome) {
    # Derive it from javac's location: Gradle needs JAVA_HOME set explicitly.
    $javacPath = (Get-Command javac).Source
    $javaHome = Split-Path -Parent (Split-Path -Parent $javacPath)
    $env:JAVA_HOME = $javaHome
}
Ok "JAVA_HOME = $javaHome"

# --- 2. Android SDK ---------------------------------------------------------
Step 'Checking the Android SDK'

$sdkmanager = Join-Path $SdkRoot 'cmdline-tools\latest\bin\sdkmanager.bat'
if (-not (Test-Path $sdkmanager)) {
    Warn 'Command-line tools not found; downloading (about 150 MB)'
    New-Item -ItemType Directory -Force -Path $SdkRoot | Out-Null
    $zip = Join-Path $env:TEMP 'android-cmdline-tools.zip'
    Invoke-WebRequest -UseBasicParsing `
        -Uri 'https://dl.google.com/android/repository/commandlinetools-win-11076708_latest.zip' `
        -OutFile $zip

    $staging = Join-Path $SdkRoot 'tmp'
    Expand-Archive -Path $zip -DestinationPath $staging -Force
    New-Item -ItemType Directory -Force -Path (Join-Path $SdkRoot 'cmdline-tools') | Out-Null
    $target = Join-Path $SdkRoot 'cmdline-tools\latest'
    if (Test-Path $target) { Remove-Item -Recurse -Force $target }
    # sdkmanager insists on living at cmdline-tools/latest/bin.
    Move-Item (Join-Path $staging 'cmdline-tools') $target
    Remove-Item -Recurse -Force $staging, $zip -ErrorAction SilentlyContinue
    Ok 'Command-line tools installed'
}

# Accepting the licences needs an interactive terminal, which a script does not
# have. Writing the hashes is exactly what answering "y" does, and is the
# documented way to do it unattended.
$licenceDir = Join-Path $SdkRoot 'licenses'
if (-not (Test-Path (Join-Path $licenceDir 'android-sdk-license'))) {
    New-Item -ItemType Directory -Force -Path $licenceDir | Out-Null
    @{
        'android-sdk-license' = @(
            '24333f8a63b6825ea9c5514f83c2829b004d1fee',
            '8933bad161af4178b1185d1a37fbf41ea5269c55',
            'd56f5187479451eabf01fb78af6dfcb131a6481e')
        'android-sdk-preview-license'   = @('84831b9409646a918e30573bab4c9c91346d8abd')
        'android-sdk-arm-dbt-license'   = @('859f317696f67ef3d7f30a50a5560e7834b43903')
        'intel-android-extra-license'   = @('d975f751698a77b662f1254ddbeed3901e976f5a')
    }.GetEnumerator() | ForEach-Object {
        [IO.File]::WriteAllText((Join-Path $licenceDir $_.Key), "`n" + ($_.Value -join "`n") + "`n")
    }
    Ok 'Licences accepted'
}

$needed = @()
if (-not (Test-Path (Join-Path $SdkRoot "platforms\$Platform")))   { $needed += "platforms;$Platform" }
if (-not (Test-Path (Join-Path $SdkRoot "build-tools\$BuildTools"))) { $needed += "build-tools;$BuildTools" }
if (-not (Test-Path (Join-Path $SdkRoot "ndk\$NdkVersion")))       { $needed += "ndk;$NdkVersion" }
if (-not (Test-Path (Join-Path $SdkRoot 'platform-tools')))        { $needed += 'platform-tools' }

if ($needed.Count -gt 0) {
    Warn "Downloading: $($needed -join ', ') (up to 3 GB, once)"
    $code = Invoke-Native $sdkmanager (@("--sdk_root=$SdkRoot") + $needed)
    if ($code -ne 0) { Fail 'sdkmanager failed' }
}
Ok "SDK ready at $SdkRoot"

$env:ANDROID_HOME = $SdkRoot
$ndk = Join-Path $SdkRoot "ndk\$NdkVersion"
$ndkBin = Join-Path $ndk 'toolchains\llvm\prebuilt\windows-x86_64\bin'
if (-not (Test-Path $ndkBin)) { Fail "NDK toolchain missing at $ndkBin" }

# Java properties treat a backslash as an escape, so the path must use forward
# slashes or the SDK location silently parses to nonsense.
$sdkForProperties = $SdkRoot -replace '\\', '/'
Set-Content (Join-Path $android 'local.properties') "sdk.dir=$sdkForProperties" -Encoding ascii

# --- 3. Rust ----------------------------------------------------------------
if (-not $SkipNative) {
    Step 'Cross-compiling the Rust core'

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Fail 'Rust not found. Install it from https://rustup.rs'
    }

    $abis = @(
        @{ Abi = 'arm64-v8a';   Target = 'aarch64-linux-android';   Clang = "aarch64-linux-android$ApiLevel-clang.cmd" }
        @{ Abi = 'armeabi-v7a'; Target = 'armv7-linux-androideabi'; Clang = "armv7a-linux-androideabi$ApiLevel-clang.cmd" }
    )

    foreach ($abi in $abis) {
        rustup target add $abi.Target 2>&1 | Out-Null

        # Cargo reads the linker from a variable named after the target,
        # upper-cased with hyphens replaced by underscores.
        $suffix = $abi.Target.ToUpper().Replace('-', '_')
        $clang = Join-Path $ndkBin $abi.Clang
        if (-not (Test-Path $clang)) { Fail "NDK clang missing: $clang" }

        Set-Item "env:CARGO_TARGET_${suffix}_LINKER" $clang
        Set-Item "env:CC_$($abi.Target.Replace('-', '_'))" $clang
        Set-Item "env:AR_$($abi.Target.Replace('-', '_'))" (Join-Path $ndkBin 'llvm-ar.exe')

        Push-Location $root
        try {
            $code = Invoke-Native 'cargo' @('build', '--release', '--target', $abi.Target, '-p', 'bifrost-core')
            if ($code -ne 0) { Fail "Rust build failed for $($abi.Abi)" }
        } finally { Pop-Location }

        $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
        $built = Join-Path $targetDir "$($abi.Target)\release\libbifrost_core.so"
        if (-not (Test-Path $built)) { Fail "No library produced at $built" }

        $dest = Join-Path $android "app\src\main\jniLibs\$($abi.Abi)"
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Copy-Item $built $dest -Force
        Ok "$($abi.Abi): $([math]::Round((Get-Item $built).Length/1MB, 1)) MB"
    }
} else {
    Warn 'Skipping the native build'
}

# --- 4. Gradle --------------------------------------------------------------
Step 'Checking Gradle'

$gradle = Join-Path $GradleRoot "gradle-$GradleVersion\bin\gradle.bat"
if (-not (Test-Path $gradle)) {
    Warn "Downloading Gradle $GradleVersion (about 130 MB)"
    New-Item -ItemType Directory -Force -Path $GradleRoot | Out-Null
    $zip = Join-Path $env:TEMP "gradle-$GradleVersion.zip"
    Invoke-WebRequest -UseBasicParsing `
        -Uri "https://services.gradle.org/distributions/gradle-$GradleVersion-bin.zip" `
        -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath $GradleRoot -Force
    Remove-Item $zip -ErrorAction SilentlyContinue
}
Ok "Gradle $GradleVersion"

# --- 5. Build ---------------------------------------------------------------
Step "Building the $Variant APK"

$task = if ($Variant -eq 'release') { 'assembleRelease' } else { 'assembleDebug' }
Push-Location $android
try {
    $code = Invoke-Native $gradle @($task, '--no-daemon')
    if ($code -ne 0) { Fail 'Gradle build failed' }
} finally { Pop-Location }

$apk = Join-Path $android "app\build\outputs\apk\$Variant\app-$Variant.apk"
if (-not (Test-Path $apk)) { Fail "No APK produced at $apk" }

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"(.+)"' |
            Select-Object -First 1).Matches.Groups[1].Value
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$final = Join-Path $dist "bifrost-$version-android.apk"
Copy-Item $apk $final -Force

Write-Host ''
Write-Host ("  Built {0} ({1:N1} MB)" -f $final, ((Get-Item $final).Length/1MB)) -ForegroundColor Green
Write-Host ''
Write-Host '  Install it over USB:'
Write-Host "      $SdkRoot\platform-tools\adb.exe install -r `"$final`""
Write-Host ''
Write-Host '  Or copy it to the phone and open it. Android will ask you to allow'
Write-Host '  installing from this source; that is expected for a sideloaded app.'
Write-Host ''
