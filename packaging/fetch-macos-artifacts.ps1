<#
.SYNOPSIS
    Brings the macOS artifacts built on the MacBook to this machine, and refuses
    them if they are not what they claim to be.

.DESCRIPTION
    The MacBook builds, signs and notarises macOS; this machine builds Windows
    and Linux and publishes the site. That leaves one problem, which is getting
    three files from one to the other.

    The transport is a GitHub release on the repository that already exists.
    Nothing new has to be installed or opened: no inbound port on this machine,
    no shared folder, no third party, and it works from anywhere the MacBook has
    a network. The repository is private, so the assets are private too, which is
    correct - inillucent.com is the public distribution point, not GitHub.

        on the MacBook:  ./packaging/macos/release-macos.sh --version 0.1.0 --upload
        here:            pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.0

    -FromDirectory skips GitHub entirely, for when the two machines are on the
    same network and a copy is simpler than a round trip.

    Whatever the transport, the artifacts are verified here before they are
    allowed anywhere near the site: the checksums the Mac wrote, and then the
    signature itself, read out of the Mach-O by rcodesign. A file that arrives
    unsigned, unstamped, or signed by something that does not chain to an Apple
    root is rejected here rather than by a reader's Gatekeeper.

.PARAMETER Version
    The release version. Required.

.PARAMETER FromDirectory
    Take the files from a directory instead of a GitHub release.

.PARAMETER Repository
    owner/name of the GitHub repository holding the release.

.EXAMPLE
    pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.0
    pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.0 -FromDirectory D:\from-macbook
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Version,
    [string] $FromDirectory,
    [string] $Repository = 'Black-Rainbow-Labs/Inillucent'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'stage-layout.ps1')

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$name = "inillucent-$Version-universal-apple-darwin"
$expected = @("$name.tar.gz", "$name.zip", "inillucent-$Version.pkg", 'SHA256SUMS-macos')

function Get-FromGitHub {
    <#
    .SYNOPSIS
        Downloads the release assets the MacBook uploaded.
    #>
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        throw 'gh is not installed. Install GitHub CLI, or use -FromDirectory.'
    }
    & gh auth status 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'gh is not signed in on this machine. Run: gh auth login'
    }
    foreach ($asset in $expected) {
        Write-Host "  downloading $asset"
        & gh release download "v$Version" --repo $Repository --pattern $asset --dir $dist --clobber
        if ($LASTEXITCODE -ne 0) { throw "gh release download failed for $asset" }
    }
}

function Get-FromDirectory {
    <#
    .SYNOPSIS
        Copies the artifacts from a directory the MacBook wrote to.

    .PARAMETER Source
        The directory holding them.
    #>
    param([string] $Source)
    foreach ($asset in $expected) {
        $from = Join-Path $Source $asset
        if (-not (Test-Path -LiteralPath $from)) { throw "$from does not exist" }
        Copy-Item -LiteralPath $from -Destination $dist -Force
        Write-Host "  copied $asset"
    }
}

Write-Host "inillucent $Version, macOS artifacts"
if ($FromDirectory) { Get-FromDirectory -Source $FromDirectory } else { Get-FromGitHub }

# --- the checksums the Mac wrote -------------------------------------------

Write-Host ''
Write-Host 'verifying checksums'
$sumsFile = Join-Path $dist 'SHA256SUMS-macos'
$failures = 0
foreach ($line in Get-Content -Path $sumsFile) {
    if ($line -notmatch '^([0-9a-fA-F]{64})\s+(.+)$') { continue }
    $claimed = $Matches[1].ToLower()
    $file = Join-Path $dist $Matches[2].Trim()
    if (-not (Test-Path -LiteralPath $file)) {
        Write-Host "  FAIL  $($Matches[2]) is listed but was not delivered"
        $failures++
        continue
    }
    $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLower()
    if ($actual -eq $claimed) {
        Write-Host "  ok    $($Matches[2])"
    } else {
        Write-Host "  FAIL  $($Matches[2]) is $actual, the Mac said $claimed"
        $failures++
    }
}

# --- the signature, read out of the binaries themselves ---------------------

$rcodesign = Join-Path $root 'tools/cross/bin/rcodesign.exe'
if (Test-Path -LiteralPath $rcodesign) {
    Write-Host ''
    Write-Host 'verifying the signatures'
    $unpacked = Join-Path $dist '_macos-check'
    if (Test-Path -LiteralPath $unpacked) { Remove-Item -LiteralPath $unpacked -Recurse -Force -Confirm:$false }
    New-Item -ItemType Directory -Force -Path $unpacked | Out-Null
    & tar --force-local --directory $unpacked -xzf (Join-Path $dist "$name.tar.gz")
    if ($LASTEXITCODE -ne 0) { throw 'the tarball could not be unpacked' }

    foreach ($program in @('inillucent', 'inillucent-shell', 'inillucent-mcp', 'inillucent-migrate')) {
        $file = Join-Path $unpacked "$name/bin/$program"
        $info = & $rcodesign print-signature-info $file 2>&1 | Out-String

        # Four separate claims, each of which has its own way of being wrong: an
        # unsigned binary, a self-signed one, one without the hardened runtime,
        # and one whose signature dies with the certificate because nobody
        # timestamped it.
        $checks = @{
            'chains to an Apple root' = ($info -match 'chains_to_apple_root_ca:\s*true')
            'is a Developer ID certificate' = ($info -match 'apple_certificate_profile:\s*developer-id-application')
            'has the hardened runtime' = ($info -match 'CodeSignatureFlags\(RUNTIME\)')
            'carries a timestamp' = ($info -match 'time_stamp_token')
            'is universal' = (($info -match 'macho-index:0') -and ($info -match 'macho-index:1'))
        }
        foreach ($claim in $checks.Keys | Sort-Object) {
            if ($checks[$claim]) {
                Write-Host "  ok    $program $claim"
            } else {
                Write-Host "  FAIL  $program $claim"
                $failures++
            }
        }
    }
    Remove-Item -LiteralPath $unpacked -Recurse -Force -Confirm:$false
} else {
    Write-Warning 'rcodesign is missing, so the signatures were not checked. Run: pwsh tools/cross/fetch-toolchain.ps1'
}

$sums = Update-Sha256Sums -Dist $dist

Write-Host ''
if ($failures -gt 0) {
    throw "$failures check(s) failed. These artifacts must not be published."
}
Write-Host "the macOS artifacts are in $dist and every check passed"
Write-Host "sums $sums"
