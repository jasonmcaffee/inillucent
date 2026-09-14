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

        on the MacBook:  ./packaging/macos/release-macos.sh --version 0.1.1 --upload
        here:            pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.1

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

.PARAMETER AllowUnverifiedSignatures
    Accept the artifacts without reading their signatures, for a machine that
    does not have rcodesign. The signature check is the only thing standing
    between an unsigned Mach-O and the site, so this is a deliberate act rather
    than a warning that scrolls past.

.EXAMPLE
    pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.1
    pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.1 -FromDirectory D:\from-macbook
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Version,
    [string] $FromDirectory,
    [string] $Repository = 'Black-Rainbow-Labs/Inillucent',
    [switch] $AllowUnverifiedSignatures
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
$checked = @()
foreach ($line in Get-Content -Path $sumsFile) {
    if ($line -notmatch '^([0-9a-fA-F]{64})\s+(.+)$') { continue }
    $claimed = $Matches[1].ToLower()
    $checked += $Matches[2].Trim()
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

# An artifact the checksum file does not mention has not been checked, and a
# loop over an empty file reports nothing and reports no failure either. The
# Mac writes SHA256SUMS-macos by appending three `shasum` lines to a file it has
# just truncated, so a shasum that is not on its PATH leaves an empty file, this
# loop runs zero times, and three unverified artifacts go on to be published
# with every check reported as passed. A check that cannot fail is worse than no
# check.
foreach ($artifact in @("$name.tar.gz", "$name.zip", "inillucent-$Version.pkg")) {
    if ($checked -notcontains $artifact) {
        Write-Host "  FAIL  $artifact is not named in SHA256SUMS-macos, so nothing checked it"
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

    # Two different tars answer to `tar` on Windows and they disagree about this
    # one option. GNU tar, which Git for Windows puts on PATH, reads the leading
    # `C:` of an absolute path as `host:path` and needs --force-local to be told
    # otherwise. bsdtar, which Windows itself ships as System32\tar.exe, needs no
    # such thing and rejects the option outright with a usage dump - so which of
    # the two is found first decided whether this step worked. Asked rather than
    # assumed.
    $tarArguments = @('--directory', $unpacked, '-xzf', (Join-Path $dist "$name.tar.gz"))
    & tar --force-local --version *> $null
    if ($LASTEXITCODE -eq 0) { $tarArguments = @('--force-local') + $tarArguments }
    & tar @tarArguments
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
} elseif ($AllowUnverifiedSignatures) {
    Write-Warning 'rcodesign is missing, so the signatures were NOT checked. -AllowUnverifiedSignatures was passed.'
} else {
    # A warning and a pass is the same outcome as a pass, and this is the only
    # check standing between an unsigned or self-signed Mach-O and the site.
    throw 'rcodesign is missing, so the signatures cannot be checked. ' +
          'Run: pwsh tools/cross/fetch-toolchain.ps1, or pass -AllowUnverifiedSignatures.'
}

Write-Host ''
if ($failures -gt 0) {
    throw "$failures check(s) failed. These artifacts must not be published."
}

# The checksums go in only after the gate above.
#
# This used to run before it, so a run that ended with "these artifacts must not
# be published" had already written their hashes into dist/SHA256SUMS - which is
# the file packaging/publish-site.ps1 copies to the site to say what a release
# contains. An artifact rejected here for being unsigned, self-signed, built
# without the hardened runtime, untimestamped or not universal was listed as
# published anyway, by the same run that refused it.
$sums = Update-Sha256Sums -Dist $dist

Write-Host "the macOS artifacts are in $dist and every check passed"
Write-Host "sums $sums"
