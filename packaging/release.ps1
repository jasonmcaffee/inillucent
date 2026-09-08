<#
.SYNOPSIS
    Builds the release archive every installer and every package is made from.

.DESCRIPTION
    One layout, one set of binaries, six ecosystems downstream. That is the
    whole point: npm, PyPI, Homebrew, the Windows installer and the Go installer
    are five different ways of getting *this* directory onto a machine, and if
    each built its own there would be five things that could differ.

    It produces, under dist/:

        inillucent-<version>-<target>/          the staged layout
            bin/inillucent(.exe)                the verb-shaped CLI
            bin/inillucent-shell(.exe)          the sqlite3-shaped shell
            bin/inillucent-mcp(.exe)            the MCP server
            bin/inillucent-migrate(.exe)        the SQLite importer
            lib/inillucent_driver_capi.dll      the C ABI, for every binding
            include/inillucent_driver.h         the header a binding compiles against
            README.md  LICENSE  VERSION
        inillucent-<version>-<target>.zip       the archive
        SHA256SUMS                              over the archive

    The build is --release --locked, because a release built from a resolved
    lockfile is a release somebody else can reproduce.

.PARAMETER Version
    Overrides the version taken from the workspace manifest.

.PARAMETER Target
    The Rust target triple. Defaults to the host's.

.PARAMETER SkipBuild
    Stage from whatever is already in target/release. For iterating on the
    packaging itself; never for a real release.

.EXAMPLE
    pwsh packaging/release.ps1
    pwsh packaging/release.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $Target,
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Get-WorkspaceVersion {
    <#
    .SYNOPSIS
        Reads the version out of the workspace manifest's [workspace.package].
    #>
    $manifest = Get-Content -Path (Join-Path $root 'Cargo.toml') -Raw
    if ($manifest -match '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw 'Cargo.toml does not declare [workspace.package] version'
}

function Get-HostTarget {
    <#
    .SYNOPSIS
        Asks rustc what it builds for by default.
    #>
    $line = (& rustc -vV) | Where-Object { $_ -like 'host:*' }
    return ($line -split ':\s*')[1].Trim()
}

function Copy-Artifact {
    <#
    .SYNOPSIS
        Copies one built file into the staging layout, failing loudly if absent.

    .PARAMETER From
        The built file.

    .PARAMETER Into
        The directory to place it in.
    #>
    param([string] $From, [string] $Into)
    if (-not (Test-Path -LiteralPath $From)) {
        throw "the build did not produce $From"
    }
    New-Item -ItemType Directory -Force -Path $Into | Out-Null
    Copy-Item -LiteralPath $From -Destination $Into -Force
}

if (-not $Version) { $Version = Get-WorkspaceVersion }
if (-not $Target) { $Target = Get-HostTarget }
$exe = if ($IsWindows -or $env:OS -eq 'Windows_NT') { '.exe' } else { '' }

Write-Host "inillucent $Version for $Target"

if (-not $SkipBuild) {
    Write-Host 'building (release, locked)...'
    & cargo build --manifest-path (Join-Path $root 'Cargo.toml') --release --locked `
        -p inillucent-cli -p inillucent-migrate -p inillucent-driver-capi
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with $LASTEXITCODE" }
}

$built = Join-Path $root 'target/release'
$dist = Join-Path $root 'dist'
$name = "inillucent-$Version-$Target"
$stage = Join-Path $dist $name

if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

foreach ($program in @('inillucent', 'inillucent-shell', 'inillucent-mcp', 'inillucent-migrate')) {
    Copy-Artifact -From (Join-Path $built "$program$exe") -Into (Join-Path $stage 'bin')
}

# The C ABI, which is how every language that is not Rust reaches the engine.
# Its file name is the platform's, so each is tried and the one that exists is
# the one this platform builds.
$libraries = @('inillucent_driver_capi.dll', 'libinillucent_driver_capi.so', 'libinillucent_driver_capi.dylib')
$found = $false
foreach ($library in $libraries) {
    $candidate = Join-Path $built $library
    if (Test-Path -LiteralPath $candidate) {
        Copy-Artifact -From $candidate -Into (Join-Path $stage 'lib')
        $found = $true
    }
}
if (-not $found) { throw 'the build produced no C ABI shared library' }

Copy-Artifact -From (Join-Path $root 'drivers/inillucent-driver-capi/include/inillucent_driver.h') `
    -Into (Join-Path $stage 'include')
Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $stage -Force
Copy-Item -LiteralPath (Join-Path $root 'drivers/README.md') -Destination (Join-Path $stage 'DRIVER.md') -Force
Set-Content -Path (Join-Path $stage 'VERSION') -Value $Version -NoNewline

# The licence has to travel with the binaries: MIT requires the notice to
# accompany every copy, and an archive without one is not a distributable one.
$license = Join-Path $root 'LICENSE'
if (Test-Path -LiteralPath $license) {
    Copy-Item -LiteralPath $license -Destination $stage -Force
} else {
    Write-Warning 'LICENSE is missing from the repository root; the archive will not carry one'
}

$archive = Join-Path $dist "$name.zip"
if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
Compress-Archive -Path $stage -DestinationPath $archive -CompressionLevel Optimal

# One SHA256SUMS for the whole dist directory, rewritten each time, so an
# installer can verify what it downloaded against one file.
$sums = Join-Path $dist 'SHA256SUMS'
$lines = Get-ChildItem -Path $dist -Filter '*.zip' | ForEach-Object {
    "$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
}
Get-ChildItem -Path $dist -Filter '*.tar.gz' -ErrorAction SilentlyContinue | ForEach-Object {
    $lines += "$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
}
Set-Content -Path $sums -Value $lines

Write-Host ''
Write-Host "staged  $stage"
Write-Host "archive $archive"
Write-Host "sums    $sums"
Get-Content -Path $sums
