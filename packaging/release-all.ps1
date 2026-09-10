<#
.SYNOPSIS
    Builds every release target on this Windows machine.

.DESCRIPTION
    Four targets, one box, no Mac and no Linux machine:

        x86_64-pc-windows-msvc          cargo, natively
        x86_64-unknown-linux-gnu.2.28   cargo-zigbuild
        aarch64-unknown-linux-gnu.2.28  cargo-zigbuild
        aarch64-apple-darwin            cargo-zigbuild, no Apple SDK
        x86_64-apple-darwin             cargo-zigbuild, no Apple SDK

    The `.2.28` suffix on the Linux triples is a cargo-zigbuild feature: it is
    the oldest glibc the result will start against. 2.28 is Debian 10, Ubuntu
    18.10, RHEL 8 and Amazon Linux 2023. Building on this machine's WSL instead
    would produce a 2.39 floor, which refuses to start on Debian 12 or RHEL 9.

    The two Apple targets are built but not archived here. They are joined into
    universal binaries and signed by packaging/macos/sign-macos.ps1, because an
    archive of unsigned Mach-O is not something anybody should be able to pick
    up by accident.

.PARAMETER Version
    Overrides the version taken from the workspace manifest.

.PARAMETER Targets
    all (the default), windows, linux, or macos.

.PARAMETER SkipBuild
    Stage from what is already built. For iterating on the packaging itself.

.EXAMPLE
    pwsh packaging/release-all.ps1
    pwsh packaging/release-all.ps1 -Targets linux
#>
[CmdletBinding()]
param(
    [string] $Version,
    [ValidateSet('all', 'windows', 'linux', 'macos')]
    [string] $Targets = 'all',
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'stage-layout.ps1')

$crossBin = Join-Path $root 'tools/cross/bin'
$zigDir = Join-Path $crossBin 'zig'
$cargoZigbuild = Join-Path $crossBin 'cargo-zigbuild.exe'

# The programs the release ships, and the only ones built.
$packages = @('-p', 'inillucent-cli', '-p', 'inillucent-migrate', '-p', 'inillucent-driver-capi')

# Reserving header space for a signature load command. A Mach-O linked by zig
# for x86-64 has neither an LC_CODE_SIGNATURE nor room to add one, and rcodesign
# then refuses it with "insufficient room to write code signature load command".
# The arm64 binary is unaffected because Apple silicon requires a signature and
# zig writes an ad-hoc one. Applied per target rather than through RUSTFLAGS so
# that switching targets does not invalidate the host build.
$appleLinkArgs = 'target.{0}.rustflags=["-C","link-arg=-Wl,-headerpad_max_install_names"]'

function Invoke-CargoZigbuild {
    <#
    .SYNOPSIS
        Cross compiles one target with zig as the linker.

    .PARAMETER Target
        The triple, with a glibc suffix for Linux.

    .PARAMETER ConfigArgs
        Extra `--config` arguments, used for the Apple header padding.
    #>
    param([string] $Target, [string[]] $ConfigArgs = @())

    if (-not (Test-Path -LiteralPath $cargoZigbuild)) {
        throw "cargo-zigbuild is missing. Run: pwsh tools/cross/fetch-toolchain.ps1"
    }
    $previousPath = $env:PATH
    $env:PATH = "$zigDir;$env:PATH"
    try {
        & $cargoZigbuild zigbuild --manifest-path (Join-Path $root 'Cargo.toml') `
            --release --locked --target $Target @ConfigArgs @packages
        if ($LASTEXITCODE -ne 0) { throw "cargo-zigbuild failed for $Target with $LASTEXITCODE" }
    } finally {
        $env:PATH = $previousPath
    }
}

function Invoke-CargoNative {
    <#
    .SYNOPSIS
        Builds the host target with cargo, no cross toolchain involved.

    .PARAMETER Target
        The triple.
    #>
    param([string] $Target)
    & cargo build --manifest-path (Join-Path $root 'Cargo.toml') --release --locked --target $Target @packages
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $Target with $LASTEXITCODE" }
}

if (-not $Version) { $Version = Get-WorkspaceVersion -Root $root }
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$wantWindows = $Targets -in @('all', 'windows')
$wantLinux = $Targets -in @('all', 'linux')
$wantMacos = $Targets -in @('all', 'macos')

Write-Host "inillucent $Version"

if ($wantWindows) {
    $target = 'x86_64-pc-windows-msvc'
    Write-Host "== $target"
    if (-not $SkipBuild) { Invoke-CargoNative -Target $target }
    $stage = New-InillucentStage -Root $root -Version $Version -Target $target `
        -BuiltDir (Join-Path $root "target/$target/release") -Dist $dist
    $archive = New-InillucentZip -Stage $stage -Dist $dist
    Write-Host "   $archive"
}

if ($wantLinux) {
    foreach ($pair in @(
        @{ Triple = 'x86_64-unknown-linux-gnu'; Build = 'x86_64-unknown-linux-gnu.2.28' },
        @{ Triple = 'aarch64-unknown-linux-gnu'; Build = 'aarch64-unknown-linux-gnu.2.28' }
    )) {
        Write-Host "== $($pair.Build)"
        if (-not $SkipBuild) { Invoke-CargoZigbuild -Target $pair.Build }
        $stage = New-InillucentStage -Root $root -Version $Version -Target $pair.Triple `
            -BuiltDir (Join-Path $root "target/$($pair.Triple)/release") -Dist $dist
        $archive = New-InillucentTarGz -Stage $stage -Dist $dist
        Write-Host "   $archive"
    }
}

if ($wantMacos) {
    foreach ($target in @('aarch64-apple-darwin', 'x86_64-apple-darwin')) {
        Write-Host "== $target"
        if (-not $SkipBuild) {
            Invoke-CargoZigbuild -Target $target -ConfigArgs @('--config', ($appleLinkArgs -f $target))
        }
        Write-Host "   built, unsigned: target/$target/release"
    }
    Write-Host '   sign them with: pwsh packaging/macos/sign-macos.ps1'
}

$sums = Update-Sha256Sums -Dist $dist
Write-Host ''
Write-Host "sums $sums"
Get-Content -Path $sums
