<#
.SYNOPSIS
    The whole macOS release, on the Windows machine, in one command.

.DESCRIPTION
    packaging/macos/release-macos.sh does this on a Mac and still works there.
    This is the same release without one (task-1995): it builds both Apple
    architectures with zig, joins them into universal binaries, signs them with
    the Developer ID, writes the two archives and the .pkg, notarises them, and
    staples the ticket to the .pkg.

        dist/inillucent-<version>-universal-apple-darwin.tar.gz   install.sh, Homebrew
        dist/inillucent-<version>-universal-apple-darwin.zip      the notary submission
        dist/inillucent-<version>.pkg                             the double-click download

    WHAT REPLACED EACH MACOS PROGRAM

        lipo        -> rcodesign macho-universal-create
        codesign    -> rcodesign sign
        pkgbuild    -> tools/macos-pkg
        productbuild-> tools/macos-pkg, which writes the product archive directly
        productsign -> rcodesign sign, which signs a XAR archive
        notarytool  -> rcodesign notary-submit
        stapler     -> rcodesign staple

    WHAT DID NOT GET REPLACED, AND IS NOT PRETENDED TO BE

    A Mach-O can only be executed on macOS. packaging/macos/verify-macos.sh has
    seven checks and four of them run the binary; those four cannot run here and
    the summary at the end of this script says so every time, so that a release
    cut on Windows is never mistaken for one that went through the Mac gate.
    What stands in for them is Apple's notary service, which unpacks the
    submission, walks every Mach-O in it, and rejects an unsigned binary, a
    missing hardened runtime, a missing secure timestamp, an SDK that is too old
    or a package it cannot parse.

    CREDENTIALS

    packaging/macos/apple-credentials.ps1 finds them, and
    packaging/macos/new-apple-csr.ps1 obtains them without a Mac. Nothing secret
    is in this repository, on a command line, or in an environment variable.

.PARAMETER Version
    The release version. Defaults to the workspace version.

.PARAMETER SkipBuild
    Sign and package what is already built.

.PARAMETER SkipNotarize
    Stop after signing and packaging. Everything is still signed; nothing is
    submitted to Apple, so nothing is publishable.

.PARAMETER SelfSigned
    Sign with a certificate generated on the spot. Proves every step except the
    two that are about Apple's opinion of the certificate, and refuses to leave
    the artifacts in dist/ unless -Unpublishable says that is wanted.

.PARAMETER Unpublishable
    Keep the artifacts a -SelfSigned run produced, named so they cannot be
    mistaken for a release.

.EXAMPLE
    pwsh packaging/macos/release-macos.ps1
    pwsh packaging/macos/release-macos.ps1 -SelfSigned -SkipNotarize -Unpublishable
#>
[CmdletBinding()]
param(
    [string] $Version,
    [switch] $SkipBuild,
    [switch] $SkipNotarize,
    [switch] $SelfSigned,
    [switch] $Unpublishable,
    [string] $Identifier = 'com.blackrainbowlabs.inillucent'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
. (Join-Path (Split-Path -Parent $PSScriptRoot) 'stage-layout.ps1')
. (Join-Path $PSScriptRoot 'apple-credentials.ps1')

$crossBin = Get-CrossBin -Root $root
$rcodesign = Join-Path $crossBin 'rcodesign.exe'
$cargoZigbuild = Join-Path $crossBin 'cargo-zigbuild.exe'
$zigDir = Join-Path $crossBin 'zig'

# The five files a macOS release carries, and the identifier each is signed
# under. codesign on a Mac derives the identifier from the file name, so these
# are the names it would have chosen; matching them keeps an artifact built here
# interchangeable with one built there.
$signables = @(
    @{ File = 'inillucent'; Id = 'inillucent' },
    @{ File = 'inillucent-shell'; Id = 'inillucent-shell' },
    @{ File = 'inillucent-mcp'; Id = 'inillucent-mcp' },
    @{ File = 'inillucent-migrate'; Id = 'inillucent-migrate' },
    @{ File = 'libinillucent_driver_capi.dylib'; Id = 'libinillucent_driver_capi' }
)

# The oldest macOS these binaries run on, which is zig's default and not a
# choice anybody made here.
#
# **It cannot currently be changed, and that was measured rather than assumed.**
# zig sets the minimum through its own target string - `-target
# aarch64-macos.11.0-none` produces `Minimum OS: 11.0.0`, verified directly with
# `zig cc`. cargo-zigbuild builds that string as `{arch}-macos-none{suffix}`,
# which puts a version after the ABI, and zig rejects that form with
# `InvalidAbiVersion`. Passing a second `-target` through `-C link-arg` does not
# work either: `cargo-zigbuild zig cc` drops a duplicate `-target` on purpose,
# with a comment saying so. MACOSX_DEPLOYMENT_TARGET is not read anywhere in
# cargo-zigbuild, and `-mmacosx-version-min` is accepted and ignored by zig.
#
# So 13.0 is the floor until cargo-zigbuild can express an Apple deployment
# target. It excludes macOS 11 and 12, which matters for an Apple Silicon Mac
# that never moved past Big Sur or Monterey. No macOS release has been published
# before this one - `dist/SHA256SUMS` has never carried a darwin line - so this
# is a first floor rather than a floor that moved.
#
# It is asserted rather than trusted: a zig upgrade that changes the default
# fails the release instead of quietly shipping a different minimum.
$expectedMinimumOs = '13.0.0'

function Invoke-Rcodesign {
    <#
    .SYNOPSIS
        Runs rcodesign and throws if it failed, so no step can be skipped
        silently.

    .PARAMETER Arguments
        The whole command line.

    .PARAMETER Quiet
        Discard its output; used for the calls whose result is read afterwards.
    #>
    param([string[]] $Arguments, [switch] $Quiet)
    if ($Quiet) {
        & $rcodesign @Arguments *> $null
    } else {
        & $rcodesign @Arguments
    }
    if ($LASTEXITCODE -ne 0) { throw "rcodesign $($Arguments[0]) failed with $LASTEXITCODE" }
}

function Get-CargoTargetDir {
    <#
    .SYNOPSIS
        Where cargo puts its output, honouring CARGO_TARGET_DIR.

    .DESCRIPTION
        The macOS build is about 40 GB of intermediate objects across the two
        architectures, and this machine's C: drive has filled before. Reading
        the variable rather than assuming <root>/target is what lets a release
        put that on another drive.
    #>
    if ($env:CARGO_TARGET_DIR) { return $env:CARGO_TARGET_DIR }
    return (Join-Path $root 'target')
}

function Get-MacosPkgTargetDir {
    <#
    .SYNOPSIS
        Where the flat package writer is built.

    .DESCRIPTION
        Its own directory beside the workspace's, because tools/macos-pkg is a
        separate workspace: two workspaces sharing one target directory
        rebuild each other's dependencies every time the feature sets differ.
    #>
    return (Join-Path (Get-CargoTargetDir) 'macos-pkg')
}

function Build-AppleTarget {
    <#
    .SYNOPSIS
        Cross compiles one Apple architecture with zig as the linker.

    .DESCRIPTION
        -headerpad_max_install_names reserves room for the code signature load
        command. A Mach-O linked by zig for x86-64 has neither the command nor
        space to add one, and rcodesign then refuses it with "insufficient room
        to write code signature load command". The arm64 binary is unaffected
        because Apple Silicon requires a signature and zig writes an ad-hoc one.

    .PARAMETER Target
        The triple.
    #>
    param([string] $Target)

    if (-not (Test-Path -LiteralPath $cargoZigbuild)) {
        throw 'cargo-zigbuild is missing. Run: pwsh tools/cross/fetch-toolchain.ps1'
    }
    $previousPath = $env:PATH
    $env:PATH = "$zigDir;$env:PATH"
    try {
        & $cargoZigbuild zigbuild --manifest-path (Join-Path $root 'Cargo.toml') `
            --release --locked --target $Target `
            --config "target.$Target.rustflags=[`"-C`",`"link-arg=-Wl,-headerpad_max_install_names`"]" `
            --features inillucent-cli/embed `
            -p inillucent-cli -p inillucent-migrate -p inillucent-driver-capi
        if ($LASTEXITCODE -ne 0) { throw "cargo-zigbuild failed for $Target with $LASTEXITCODE" }
    } finally {
        $env:PATH = $previousPath
    }
}

function New-UniversalBinaries {
    <#
    .SYNOPSIS
        Joins each architecture's build into one universal binary and returns
        the directory holding them.

    .PARAMETER Dist
        Where the directory is created.
    #>
    param([string] $Dist)

    $targetDir = Get-CargoTargetDir
    $universal = Join-Path $Dist 'universal-apple-darwin'
    if (Test-Path -LiteralPath $universal) { Remove-Item -LiteralPath $universal -Recurse -Force -Confirm:$false }
    New-Item -ItemType Directory -Force -Path $universal | Out-Null

    foreach ($entry in $signables) {
        $arm = Join-Path $targetDir "aarch64-apple-darwin/release/$($entry.File)"
        $intel = Join-Path $targetDir "x86_64-apple-darwin/release/$($entry.File)"
        foreach ($input in @($arm, $intel)) {
            if (-not (Test-Path -LiteralPath $input)) { throw "the build did not produce $input" }
        }
        $output = Join-Path $universal $entry.File
        Invoke-Rcodesign -Quiet -Arguments @('macho-universal-create', '--output', $output, $arm, $intel)
    }
    return $universal
}

function Assert-UniversalBinary {
    <#
    .SYNOPSIS
        Fails unless a file carries both architectures.

    .DESCRIPTION
        A universal binary with one slice is what an interrupted build produces,
        and it installs and runs on the machine that made it, so nothing later
        in the release would notice.

    .PARAMETER Path
        The binary to read.
    #>
    param([string] $Path)
    $seen = @()
    foreach ($index in 0, 1) {
        $header = & $rcodesign extract macho-header --universal-index $index $Path 2>&1
        if ($LASTEXITCODE -ne 0) { throw "$Path has no slice at index $index" }
        $seen += ($header | Select-String -Pattern 'cputype: (\d+)').Matches.Groups[1].Value
    }
    # 16777228 is arm64 and 16777223 is x86-64, as Mach-O numbers them.
    foreach ($wanted in '16777228', '16777223') {
        if ($seen -notcontains $wanted) { throw "$Path is missing the cputype $wanted slice (found $($seen -join ', '))" }
    }
}

function Assert-MinimumOs {
    <#
    .SYNOPSIS
        Fails unless both slices declare the macOS version this release says it
        needs.

    .DESCRIPTION
        The minimum comes from zig's default rather than from a setting, so the
        only thing keeping it true is a check that reads it back. A zig upgrade
        that moves the default would otherwise ship a release whose stated floor
        and actual floor disagree.

    .PARAMETER Path
        The binary to read.
    #>
    param([string] $Path)
    foreach ($index in 0, 1) {
        $target = & $rcodesign extract macho-target --universal-index $index $Path 2>&1
        $found = ($target | Select-String -Pattern 'Minimum OS: (\S+)').Matches.Groups[1].Value
        if ($found -ne $expectedMinimumOs) {
            throw "$Path slice $index declares macOS $found; this release states $expectedMinimumOs. See the note beside `$expectedMinimumOs before changing either."
        }
    }
}

function Assert-Signature {
    <#
    .SYNOPSIS
        Fails unless every slice of a file is signed with the hardened runtime.

    .DESCRIPTION
        Reading the signature back is the check, not the exit code of the
        signing command: rcodesign signs each slice of a universal binary
        separately, so "it did not fail" and "both slices are signed" are
        different statements.

    .PARAMETER Path
        The binary to read.
    #>
    param([string] $Path)
    $info = & $rcodesign print-signature-info $Path 2>&1
    if ($LASTEXITCODE -ne 0) { throw "$Path has no readable signature" }
    $runtime = ($info | Select-String -Pattern 'CodeSignatureFlags\(RUNTIME\)').Count
    if ($runtime -lt 2) { throw "$Path has $runtime slices with the hardened runtime flag; both slices need it" }
    if (-not ($info | Select-String -Pattern 'time_stamp_token')) {
        throw "$Path carries no trusted timestamp, so its signature stops verifying when the certificate expires"
    }
}

function Set-AppleSignatures {
    <#
    .SYNOPSIS
        Signs the five universal binaries and checks each one afterwards.

    .PARAMETER Universal
        The directory holding them.

    .PARAMETER IdentityArguments
        The rcodesign arguments naming the signing identity.
    #>
    param([string] $Universal, [string[]] $IdentityArguments)

    foreach ($entry in $signables) {
        $path = Join-Path $Universal $entry.File
        Assert-UniversalBinary -Path $path
        Assert-MinimumOs -Path $path
        Invoke-Rcodesign -Quiet -Arguments (@('sign') + $IdentityArguments + @(
                '--binary-identifier', $entry.Id,
                '--code-signature-flags', 'runtime',
                $path))
        Assert-Signature -Path $path
        Write-Host "   signed $($entry.File)"
    }
}

function New-AppleInstaller {
    <#
    .SYNOPSIS
        Builds the .pkg from the binaries that were just signed, and signs it.

    .DESCRIPTION
        The payload is assembled from the signed binaries rather than signed
        afterwards. A package built from unsigned binaries and then signed
        passes a check on the package and fails on first run, which is the one
        ordering mistake in this whole sequence that a release would not catch.

    .PARAMETER Stage
        The staged archive directory, which already holds bin/, lib/ and include/.

    .PARAMETER Dist
        Where the package is written.

    .PARAMETER Version
        The release version.

    .PARAMETER IdentityArguments
        The rcodesign arguments naming the Developer ID Installer identity.
    #>
    param([string] $Stage, [string] $Dist, [string] $Version, [string[]] $IdentityArguments)

    $tool = Join-Path (Get-MacosPkgTargetDir) 'release/macos-pkg.exe'
    if (-not (Test-Path -LiteralPath $tool)) {
        throw "macos-pkg is missing at $tool. Run this without -SkipBuild, or: cargo build --manifest-path tools/macos-pkg/Cargo.toml --release"
    }

    $pkgroot = Join-Path $Dist 'pkgroot'
    if (Test-Path -LiteralPath $pkgroot) { Remove-Item -LiteralPath $pkgroot -Recurse -Force -Confirm:$false }
    foreach ($pair in @(@('bin', 'usr/local/bin'), @('lib', 'usr/local/lib'), @('include', 'usr/local/include'))) {
        $into = Join-Path $pkgroot $pair[1]
        New-Item -ItemType Directory -Force -Path $into | Out-Null
        Copy-Item -Path (Join-Path $Stage ($pair[0] + '/*')) -Destination $into -Force
    }

    $unsigned = Join-Path $Dist "inillucent-$Version-unsigned.pkg"
    $product = Join-Path $Dist "inillucent-$Version.pkg"
    foreach ($stale in @($unsigned, $product)) {
        if (Test-Path -LiteralPath $stale) { Remove-Item -LiteralPath $stale -Force -Confirm:$false }
    }

    & $tool --root $pkgroot --identifier $Identifier --version $Version --install-location / `
        --executable-dir usr/local/bin --executable-dir usr/local/lib `
        --distribution (Join-Path $PSScriptRoot 'Distribution.xml') `
        --resource (Join-Path $PSScriptRoot 'welcome.txt') `
        --resource (Join-Path $PSScriptRoot 'conclusion.txt') `
        --resource (Join-Path $root 'LICENSE') `
        --output $unsigned | Write-Host
    if ($LASTEXITCODE -ne 0) { throw "macos-pkg failed with $LASTEXITCODE" }

    # rcodesign is the independent reader of what macos-pkg wrote: it parses the
    # table of contents, rewrites every entry's offset and signs the result. A
    # package it cannot read fails here, on the machine that built it.
    Invoke-Rcodesign -Quiet -Arguments (@('sign') + $IdentityArguments + @($unsigned, $product))
    Remove-Item -LiteralPath $unsigned -Force -Confirm:$false
    Remove-Item -LiteralPath $pkgroot -Recurse -Force -Confirm:$false
    return $product
}

function Submit-ForNotarisation {
    <#
    .SYNOPSIS
        Uploads one artifact to Apple and waits for the answer.

    .DESCRIPTION
        The .pkg is stapled, so it installs on a machine with no network. The
        .zip is not, because no container outside .pkg, .dmg and .app can carry
        a ticket; submitting it still registers the cdhash of every binary
        inside it, which is what Gatekeeper looks up when somebody runs a
        program out of the tarball.

    .PARAMETER Path
        The artifact.

    .PARAMETER KeyFile
        The unsealed App Store Connect key, which the caller deletes afterwards.

    .PARAMETER Staple
        Whether to write the ticket into it afterwards.
    #>
    param([string] $Path, [string] $KeyFile, [switch] $Staple)

    $arguments = @('notary-submit', '--api-key-file', $KeyFile, '--wait')
    if ($Staple) { $arguments += '--staple' }
    $arguments += $Path
    Write-Host "   submitting $(Split-Path -Leaf $Path); Apple usually answers in two to fifteen minutes"
    Invoke-Rcodesign -Arguments $arguments
}

function New-SelfSignedIdentity {
    <#
    .SYNOPSIS
        Makes a throwaway certificate on the RAM disk, for proving the pipeline
        before the real certificates exist.

    .PARAMETER Kind
        `application` or `installer`.
    #>
    param([ValidateSet('application', 'installer')] [string] $Kind)

    $scratch = Get-AppleScratchDir
    $stamp = [guid]::NewGuid().ToString('N')
    $pem = Join-Path $scratch "selfsigned-$Kind-$stamp.pem"
    Invoke-Rcodesign -Quiet -Arguments @(
        'generate-self-signed-certificate', '--algorithm', 'rsa',
        '--profile', "developer-id-$Kind", '--team-id', 'SELFSIGNED',
        '--person-name', 'inillucent self-signed release proof',
        '--validity-days', '30', '--pem-unified-file', $pem)
    return [pscustomobject]@{ Arguments = @('--pem-file', $pem); Scratch = @($pem) }
}

# ---------------------------------------------------------------------------
# The release itself.
# ---------------------------------------------------------------------------

if (-not $Version) { $Version = Get-WorkspaceVersion -Root $root }

# **A version that is not a version stops here**, rather than naming every archive after it. A
# caller that splats an array instead of a hashtable passes the string `-Version` as the version
# itself, and the first sign of it was an archive called
# `inillucent--Version-universal-apple-darwin.tar.gz` that had already been built and signed.
if ($Version -notmatch '^\d+\.\d+\.\d+') {
    throw "'$Version' is not a version. A caller has passed a parameter name as the value - array splatting binds positionally, so @('-Version', `$v) sends the string '-Version'. Use a hashtable."
}
if ($SelfSigned -and -not $SkipNotarize) {
    throw 'a self-signed certificate cannot be notarised; add -SkipNotarize'
}
if ($SelfSigned -and -not $Unpublishable) {
    throw '-SelfSigned proves the pipeline and does not produce a release. Add -Unpublishable to keep what it builds.'
}

if (-not $SelfSigned) {
    $present = Test-AppleCredentials
    foreach ($needed in @('Application', 'Installer')) {
        if (-not $present.$needed) {
            throw "no Developer ID $needed certificate in $($present.Directory). Run: pwsh packaging/macos/new-apple-csr.ps1 -Kind $($needed.ToLower())"
        }
    }
    if (-not $SkipNotarize -and -not $present.NotaryKey) {
        throw "no notary key in $($present.Directory). packaging/macos/README.md says how to make one, or pass -SkipNotarize."
    }
}

# A self-signed run writes somewhere else entirely. The artifacts are bit for
# bit what a release looks like apart from who signed them, so leaving them in
# dist/ beside real ones is how an unsignable build gets published by accident.
$dist = if ($SelfSigned) { Join-Path $root 'dist/unpublishable' } else { Join-Path $root 'dist' }
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Write-Host "inillucent $Version - macOS, built and signed on Windows"

if (-not $SkipBuild) {
    foreach ($target in @('aarch64-apple-darwin', 'x86_64-apple-darwin')) {
        Write-Host "== $target"
        Build-AppleTarget -Target $target
    }
    Write-Host '== macos-pkg'
    & cargo build --manifest-path (Join-Path $root 'tools/macos-pkg/Cargo.toml') --release `
        --target-dir (Get-MacosPkgTargetDir)
    if ($LASTEXITCODE -ne 0) { throw "building macos-pkg failed with $LASTEXITCODE" }
}

Write-Host '== universal binaries'
$universal = New-UniversalBinaries -Dist $dist

$applicationIdentity = $null
$installerIdentity = $null
try {
    $applicationIdentity = if ($SelfSigned) { New-SelfSignedIdentity -Kind application } else { New-AppleSigningSession -Kind application }
    Write-Host '== signing'
    Set-AppleSignatures -Universal $universal -IdentityArguments $applicationIdentity.Arguments

    Write-Host '== archives'
    $stage = New-InillucentStage -Root $root -Version $Version -Target 'universal-apple-darwin' `
        -BuiltDir $universal -Dist $dist
    $tarball = New-InillucentTarGz -Stage $stage -Dist $dist
    $zip = New-InillucentZip -Stage $stage -Dist $dist
    Write-Host "   $tarball"
    Write-Host "   $zip"

    Write-Host '== package'
    $installerIdentity = if ($SelfSigned) { New-SelfSignedIdentity -Kind installer } else { New-AppleSigningSession -Kind installer }
    $product = New-AppleInstaller -Stage $stage -Dist $dist -Version $Version -IdentityArguments $installerIdentity.Arguments
    Write-Host "   $product"
} finally {
    if ($SelfSigned) {
        foreach ($identity in @($applicationIdentity, $installerIdentity)) {
            if ($identity) {
                foreach ($path in $identity.Scratch) {
                    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -Confirm:$false }
                }
            }
        }
    } else {
        Remove-AppleSigningSession -Session $applicationIdentity
        Remove-AppleSigningSession -Session $installerIdentity
    }
}

if (-not $SkipNotarize) {
    Write-Host '== notarising'
    $notary = $null
    try {
        $notary = New-AppleNotarySession
        Submit-ForNotarisation -Path $product -KeyFile $notary.Path -Staple
        Submit-ForNotarisation -Path $zip -KeyFile $notary.Path
    } finally {
        Remove-AppleNotarySession -Session $notary
    }
}

# The universal binaries are an intermediate: the archive and the package both
# carry their own copies, so leaving the directory in dist/ would put five
# loose signed Mach-O files where a publish step could pick them up.
if (Test-Path -LiteralPath $universal) { Remove-Item -LiteralPath $universal -Recurse -Force -Confirm:$false }

$sums = Update-Sha256Sums -Dist $dist -Version $Version
Write-Host ''
Write-Host "sums $sums"

# ---------------------------------------------------------------------------
# What was checked, and what could not be. Printed every time, because a
# release cut here has not been through the four checks that need a Mach-O to
# run, and a summary that does not say so invites somebody to assume it has.
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host 'checked here:'
Write-Host "  both architectures present in all five binaries, each declaring macOS $expectedMinimumOs"
Write-Host '  every slice signed, hardened runtime set, trusted timestamp attached'
Write-Host '  the .pkg parsed and re-signed by an implementation that did not write it'
if (-not $SkipNotarize) {
    Write-Host '  Apple notarised the .pkg and the .zip, and the .pkg carries a stapled ticket'
} else {
    Write-Host '  NOT notarised (-SkipNotarize), so nothing here is publishable'
}
Write-Host ''
Write-Host 'not checked here, because a Mach-O only runs on macOS:'
Write-Host '  spctl accepting a quarantined copy; a database round trip; the MCP tool list;'
Write-Host '  the x86-64 slice under Rosetta.'
Write-Host '  On any Mac: curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version ' -NoNewline
Write-Host $Version
if ($SelfSigned) {
    Write-Host ''
    Write-Warning 'signed with a throwaway certificate. Gatekeeper rejects these; this run proves the pipeline, not a release.'
}
