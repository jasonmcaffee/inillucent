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

    Since task-1995 that list is the whole release: the macOS half is signed,
    packaged and notarised here too.

    The `.2.28` suffix on the Linux triples is a cargo-zigbuild feature: it is
    the oldest glibc the result will start against. 2.28 is Debian 10, Ubuntu
    18.10, RHEL 8 and Amazon Linux 2023. Building on this machine's WSL instead
    would produce a 2.39 floor, which refuses to start on Debian 12 or RHEL 9.

    The macOS half is packaging/macos/release-macos.ps1, which this calls: it
    joins the two Apple builds into universal binaries with rcodesign, signs
    them with the Developer ID, writes the archives and the .pkg, and notarises
    them over Apple's HTTPS API. None of that needs a Mac any more.
    packaging/macos/release-macos.sh still does the same thing on one, and is
    kept for a machine that has one.

    The one thing that still cannot happen here is running the result, because
    a Mach-O only executes on macOS. release-macos.ps1 prints which checks it
    ran and which four it could not, every time.

.PARAMETER Version
    Overrides the version taken from the workspace manifest.

.PARAMETER Targets
    all (the default), windows, linux, or macos.

.PARAMETER SkipBuild
    Stage from what is already built. For iterating on the packaging itself.

.PARAMETER SkipNotarize
    Passed through to the macOS half: sign and package, submit nothing to
    Apple. Nothing produced under it is publishable.

.PARAMETER Serial
    Build the targets one after another in the shared target directory, the
    way every release before the parallel build was made.

.PARAMETER BuildOnly
    Compile and stop: no staging, no signing, no archives. For measuring the
    build.

.NOTES
    THE FIVE BUILDS RUN AT ONCE

    Fat LTO with one codegen unit spends most of its time on one thread, the
    final optimisation and link of each program. Five of them one after
    another left most of this machine's 24 threads idle for most of a
    release, about 100 s per target measured on the native one. They now start
    together, each with a target directory of its own under
    <target>/release-all/<triple>, because cargo locks a target directory and
    five builds in one would wait on each other. Each build's output goes to
    its own log beside that directory, and a failure prints the end of it.
    The release profile does not change: fat LTO and one codegen unit are the
    fairness contract every published ratio was measured under.

.EXAMPLE
    pwsh packaging/release-all.ps1
    pwsh packaging/release-all.ps1 -Targets linux
#>
[CmdletBinding()]
param(
    [string] $Version,
    [ValidateSet('all', 'windows', 'linux', 'macos')]
    [string] $Targets = 'all',
    [switch] $SkipBuild,
    [switch] $SkipNotarize,
    [switch] $Serial,
    [switch] $BuildOnly
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'stage-layout.ps1')

# Where cargo puts its output. Reading CARGO_TARGET_DIR rather than assuming
# <root>/target is what lets a release keep tens of gigabytes of intermediate
# objects off the C: drive, which has filled on this machine before.
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }

$crossBin = Get-CrossBin -Root $root
$zigDir = Join-Path $crossBin 'zig'
$cargoZigbuild = Join-Path $crossBin 'cargo-zigbuild.exe'

# The programs the release ships, and the only ones built.
#
# `--features inillucent-cli/embed` is what makes `embed(TEXT)` answer in a
# shipped binary. Without it `inillucent setup-embeddings all` downloads 620 MB
# of ONNX Runtime and weights that the program which downloaded them cannot use,
# and `docs/embeddings.md`'s own first example answers `no such function: embed`.
# That was true of every release up to 0.1.1.
#
# It costs 3.2 MB of binary and nothing at run time, because `ort` links
# `load-dynamic`: a machine with no ONNX Runtime installed still runs every
# command that does not embed. What it does add is a C and a C++ dependency -
# `tokenizers` pulls `onig` and `esaxx-rs` - which the two cross compiled
# families build through zig rather than through a platform toolchain.
$packages = @(
    '--features', 'inillucent-cli/embed',
    '-p', 'inillucent-cli', '-p', 'inillucent-migrate', '-p', 'inillucent-driver-capi'
)

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
    # onig_sys compiles oniguruma with cl.exe, which needs INCLUDE and LIB from vcvars64.
    Import-MsvcEnvironment
    & cargo build --manifest-path (Join-Path $root 'Cargo.toml') --release --locked --target $Target @packages
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $Target with $LASTEXITCODE" }
}

function Start-ReleaseBuild {
    <#
    .SYNOPSIS
        Starts one target's release build in the background and returns it.

    .PARAMETER Name
        The triple, used for the target directory and the log.

    .PARAMETER Program
        cargo or cargo-zigbuild.

    .PARAMETER Arguments
        Everything after the program.
    #>
    param([string] $Name, [string] $Program, [string[]] $Arguments)
    $directory = Join-Path $parallelRoot $Name
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $log = Join-Path $parallelRoot "$Name.log"
    $quoted = @($Arguments + @('--target-dir', $directory)) | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }
    $process = Start-Process -FilePath $Program -ArgumentList ($quoted -join ' ') -NoNewWindow -PassThru `
        -WorkingDirectory $root -RedirectStandardOutput "$log.out" -RedirectStandardError $log
    # Reading the handle now is what makes ExitCode readable after the process has gone.
    $null = $process.Handle
    return [pscustomobject]@{ Name = $Name; Process = $process; Log = $log; Started = Get-Date }
}

function Invoke-ParallelBuilds {
    <#
    .SYNOPSIS
        Builds every wanted target at once and waits for all of them.

    .DESCRIPTION
        Throws naming every build that failed, with the end of its log, after all of them have
        finished: stopping at the first would leave the others running with nobody reading them.
    #>
    Import-MsvcEnvironment
    if (-not (Test-Path -LiteralPath $cargoZigbuild) -and ($wantLinux -or $wantMacos)) {
        throw 'cargo-zigbuild is missing. Run: pwsh tools/cross/fetch-toolchain.ps1'
    }
    $previousPath = $env:PATH
    $env:PATH = "$zigDir;$env:PATH"
    $manifest = @('--manifest-path', (Join-Path $root 'Cargo.toml'))
    $builds = @()
    try {
        if ($wantWindows) {
            $builds += Start-ReleaseBuild -Name 'x86_64-pc-windows-msvc' -Program 'cargo' `
                -Arguments (@('build') + $manifest + @('--release', '--locked', '--target', 'x86_64-pc-windows-msvc') + $packages)
        }
        if ($wantLinux) {
            foreach ($triple in @('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')) {
                $builds += Start-ReleaseBuild -Name $triple -Program $cargoZigbuild `
                    -Arguments (@('zigbuild') + $manifest + @('--release', '--locked', '--target', "$triple.2.28") + $packages)
            }
        }
        if ($wantMacos) {
            foreach ($triple in @('aarch64-apple-darwin', 'x86_64-apple-darwin')) {
                $builds += Start-ReleaseBuild -Name $triple -Program $cargoZigbuild `
                    -Arguments (@('zigbuild') + $manifest + @('--release', '--locked', '--target', $triple, '--config', ($appleLinkArgs -f $triple)) + $packages)
            }
        }
    } finally {
        $env:PATH = $previousPath
    }
    $failed = @()
    foreach ($build in $builds) {
        $build.Process.WaitForExit()
        $seconds = [math]::Round(((Get-Date) - $build.Started).TotalSeconds, 1)
        $code = $build.Process.ExitCode
        Write-Host ("   {0,-28} exit {1}, done {2} s after the builds started" -f $build.Name, $code, $seconds)
        if ($code -ne 0) {
            $tail = (Get-Content -LiteralPath $build.Log -Tail 30 -ErrorAction SilentlyContinue) -join "`n"
            $failed += "$($build.Name) exited $code; the end of $($build.Log):`n$tail"
        }
    }
    if ($failed.Count -gt 0) { throw ($failed -join "`n`n") }
}

if (-not $Version) { $Version = Get-WorkspaceVersion -Root $root }
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$wantWindows = $Targets -in @('all', 'windows')
$wantLinux = $Targets -in @('all', 'linux')
$wantMacos = $Targets -in @('all', 'macos')

Write-Host "inillucent $Version"

# Where each target's release build is: its own directory when the builds ran in parallel, the shared
# target directory when they ran one after another.
$parallelRoot = Join-Path $targetDir 'release-all'
$parallel = -not $Serial -and -not $SkipBuild
$builtFromParallel = -not $Serial -and (Test-Path -LiteralPath $parallelRoot)

function Get-ReleaseDir {
    <#
    .SYNOPSIS
        The release output directory of one triple.

    .PARAMETER Triple
        The triple, without a glibc suffix.
    #>
    param([string] $Triple)
    if ($builtFromParallel) { return Join-Path $parallelRoot "$Triple/$Triple/release" }
    return Join-Path $targetDir "$Triple/release"
}

if ($parallel) {
    Write-Host '== building every target at once'
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    Invoke-ParallelBuilds
    $builtFromParallel = $true
    Write-Host ("   every build finished in {0:N1} s" -f $clock.Elapsed.TotalSeconds)
    if ($wantMacos) {
        Write-Host '== macos-pkg'
        & cargo build --manifest-path (Join-Path $root 'tools/macos-pkg/Cargo.toml') --release `
            --target-dir (Join-Path $targetDir 'macos-pkg')
        if ($LASTEXITCODE -ne 0) { throw "building macos-pkg failed with $LASTEXITCODE" }
    }
}
if ($BuildOnly) { return }
$built = $parallel -or $SkipBuild

if ($wantWindows) {
    $target = 'x86_64-pc-windows-msvc'
    Write-Host "== $target"
    if (-not $built) { Invoke-CargoNative -Target $target }
    $stage = New-InillucentStage -Root $root -Version $Version -Target $target `
        -BuiltDir (Get-ReleaseDir -Triple $target) -Dist $dist
    $archive = New-InillucentZip -Stage $stage -Dist $dist
    Write-Host "   $archive"
}

if ($wantLinux) {
    foreach ($pair in @(
        @{ Triple = 'x86_64-unknown-linux-gnu'; Build = 'x86_64-unknown-linux-gnu.2.28' },
        @{ Triple = 'aarch64-unknown-linux-gnu'; Build = 'aarch64-unknown-linux-gnu.2.28' }
    )) {
        Write-Host "== $($pair.Build)"
        if (-not $built) { Invoke-CargoZigbuild -Target $pair.Build }
        $stage = New-InillucentStage -Root $root -Version $Version -Target $pair.Triple `
            -BuiltDir (Get-ReleaseDir -Triple $pair.Triple) -Dist $dist
        $archive = New-InillucentTarGz -Stage $stage -Dist $dist
        Write-Host "   $archive"
    }
}

if ($wantMacos) {
    # The macOS half builds its own two targets, because it sets a deployment
    # target per architecture that the Linux and Windows builds have no use for.
    #
    # **A hashtable, not an array.** `@('-Version', $Version)` splats POSITIONALLY: the string
    # `-Version` binds to the first parameter and the version binds to the second. The first run of
    # this produced `inillucent--Version-universal-apple-darwin.tar.gz`, signed it, and only failed
    # three steps later when macos-pkg refused `--version -Version` as `unexpected argument '-V'`.
    # Hash splatting binds by name and cannot do that.
    $macosArguments = @{ Version = $Version }
    if ($built) { $macosArguments['SkipBuild'] = $true }
    if ($builtFromParallel) { $macosArguments['BuiltRoot'] = $parallelRoot }
    if ($SkipNotarize) { $macosArguments['SkipNotarize'] = $true }
    & (Join-Path $PSScriptRoot 'macos/release-macos.ps1') @macosArguments
    if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) { throw "the macOS release failed with $LASTEXITCODE" }
}

$sums = Update-Sha256Sums -Dist $dist -Version $Version
Write-Host ''
Write-Host "sums $sums"
Get-Content -Path $sums
