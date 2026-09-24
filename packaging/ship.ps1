<#
.SYNOPSIS
    The whole release: build every target, sign it, publish it everywhere, and say what arrived.

.DESCRIPTION
    One command. Before this, a release was ten routes across eight scripts and a page of
    `PUBLISHING.md` saying which to run in what order, and the failure that produces is on disk:
    `CHANGELOG.md` records 0.1.3 as "Tagged and not published. The GitHub release is a draft waiting
    on the Linux archives." The tag was cut on 2026-09-15; four days later the site still served
    0.1.2 and the release was still a draft. Nothing failed. The person running it reached the end of
    what they remembered.

    FIVE PHASES, AND THE ORDER IS THE DESIGN

        1. preflight   read every credential and tool, decide which routes can run, print the plan.
                       Mutates nothing.
        2. tests       `inillucent-testrun --strict`. Refuses the release on any non-zero exit.
        3. version     write the new version into every file that carries it, in one step, and
                       refuse to continue if a seventh file is found holding the old one.
        4. build       compile, sign, package, notarise. Local; nothing has left the machine.
        5. publish     tag, push, GitHub, the mirror, the site, the registries.
        6. report      route by route: published, skipped or failed, and why.

    Preflight is first and separate because a tag is the one step that cannot be taken back quietly.
    Nothing reaches it until the script knows which routes will run.

    TESTS ARE SECOND, AND -Only CANNOT SKIP THEM

    This script published to twelve destinations without running a test, for seven releases. The
    phase sits above `version` rather than below it because the version phase rewrites eight files
    and the publish phase commits them: a suite that goes red after that has already changed the
    tree. Here, a red suite stops the release with nothing written.

    It reads the runner's three exit codes and says which it got. `2` is not a test failure - it
    means the run did not happen, so nothing was graded - and reporting it as a red suite is the
    confusion task-2047 removed from the runner and would put back here.

    `-Only` selects routes, and the tests are not a route, so `-Only site` still runs them. The one
    way past is `-SkipTests`, which prints a sentence saying the release is untested and writes that
    same sentence into the GitHub release notes, where the people downloading it can read it.

    A ROUTE WITH NO CREDENTIAL IS SKIPPED, NOT A FAILURE

    Eight of the routes below are waiting on a token. A script that refuses to run without all of
    them is a script that can never run, so it would be bypassed by hand - and running a release by
    hand is the fault this replaces. A skip is named, and carries the one sentence that would fix it.

    WHAT IT CALLS

    Every script in packaging/ keeps its own parameters and keeps working on its own. This
    orchestrates them; it does not replace them.

.PARAMETER Version
    Release exactly this version. Defaults to the workspace version.

.PARAMETER Part
    Bump the workspace version first: patch, minor or major.

.PARAMETER Only
    Run only these routes. Names are the ones the plan prints.

.PARAMETER Skip
    Run everything except these routes.

.PARAMETER AllowDirty
    Build from a working tree with uncommitted changes. Recorded in the report.

.PARAMETER SkipTests
    Release without running the suite. Prints that the release is untested and says so in the
    GitHub release notes, because a release nobody graded is a fact about the release rather than
    about the person who cut it.

.PARAMETER WhatIf
    Print the plan and every path that would be written, and change nothing.

.EXAMPLE
    pwsh packaging/ship.ps1 -WhatIf
    pwsh packaging/ship.ps1 -Part patch
    pwsh packaging/ship.ps1 -Only site,github
#>
[CmdletBinding()]
param(
    [string] $Version,
    [ValidateSet('patch', 'minor', 'major')]
    [string] $Part,
    [string[]] $Only,
    [string[]] $Skip,
    # The six digits from the authenticator app. Only needed when the npm credential cannot get past
    # two-factor on its own - a classic publish token cannot, a granular one minted to bypass it can,
    # and npm gives no way to tell them apart before the publish. Not needed with the token this
    # machine holds.
    [ValidatePattern('^[0-9]{6}$')]
    [string] $Otp,
    # Where inillucent-site and the Homebrew tap are. Both default to siblings of the main checkout,
    # which is right whether this runs there or in a worktree somewhere else.
    [string] $SitePath,
    [string] $TapPath,
    [switch] $AllowDirty,
    [switch] $SkipTests,
    [switch] $WhatIf
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# **-Only site,pypi arrives as one string under `pwsh -File` (task-1995).** PowerShell splits a
# comma separated list into an array when a script is dot sourced or called from another script,
# and does not when it is launched with -File - there the whole thing is a single element, so
# `$Only -contains 'site'` is false and every route reports "not asked for". That looks exactly like
# a release where nothing needed doing. Splitting here makes both spellings work.
if ($Only) { $Only = @($Only -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ }) }
if ($Skip) { $Skip = @($Skip -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ }) }
. (Join-Path $PSScriptRoot 'stage-layout.ps1')
# The DPAPI sealing helpers live in apple-credentials.ps1 because that is where sealing was first
# needed. Nothing about `Protect-AppleSecret` is Apple-specific: it is `ConvertFrom-SecureString`,
# which encrypts under one Windows account, and the project's minisign and OpenPGP keys want exactly
# the same treatment as the Developer ID one.
. (Join-Path $PSScriptRoot 'macos/apple-credentials.ps1')

$script:Outcomes = [ordered]@{}
# Empty unless -SkipTests was given. Read by `Publish-GitHubRelease`, which puts it in the notes.
$script:UntestedNote = ''
$script:Dist = Join-Path $root 'dist'

# **Script scope, and that is not a style choice.** Each route's `Needs`, `Run` and `Verify` is a
# scriptblock held in a table and invoked long after `Get-Routes` has returned. PowerShell resolves a
# scriptblock's free variables when it runs, not where it was written, so a local of `Get-Routes` is
# gone by then and reads as $null - which `Join-Path` then refuses with "Cannot bind argument to
# parameter 'Path' because it is null", from inside preflight, naming nothing useful.
# **Resolved against the main checkout, not against $root (task-1995).** A release is often cut from
# a `git worktree` - the ordinary checkout is where day to day work happens and is frequently dirty,
# and `cargo publish` and this script both refuse a dirty tree. A worktree lives wherever it was put,
# usually on another drive, so `../inillucent-site` and `../homebrew-inillucent` resolve to nothing
# and the site and Homebrew routes skip with "does not exist" - the two routes a person is most
# likely to assume ran. `git rev-parse --git-common-dir` names the original repository's .git from
# inside any worktree, so the siblings are found from there.
$script:MainCheckout = $root
$commonDir = (& git -C $root rev-parse --git-common-dir 2>$null)
if ($commonDir) {
    $resolved = if ([System.IO.Path]::IsPathRooted($commonDir)) { $commonDir } else { Join-Path $root $commonDir }
    $script:MainCheckout = Split-Path -Parent ([System.IO.Path]::GetFullPath($resolved))
}
# zig, rcodesign and the macOS SDK are gitignored - they are a few gigabytes of downloaded
# toolchain, not source - so a fresh worktree has an empty tools/cross/bin and the build route skips
# with "run fetch-toolchain". The toolchain is not per checkout, so the main one is used.
$script:CrossBin = Get-CrossBin -Root $root
# Exported so every script this one calls resolves the same directory, rather than each repeating
# the search and one of them getting a different answer from the preflight that cleared it.
$env:INILLUCENT_CROSS_BIN = $script:CrossBin
$script:SitePath = if ($SitePath) { $SitePath } else { Join-Path (Split-Path -Parent $script:MainCheckout) 'inillucent-site' }
$script:TapPath = if ($TapPath) { $TapPath } else { Join-Path (Split-Path -Parent $script:MainCheckout) 'homebrew-inillucent' }
$script:Packaging = $PSScriptRoot
# Packagist signs in with GitHub, so the account is a person rather than the organisation.
$script:PackagistUser = if ($env:PACKAGIST_USER) { $env:PACKAGIST_USER } else { 'jasonmcaffee' }
$script:Root = $root

function Write-Phase {
    <#
    .SYNOPSIS
        Prints a phase heading.

    .PARAMETER Text
        The heading.
    #>
    param([string] $Text)
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

function Get-NextVersion {
    <#
    .SYNOPSIS
        The version one step up from the current one.

    .PARAMETER Current
        The version in Cargo.toml.

    .PARAMETER Which
        patch, minor or major.
    #>
    param([string] $Current, [string] $Which)
    $parts = $Current.Split('.') | ForEach-Object { [int] $_ }
    switch ($Which) {
        'major' { return "$($parts[0] + 1).0.0" }
        'minor' { return "$($parts[0]).$($parts[1] + 1).0" }
        default { return "$($parts[0]).$($parts[1]).$($parts[2] + 1)" }
    }
}

# ---------------------------------------------------------------------------
# Phase 2: the version, in every file that carries it.
# ---------------------------------------------------------------------------

function Get-VersionCarriers {
    <#
    .SYNOPSIS
        Every file that writes the release version down, and how to rewrite each.

    .DESCRIPTION
        **Six files, and before this they drifted.** Measured on 2026-09-19:
        `Cargo.toml` and `packages/python/pyproject.toml` said 0.1.4; the npm wrapper said 0.1.4
        while pinning its five platform packages at 0.1.2, so `npm install inillucent@0.1.4` would
        resolve a wrapper and a binary from different releases; the Homebrew formula said 0.1.4 while
        every `url` in it named a 0.1.2 archive; and the site served 0.1.2. Nothing caught any of it,
        because no single thing wrote them all.

        The Homebrew formula's urls and checksums are not here: `homebrew/update.sh` fills those from
        the built `dist/SHA256SUMS`, which is the only place they can come from without being typed.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    $escaped = [regex]::Escape($Version)
    return @(
        @{
            Path    = Join-Path $root 'Cargo.toml'
            Pattern = '(?m)^(version = ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = "(?m)^version = ""$escaped"""
            What    = '[workspace.package] version'
        },
        @{
            Path    = Join-Path $root 'packages/npm/inillucent/package.json'
            Pattern = '(?m)^(  "version": ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = """version"": ""$escaped"""
            What    = 'the npm wrapper'
        },
        @{
            Path    = Join-Path $root 'packages/npm/inillucent/package.json'
            Pattern = '(?m)^(    "@blackrainbowlabs/cli-[a-z0-9-]+": ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = "@blackrainbowlabs/cli-win32-x64"": ""$escaped"""
            What    = 'the five platform packages the wrapper pins'
        },
        @{
            # **The Go wrapper pins the release it installs, so it carries the version in code.**
            # Its own comment says why - `go install ...@v0.1.1` must install 0.1.1 rather than
            # whatever is newest - and nothing updated it, so v0.1.5 of the module would have
            # installed the 0.1.4 binaries. The straggler scan cannot catch it either: it ignores
            # `packages/go/`, which legitimately names old versions in test data and in examples.
            Path    = Join-Path $root 'packages/go/cmd/inillucent-install/main.go'
            Pattern = '(?m)^(const nativeVersion = ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = "const nativeVersion = " + [char]34 + $escaped + [char]34
            What    = 'the Go wrapper pinned release'
        },
        @{
            Path    = Join-Path $root 'packages/python/pyproject.toml'
            Pattern = '(?m)^(version = ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = "(?m)^version = ""$escaped"""
            What    = 'the Python distribution'
        },
        @{
            Path    = Join-Path $root 'packaging/homebrew/inillucent.rb'
            Pattern = '(?m)^(  version ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = "(?m)^  version ""$escaped"""
            What    = 'the Homebrew formula'
        },
        # These two were found by the straggler scan below, on its first run, which is the whole
        # argument for having one: each names the version in code rather than in a manifest, so
        # nothing about either looks like a version to somebody adding a release step.
        @{
            Path    = Join-Path $root 'packages/php/bin/inillucent-install'
            Pattern = "(?m)^(const NATIVE_VERSION = ')[^']+(';)"
            Replace = "`${1}$Version`${2}"
            What    = 'the native version the PHP installer downloads'
        },
        @{
            Path    = Join-Path $root 'packages/python/src/inillucent/__init__.py'
            Pattern = '(?m)^(__version__ = ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            What    = 'the __version__ the Python package reports'
        }
    )
}

function Set-ReleaseVersion {
    <#
    .SYNOPSIS
        Writes the version into every carrier, and reports anything still holding the old one.

    .PARAMETER Version
        The version being released.

    .PARAMETER Previous
        The version being replaced, which is what the straggler scan looks for.
    #>
    param([string] $Version, [string] $Previous)

    foreach ($carrier in Get-VersionCarriers -Version $Version) {
        if (-not (Test-Path -LiteralPath $carrier.Path)) {
            Write-Host "   absent, skipped: $($carrier.Path)"
            continue
        }
        $text = [System.IO.File]::ReadAllText($carrier.Path)
        $updated = [regex]::Replace($text, $carrier.Pattern, $carrier.Replace)
        $relative = $carrier.Path.Substring($root.Length + 1)
        if ($updated -eq $text) {
            Write-Host "   already $Version : $relative ($($carrier.What))"
            continue
        }
        Write-Host "   $relative -> $Version ($($carrier.What))"
        if (-not $WhatIf) { [System.IO.File]::WriteAllText($carrier.Path, $updated) }
    }

    # **Cargo.lock carries the workspace's own versions, and the build passes --locked.** Writing
    # 0.1.5 into Cargo.toml and leaving the lock at 0.1.4 made the first build of the release stop
    # at `cannot update the lock file because --locked was passed`, after the version phase had
    # already rewritten seven files. `--offline` because nothing about a version bump needs the
    # network, and `--workspace` so only the members' own entries move - a release is not the place
    # to pick up a new dependency.
    # Unconditional, not "only when the version changed". A re-run of a release that already wrote
    # the version finds nothing to change and skipped this, so the lock stayed at the old version
    # and the build stopped in exactly the same place the second time. It is idempotent and offline.
    if (-not $WhatIf) {
        Write-Host '   Cargo.lock'
        & cargo update --manifest-path (Join-Path $root 'Cargo.toml') --workspace --offline *> $null
        if ($LASTEXITCODE -ne 0) { throw 'refreshing Cargo.lock after the version bump failed' }
    }

    Find-VersionStraggler -Previous $Previous -Version $Version
}

function Find-VersionStraggler {
    <#
    .SYNOPSIS
        Warns about any tracked file still naming the previous version.

    .DESCRIPTION
        This is what stops the drift coming back. A new manifest, a new package or a README that
        prints an install line will hold the version too, and it will not be in the table above until
        somebody adds it. The changelog and the lock file are expected to name old versions, so they
        are not reported.

    .PARAMETER Previous
        The version being replaced.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Previous, [string] $Version)
    if (-not $Previous -or $Previous -eq $Version) { return }

    # What is not reported, and why each one.
    #
    #   the carriers        this function has just written them; under -WhatIf it has not, and
    #                       naming a file it is about to fix hides the one real find in the noise.
    #   *.md                prose. A README naming the version it was written against is correct.
    #   CHANGELOG, tasks/   history: old versions are the point.
    #   Cargo.lock          cargo writes it, and it names every dependency's version as well.
    #   dist/, packages/go/ build output, and a Go module is versioned by its tag rather than a file.
    $carriers = @(Get-VersionCarriers -Version $Version | ForEach-Object {
            $_.Path.Substring($root.Length + 1).Replace('\', '/')
        })
    # `Cargo.lock` matched only the one at the root. `tools/macos-pkg/Cargo.lock` is a lock file for
    # exactly the reason the note above gives - it names every dependency's version - and it was
    # reported on the 0.1.7 run for holding `android_system_properties 0.1.6`.
    $ignore = @('CHANGELOG.md', 'Cargo.lock', 'tasks/', 'docs/', 'dist/', 'packages/go/')
    $ignoreLeaf = @('Cargo.lock')
    $hits = & git -C $root grep -l --fixed-strings -- $Previous 2>$null
    $unexpected = @($hits | Where-Object {
            $path = $_
            $path -notlike '*.md' -and
            $carriers -notcontains $path -and
            $ignoreLeaf -notcontains (Split-Path -Leaf $path) -and
            -not ($ignore | Where-Object { $path -like "$_*" })
        } | Where-Object {
            # **A version named only in a comment is prose (task-1995).** The scripts in packaging/
            # explain past releases by name - "0.1.3 was tagged and not published for four days" -
            # and flagging those on every release afterwards is how a warning stops being read. A
            # file counts only if the old version appears somewhere that is not a comment line.
            $lines = & git -C $root grep -h --fixed-strings -- $Previous -- $_ 2>$null
            # **A whole version, not a substring.** `--fixed-strings` found 0.1.6 inside
            # `iana-time-zone 0.1.65`, so a dependency's version reported the project's as a
            # straggler. A digit or a dot on either side means it is part of a longer number.
            $whole = "(?<![0-9.])" + [regex]::Escape($Previous) + "(?![0-9.])"
            @($lines | Where-Object { $_ -notmatch '^\s*(#|//|\*|<#)' -and $_ -match $whole }).Count -gt 0
        })
    if ($unexpected.Count -gt 0) {
        Write-Host ''
        Write-Warning "these tracked files still name $Previous and are not in Get-VersionCarriers:"
        $unexpected | ForEach-Object { Write-Host "     $_" }
        Write-Warning 'add each to the table, or confirm it is meant to name an older release.'
    }
}

# ---------------------------------------------------------------------------
# Phase 1: what can run.
# ---------------------------------------------------------------------------

function Resolve-GitHubToken {
    <#
    .SYNOPSIS
        A token gh can use, from GH_TOKEN, from a gh login, or from the credential git already has.

    .DESCRIPTION
        **`gh` has never been logged in on this machine, and the release needed it anyway.** The
        github route checked only that the program was installed, so preflight said `[run ]` and the
        publish would have answered `To get started with GitHub CLI, please run: gh auth login` -
        the same false positive the npm route had twice. It is not a small one: inillucent 0.1.3
        reached the site and PyPI on 2026-09-19 with no GitHub release at all, and nothing said so.

        Asking for `gh auth login` would be asking for a second credential for a host this machine
        is already authenticated to. `git push` works here because Git Credential Manager holds an
        OAuth token for github.com, and `git credential fill` is the supported way to read it - the
        same interface git itself uses. So the order is: an explicit GH_TOKEN, then a real gh login,
        then git's stored credential.

        The token is returned rather than printed, and `Remove-SigningSecrets` clears it.
    #>
    if ($env:GH_TOKEN) { return $env:GH_TOKEN }
    & gh auth status *> $null
    if ($LASTEXITCODE -eq 0) { return $null }   # gh has its own login; leave it alone.
    $answer = ("protocol=https`nhost=github.com`n`n" | & git credential fill 2>$null)
    $line = $answer | Where-Object { $_ -like 'password=*' } | Select-Object -First 1
    if (-not $line) { return $null }
    return $line.Substring('password='.Length)
}

function Import-SigningSecrets {
    <#
    .SYNOPSIS
        Puts the project's signing keys into the environment for this run, from the sealed store.

    .DESCRIPTION
        `sign-sums.ps1` and `linux/package-linux.ps1` read theirs out of the environment, which is
        right for them - they are usable on their own and on another machine. On this machine the
        keys are sealed with DPAPI under Jason's account, so nothing has to be exported by hand
        before a release and nothing sits in the clear:

            %LOCALAPPDATA%\inillucent\signing\minisign.key.sealed
            %LOCALAPPDATA%\inillucent\signing\gpg.passphrase.sealed
            %LOCALAPPDATA%\inillucent\signing\gpg.keyid

        Anything already in the environment wins, so a one-off run with a different key still works.
        The unsealed minisign key is written to the RAM disk and deleted by
        Remove-SigningSecrets in a `finally`.
    #>
    $store = Join-Path $env:LOCALAPPDATA 'inillucent\signing'
    $script:SigningScratch = @()

    $sealedMinisign = Join-Path $store 'minisign.key.sealed'
    if (-not $env:INILLUCENT_MINISIGN_KEY -and (Test-Path -LiteralPath $sealedMinisign)) {
        $keyFile = Join-Path (Get-AppleScratchDir) ('minisign-' + [guid]::NewGuid().ToString('N') + '.key')
        Set-Content -Path $keyFile -Value (Unprotect-AppleSecret -Path $sealedMinisign) -NoNewline
        $env:INILLUCENT_MINISIGN_KEY = $keyFile
        # **Empty, and set rather than absent.** The sealed key is created with `minisign -G -W`,
        # which writes an unencrypted secret key - DPAPI under this Windows account protects it, not
        # a passphrase, because an unattended release has nobody to type one. `sign-sums.ps1`
        # refuses when this variable is unset and accepts an empty string, so leaving it out stopped
        # the signature route with `INILLUCENT_MINISIGN_PASSPHRASE is not set` while the key beside
        # it was perfectly usable.
        if ($null -eq $env:INILLUCENT_MINISIGN_PASSPHRASE) { $env:INILLUCENT_MINISIGN_PASSPHRASE = '' }
        $script:SigningScratch += $keyFile
    }

    # npm reads its credential out of an npmrc rather than an environment variable, so the token is
    # unsealed into one on the RAM disk and npm is pointed at it.
    #
    # **The sealed token wins over an inherited `npm_config_userconfig`.** Every other secret here
    # follows "anything already in the environment wins", and for npm that rule is backwards: this
    # machine exports `npm_config_userconfig=C:\Users\jason\.npmrc`, and that file holds a token
    # which answers 401. Deferring to it meant the preflight reported npm as unusable while a
    # working token sat sealed beside it. Only an explicit NPM_TOKEN defers now, because that is
    # somebody deliberately choosing a different credential rather than a machine-wide default.
    $sealedNpm = Join-Path $store 'npm.token.sealed'
    if (-not $env:NPM_TOKEN -and (Test-Path -LiteralPath $sealedNpm)) {
        $script:PreviousNpmConfig = $env:npm_config_userconfig
        $npmrc = Join-Path (Get-AppleScratchDir) ('npmrc-' + [guid]::NewGuid().ToString('N'))
        Set-Content -Path $npmrc -Encoding ascii -NoNewline `
            -Value "//registry.npmjs.org/:_authToken=$(Unprotect-AppleSecret -Path $sealedNpm)"
        $env:npm_config_userconfig = $npmrc
        $script:SigningScratch += $npmrc
    }

    # twine takes `__token__` as the username and the API token as the password.
    $sealedPypi = Join-Path $store 'pypi.token.sealed'
    if (-not $env:TWINE_PASSWORD -and (Test-Path -LiteralPath $sealedPypi)) {
        $env:TWINE_USERNAME = '__token__'
        $env:TWINE_PASSWORD = Unprotect-AppleSecret -Path $sealedPypi
        $env:TWINE_NON_INTERACTIVE = '1'
    }

    # gh reads GH_TOKEN before anything else, so this is all it takes to make the github route work
    # on a box where `gh auth login` was never run.
    $resolved = Resolve-GitHubToken
    if ($resolved) { $env:GH_TOKEN = $resolved }

    # crates.io reads CARGO_REGISTRY_TOKEN, so the sealed token is all this route needs.
    $sealedCrates = Join-Path $store 'crates.token.sealed'
    if (-not $env:CARGO_REGISTRY_TOKEN -and (Test-Path -LiteralPath $sealedCrates)) {
        $env:CARGO_REGISTRY_TOKEN = Unprotect-AppleSecret -Path $sealedCrates
    }

    $keyIdFile = Join-Path $store 'gpg.keyid'
    $sealedPassphrase = Join-Path $store 'gpg.passphrase.sealed'
    if (-not $env:INILLUCENT_GPG_KEY -and (Test-Path -LiteralPath $keyIdFile)) {
        $env:INILLUCENT_GPG_KEY = (Get-Content -Path $keyIdFile -Raw).Trim()
    }
    if (-not $env:INILLUCENT_GPG_PASSPHRASE -and (Test-Path -LiteralPath $sealedPassphrase)) {
        $env:INILLUCENT_GPG_PASSPHRASE = Unprotect-AppleSecret -Path $sealedPassphrase
    }
}

function Remove-SigningSecrets {
    <#
    .SYNOPSIS
        Deletes what Import-SigningSecrets unsealed, and clears the passphrase.
    #>
    foreach ($path in $script:SigningScratch) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -Confirm:$false }
    }
    $env:INILLUCENT_GPG_PASSPHRASE = $null
    $env:INILLUCENT_MINISIGN_PASSPHRASE = $null
    $env:CARGO_REGISTRY_TOKEN = $null
    $env:GH_TOKEN = $null
    $env:TWINE_PASSWORD = $null
    # Put back whatever the machine had, rather than clearing it: npm outside this script should
    # keep reading the config it was pointed at.
    $env:npm_config_userconfig = $script:PreviousNpmConfig
}

function Test-Tool {
    <#
    .SYNOPSIS
        Whether a program is available, by path or on PATH.

    .PARAMETER Name
        The command or the full path.
    #>
    param([string] $Name)
    if (Test-Path -LiteralPath $Name) { return $true }
    return [bool] (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Get-Routes {
    <#
    .SYNOPSIS
        Every route a release takes, in the order it takes them.

    .DESCRIPTION
        `Needs` returns $null when the route can run, or the one sentence that says what is missing.
        `Run` does it. `Verify` asks the destination whether it arrived, and returns $null or a
        reason - a route whose check fails is reported as failed even when its command exited 0.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)

    return @(
        @{
            Name  = 'build'
            What  = 'every target, compiled'
            Needs = {
                if (-not (Test-Tool (Join-Path $script:CrossBin 'cargo-zigbuild.exe'))) {
                    return 'tools/cross/bin is empty. Run: pwsh tools/cross/fetch-toolchain.ps1'
                }
                $null
            }
            Run   = { & (Join-Path $script:Packaging 'release-all.ps1') -Version $Version -Targets all }
        },
        @{
            Name  = 'linux-packages'
            What  = 'the .deb and the .rpm'
            Needs = {
                if (-not $env:INILLUCENT_GPG_KEY) {
                    return 'INILLUCENT_GPG_KEY is not set. apt and dnf refuse an unsigned package from outside a distribution.'
                }
                $null
            }
            Run   = { & (Join-Path $script:Packaging 'linux/package-linux.ps1') -Version $Version }
        },
        @{
            Name  = 'signature'
            What  = 'the detached signature over SHA256SUMS'
            Needs = {
                if (-not $env:INILLUCENT_MINISIGN_KEY) {
                    return 'INILLUCENT_MINISIGN_KEY is not set. Create a minisign key pair once; packaging/inillucent.pub must exist too.'
                }
                $null
            }
            Run   = { & (Join-Path $script:Packaging 'sign-sums.ps1') }
        },
        @{
            Name  = 'tag'
            What  = 'the commit, the tag and the push'
            Needs = { $null }
            Run   = { Publish-Tag -Version $Version }
        },
        # **The mirror goes first, and the order is load bearing (task-1995).** The GitHub
        # release is created on the public repository, and `gh release create` against a tag
        # that does not exist makes one at that repository's current HEAD - which, before the
        # mirror route has run, is the previous release's commit. The release would then carry
        # this version's artifacts on last version's source. The Go module tag has the same
        # dependency and already ran after this.
        @{
            Name  = 'mirror'
            What  = 'the public source mirror'
            Needs = { $null }
            # **-Push alone. -Verify means "check the mirror as it stands and exit".** Passing both
            # made this route print a tidy report of the mirror's existing tags and push nothing,
            # every time, while reporting success - so the public source mirror was never updated by
            # a release. The damage showed up two routes later: `gh release create` against a tag
            # that does not exist makes one at the repository's current HEAD, which was the previous
            # release's commit, and the Go module tag then followed it. proxy.golang.org caches a
            # module version permanently on first fetch, so v0.1.5 of the Go module serves 0.1.3's
            # source and cannot be corrected.
            Run   = { & (Join-Path $script:Packaging 'mirror-github.ps1') -Version $Version -Push }
        },
        @{
            Name   = 'github'
            What   = 'the GitHub release, with every asset'
            Needs  = {
                if (-not (Test-Tool 'gh')) { return 'gh is not installed. https://cli.github.com' }
                # Installed is not signed in, and this route has been the difference between a
                # release that exists on GitHub and one that does not. One request, and it names the
                # account, so publishing as the wrong one is visible in the plan.
                $who = (& gh api user --jq '.login' 2>&1 | Out-String).Trim()
                if ($LASTEXITCODE -ne 0 -or -not $who -or $who -match '\s') {
                    return "gh is not authenticated: $($who -replace '\s+', ' ')"
                }
                $script:GitHubAccount = $who
                $null
            }
            Run    = { Publish-GitHubRelease -Version $Version }
            Verify = { Test-GitHubRelease -Version $Version }
        },
        # **After the GitHub release, because the fixture is built from the published archive.**
        # `tools/build-interop-fixture.ps1` downloads this version's Windows zip, verifies it
        # against SHA256SUMS and the minisign signature, runs `tests/interop/build.sql` with it and
        # checks in what it produced. Without this route the directory lags by one release for
        # ever, and `release_format.rs` grades a format nobody is shipping.
        @{
            Name   = 'interop'
            What   = 'tests/interop/<version>, written by the binary this release publishes'
            Needs  = { $null }
            Run    = { Publish-InteropFixture -Version $Version }
            Verify = { Test-InteropFixture -Version $Version }
        },
        @{
            Name   = 'site'
            What   = 'inillucent.com: the artifacts, then the links'
            Needs  = {
                if (-not (Test-Path -LiteralPath $script:SitePath)) { return "$script:SitePath does not exist." }
                # **The installers are parsed before they are published (task-1995).** install.sh is
                # what `curl -fsSL https://inillucent.com/downloads/install.sh | sh` runs, and it
                # shipped with an unbalanced quote for at least three releases: a literal carriage
                # return inside `tr -d '...'` had been normalised into a newline, which split the
                # command and left the script unparseable. It failed on line 1 with "Unterminated
                # quoted string", so the install command the README gives for macOS and Linux did
                # nothing, and no release noticed because nothing ever ran it.
                foreach ($script in @('install.sh', 'macos/verify-macos.sh')) {
                    $path = Join-Path $script:Packaging $script
                    if (-not (Test-Path -LiteralPath $path)) { continue }
                    # The script goes in on standard input rather than as a path. `bash` on this machine can be
                    # WSL's, which cannot open a Windows path such as J:/build/release, and the site route
                    # was skipped as "No such file or directory".
                    $complaint = ([System.IO.File]::ReadAllText($path) | & bash -n 2>&1 | Out-String).Trim()
                    if ($LASTEXITCODE -ne 0) { return "packaging/$script does not parse: $complaint" }
                    # **And no carriage returns, which `bash -n` does not object to.** These scripts
                    # run under `sh`, which on Debian and Ubuntu is dash, and dash reads a CR as
                    # part of the command: a CRLF script fails at `set: Illegal option -` on its
                    # second line. .gitattributes marks *.sh as eol=lf, and install.sh escaped that
                    # conversion for three releases because git leaves a file that already contains
                    # a carriage return alone - and this one had one, inside a `tr -d` argument.
                    $bytes = [System.IO.File]::ReadAllBytes($path)
                    $carriageReturns = @($bytes | Where-Object { $_ -eq 13 }).Count
                    if ($carriageReturns -gt 0) {
                        return "packaging/$script holds $carriageReturns carriage return(s). sh on Linux is dash, which fails on the first one. Write it with LF."
                    }
                }
                $null
            }
            Run    = {
                # -SitePath, because publish-site.ps1 defaults to a sibling of its own checkout and
                # a release is cut from a worktree, where that is nothing. Preflight checked the
                # path this script resolved and the run used a different one.
                & (Join-Path $script:Packaging 'publish-site.ps1') -Version $Version -Stage -SitePath $script:SitePath
                & (Join-Path $script:Packaging 'publish-site.ps1') -Version $Version -Link -SitePath $script:SitePath
                # **And deploy it, because staging is not publishing.** The site is a static export
                # served out of `out/` by a small Rust binary, and `public/downloads/` is gitignored
                # - so the artifacts reach the live site only through a build and a restart. Without
                # this the 0.1.5 run staged and linked everything and inillucent.com still answered
                # 0.1.3, which the route's own Verify caught.
                & (Join-Path $script:Packaging 'deploy-site.ps1') -SitePath $script:SitePath
            }
            Verify = { Test-SiteVersion -Version $Version }
        },
        @{
            Name   = 'crates'
            What   = 'crates.io'
            Needs  = {
                if (-not $env:CARGO_REGISTRY_TOKEN) { return 'CARGO_REGISTRY_TOKEN is not set.' }
                $null
            }
            Run    = { & (Join-Path $script:Packaging 'cargo-publish.ps1') -Execute -Confirmed }
            Verify = { Test-Registry -Url "https://crates.io/api/v1/crates/inillucent" -Version $Version }
        },
        @{
            Name   = 'npm'
            What   = 'the npm wrapper and its four platform packages'
            Needs  = {
                if (-not (Test-Tool 'npm')) { return 'npm is not installed.' }
                # **Asked, not assumed.** A `~/.npmrc` with a token in it is not a working
                # credential: this machine had one that answered 401, and a preflight that checks
                # only whether the file exists reports a route as ready and then fails in the
                # publish phase - which is the failure preflight exists to prevent. `npm whoami` is
                # one request and it also names the account, so publishing under the wrong one is
                # visible in the plan rather than discovered afterwards.
                $who = (& npm whoami 2>&1 | Out-String).Trim()
                if (-not $who -or $who -match 'E401|Unauthorized|ENEEDAUTH') {
                    # npm's own words, because "not signed in" hid the real answer once already.
                    return "npm rejected the credential: $($who -replace '\s+', ' ')"
                }
                $script:NpmAccount = $who

                # **Being signed in is not being able to publish, and npm will not say which you
                # have.** A classic publish token and a granular token minted to bypass two-factor
                # both authenticate, both name the account, and `npm token list` prints both as
                # "Publish token" - the first then answers `403 ... Two-factor authentication or
                # granular access token with bypass 2fa enabled is required to publish packages` and
                # the second publishes. Reading that output was tried and it is wrong in both
                # directions: it passed the token that could not publish, and once a working one was
                # minted it skipped the route that would have succeeded.
                #
                # So there is no preflight for this, and pretending otherwise is the worse failure.
                # The publish is the test, it fails with npm's own sentence, and `Verify` asks the
                # registry what it serves. `-Otp` is there for an account whose token cannot bypass
                # the second factor.
                $null
            }
            Run    = {
                $publishArguments = @('--publish')
                if ($Otp) { $publishArguments += @('--otp', $Otp) }
                & node (Join-Path $script:Root 'packages/npm/build.mjs') @publishArguments
            }
            Verify = { Test-Registry -Url "https://registry.npmjs.org/inillucent" -Version $Version }
        },
        @{
            Name   = 'pypi'
            What   = 'the Python wheel'
            Needs  = {
                if (-not $env:TWINE_PASSWORD -and -not (Test-Path -LiteralPath (Join-Path $HOME '.pypirc'))) {
                    return 'no PyPI credential: nothing sealed at %LOCALAPPDATA%\inillucent\signing\pypi.token.sealed, TWINE_PASSWORD is unset and there is no ~/.pypirc.'
                }
                if (-not (Test-Tool 'python')) { return 'python is not installed.' }
                $probe = & python -m twine --version 2>&1
                if ($LASTEXITCODE -ne 0) { return 'twine is not installed: python -m pip install twine' }
                $null
            }
            Run    = { & python (Join-Path $script:Root 'packages/python/build.py') --all --publish }
            Verify = { Test-Registry -Url "https://pypi.org/pypi/inillucent/json" -Version $Version }
        },
        @{
            Name   = 'go'
            What   = 'the Go module tag'
            Needs  = { $null }
            Run    = { Publish-GoModule -Version $Version }
            # **The path is case escaped, which is not optional (task-1995).** proxy.golang.org
            # lower cases a module path and marks each original capital with a leading `!`, so
            # `Black-Rainbow-Labs/Inillucent` is asked for as `!black-!rainbow-!labs/!inillucent`.
            # The unescaped path is a different module that does not exist, so this reported "does
            # not name 0.1.7 yet" for a tag that had been pushed correctly - a verifier that fails
            # on a healthy release teaches people to ignore it.
            Verify = { Test-Registry -Url 'https://proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@latest' -Version $Version }
        },
        @{
            Name   = 'packagist'
            What   = 'Composer: Packagist re-reads the tags'
            Needs  = {
                $sealed = Join-Path $env:LOCALAPPDATA 'inillucent\signing\packagist.token.sealed'
                if (-not (Test-Path -LiteralPath $sealed)) {
                    return 'no Packagist token: nothing sealed at %LOCALAPPDATA%\inillucent\signing\packagist.token.sealed.'
                }
                $null
            }
            # **Told, rather than left to notice (task-1995).** Packagist crawls on its own schedule,
            # and it pins a version's commit the first time it sees the tag and never moves it -
            # the same immutability the Go proxy has. v0.1.6 was crawled while the mirror's tag
            # still pointed at the previous release's commit, so `composer require
            # black-rainbow-labs/inillucent:0.1.6` installs 0.1.3's source and cannot be corrected.
            # Asking for the crawl here, after the mirror route has pushed, is what makes the commit
            # it reads the right one.
            Run    = {
                $token = Unprotect-AppleSecret -Path (Join-Path $env:LOCALAPPDATA 'inillucent\signing\packagist.token.sealed')
                $url = "https://packagist.org/api/update-package?username=$script:PackagistUser&apiToken=$token"
                $body = '{"repository":{"url":"https://github.com/Black-Rainbow-Labs/Inillucent"}}'
                $answer = Invoke-RestMethod -Uri $url -Method Post -ContentType 'application/json' -Body $body -TimeoutSec 60
                Write-Host "  packagist: $($answer.status)"
                if ($answer.status -ne 'success') { throw "Packagist answered $($answer.status)" }
            }
            Verify = { Test-Registry -Url 'https://repo.packagist.org/p2/black-rainbow-labs/inillucent.json' -Version $Version }
        },
        @{
            Name  = 'homebrew'
            What  = 'the Homebrew formula'
            Needs = {
                if (-not (Test-Path -LiteralPath $script:TapPath)) {
                    return "$script:TapPath does not exist; the tap has not been created."
                }
                $null
            }
            Run   = {
                & bash (Join-Path $script:Packaging 'homebrew/update.sh') --tap $script:TapPath
                if ($LASTEXITCODE -ne 0) { throw "homebrew/update.sh failed with $LASTEXITCODE" }
                # **And commit and push it (task-1995).** update.sh writes Formula/inillucent.rb and
                # prints the git commands to run next, so the route reported success while the tap
                # on GitHub - the only copy `brew install` reads - still served the previous
                # release. Measured after the 0.1.6 run: the formula on disk said 0.1.6 and
                # raw.githubusercontent.com served 0.1.3.
                & git -C $script:TapPath add Formula/inillucent.rb
                $staged = & git -C $script:TapPath diff --cached --name-only
                if (-not $staged) { Write-Host "  the tap already has $Version"; return }
                & git -C $script:TapPath commit -m "inillucent $Version"
                if ($LASTEXITCODE -ne 0) { throw 'committing the formula failed' }
                & git -C $script:TapPath push
                if ($LASTEXITCODE -ne 0) { throw 'pushing the tap failed' }
            }
            Verify = {
                # The published copy, because that is the one brew reads.
                $url = 'https://raw.githubusercontent.com/Black-Rainbow-Labs/homebrew-inillucent/main/Formula/inillucent.rb'
                Test-Registry -Url $url -Version $Version
            }
        }
    )
}

# ---------------------------------------------------------------------------
# The routes that are this script's own work rather than another script's.
# ---------------------------------------------------------------------------

function Publish-Tag {
    <#
    .SYNOPSIS
        Commits the version files, tags the release and pushes both.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    # **Read from the carrier table rather than written out again (task-1995).** This was a list of
    # five beside a table of seven, so `packages/php/bin/inillucent-install` and the Python
    # package's `__init__.py` were rewritten by the version phase and never committed - left as
    # uncommitted changes in a tree the next release refuses to build from. They are the two
    # carriers that name the version in code rather than in a manifest, which is exactly why the
    # straggler scan had to find them in the first place; adding them to a second hand-kept list
    # would have been the same mistake again.
    $paths = @('Cargo.lock') + (Get-VersionCarriers -Version $Version |
        ForEach-Object { Resolve-Path -LiteralPath $_.Path -Relative -RelativeBasePath $root -ErrorAction SilentlyContinue } |
        ForEach-Object { $_ -replace '^\.[\/]', '' })
    $paths = $paths | Where-Object { $_ } | Select-Object -Unique
    & git -C $root add -- $paths
    $staged = & git -C $root diff --cached --name-only
    if ($staged) {
        & git -C $root commit -m "inillucent $Version"
        if ($LASTEXITCODE -ne 0) { throw 'the version commit failed' }
    }
    if (-not (& git -C $root tag --list "v$Version")) {
        & git -C $root tag -a "v$Version" -m "inillucent $Version"
        if ($LASTEXITCODE -ne 0) { throw "tagging v$Version failed" }
    }
    # **The branch is named rather than implied.** Cut from a worktree, HEAD is on a branch of its
    # own with no upstream, and a bare `git push` then fails with "no upstream branch" after the
    # commit and the tag have already been made - which is the worst place to stop, because the next
    # run sees the tag and skips the work. Pushing HEAD to the default branch is what was meant in
    # either case.
    $default = (& git -C $root symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>$null)
    $branch = if ($default) { $default -replace '^origin/', '' } else { 'main' }
    & git -C $root push origin "HEAD:$branch"
    if ($LASTEXITCODE -ne 0) { throw "pushing HEAD to origin/$branch failed" }
    & git -C $root push origin "v$Version"
    if ($LASTEXITCODE -ne 0) { throw "pushing v$Version failed" }
}

function Publish-InteropFixture {
    <#
    .SYNOPSIS
        Builds this version's interop fixture and commits it.

    .DESCRIPTION
        `tests/interop/<version>/` holds a database written by that release's
        own binary, and `crates/inillucent-compat/tests/release_format.rs` opens
        every one of them with the build under test. It is the only check that
        answers "can today's engine still read what we shipped two releases
        ago", and it can only answer it if the directory has a row for every
        release.

        **It runs here rather than in the version phase because the fixture is
        built from the published archive.** The binary is downloaded from the
        GitHub release, verified against `SHA256SUMS` and its minisign
        signature, and run - so the release has to exist first. That is also why
        the fixture cannot be part of the version commit the tag points at: it
        does not exist until after the tag is pushed. It goes in a commit of its
        own, on the same branch, immediately afterwards.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    & (Join-Path $root 'tools/build-interop-fixture.ps1') -Version $Version
    if ($LASTEXITCODE -ne 0) { throw "building the interop fixture for $Version failed" }
    $fixture = "tests/interop/$Version"
    & git -C $root add -- $fixture
    $staged = & git -C $root diff --cached --name-only
    if (-not $staged) {
        Write-Host "  tests/interop/$Version was already committed"
        return
    }
    & git -C $root commit -m "inillucent $Version interop fixture"
    if ($LASTEXITCODE -ne 0) { throw 'the interop fixture commit failed' }
    $default = (& git -C $root symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>$null)
    $branch = if ($default) { $default -replace '^origin/', '' } else { 'main' }
    & git -C $root push origin "HEAD:$branch"
    if ($LASTEXITCODE -ne 0) { throw "pushing the interop fixture to origin/$branch failed" }
}

function Test-InteropFixture {
    <#
    .SYNOPSIS
        Reports what this version's interop fixture holds.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    $directory = Join-Path $root "tests/interop/$Version"
    $database = Join-Path $directory 'app.rdb'
    $answers = Join-Path $directory 'expected.tsv'
    if (-not (Test-Path -LiteralPath $database)) { return "tests/interop/$Version/app.rdb was not written" }
    if (-not (Test-Path -LiteralPath $answers)) { return "tests/interop/$Version/expected.tsv was not written" }
    $recorded = @(Get-Content -LiteralPath $answers).Count
    $segments = @(Get-ChildItem -Path $directory -Filter 'app.rdb-wal.*').Count
    if ($segments -lt 1) { return "tests/interop/$Version holds no log segment" }
    return "ok: $recorded answers, $segments log segment(s)"
}

function Get-MirrorRepo {
    <#
    .SYNOPSIS
        The owner/name of the public repository, read off the `brl` remote.

    .DESCRIPTION
        **The release belongs where the published URLs point.** This checkout has two remotes:
        `origin` is jasonmcaffee/inillucent, where the work happens, and `brl` is
        Black-Rainbow-Labs/Inillucent, which is what inillucent.com links, what the Go module path
        `github.com/Black-Rainbow-Labs/Inillucent/packages/go` resolves through, and the account the
        product is published under. `gh` with no `--repo` uses the current directory's `origin`, so
        every release would have landed on the development repository while the public one showed
        nothing newer than 0.1.2.
    #>
    $url = (& git -C $script:Root remote get-url brl 2>$null)
    if (-not $url) { throw 'this checkout has no `brl` remote, so the public repository is unknown.' }
    if ($url -notmatch 'github\.com[:/](?<owner>[^/]+)/(?<name>[^/.]+)') { throw "the brl remote is $url, which is not a GitHub URL." }
    return "$($Matches.owner)/$($Matches.name)"
}

function Invoke-ReleaseTests {
    <#
    .SYNOPSIS
        Runs the suite, and returns the sentence to put in the release notes.

    .DESCRIPTION
        Empty when the suite ran and passed. A sentence when -SkipTests was given, which the GitHub
        release notes then carry. Anything else throws, because a release is not cut over a red
        suite.

        **The three exit codes are three different answers** (task-2047), and collapsing them is the
        confusion the runner was changed to remove:

          0  every selected target ran and passed
          1  the run happened and was red - a target failed, or --strict found a suite whose
             prerequisite was absent
          2  the run did not happen. The build failed, or cargo could not say what it had built.
             Nothing was graded, so nothing in that run may be read as a pass

        A `2` is reported as a run that did not happen rather than as a failing test, because they
        need different things done about them and an agent read one as the other once already.

        **--strict rather than a plain run.** Several suites report success when a prerequisite is
        absent, which is correct for a fresh clone and wrong for a release: it is exactly how a
        release goes out with the binding conformance suite, the oracle-graded suites and the live
        server suites all reporting green having run nothing.

    .PARAMETER Root
        The checkout to run in.

    .PARAMETER Skip
        Whether the run was waived.
    #>
    param([string] $Root, [bool] $Skip)

    if ($Skip) {
        $sentence = 'This release was published without running the test suite.'
        Write-Host "   $sentence" -ForegroundColor Yellow
        Write-Host '   It will be said again in the GitHub release notes.' -ForegroundColor Yellow
        return $sentence
    }

    # The runner is behind `required-features = ["testrun"]`, so it is built here rather than
    # assumed. A release that cannot build its own test runner is not one to publish.
    Write-Host '   building inillucent-testrun'
    & cargo build --manifest-path (Join-Path $Root 'Cargo.toml') -p inillucent-compat --bin inillucent-testrun --features testrun
    if ($LASTEXITCODE -ne 0) { throw 'inillucent-testrun would not build, so the suite could not be run. Fix the build, or pass -SkipTests and accept an untested release.' }

    $runner = Join-Path $Root 'target/debug/inillucent-testrun.exe'
    if (-not (Test-Path -LiteralPath $runner)) {
        # A worktree redirects CARGO_TARGET_DIR through its own .cargo/config.toml, so the binary
        # is not under the checkout at all. Ask cargo where it put it rather than guessing.
        $located = & cargo metadata --manifest-path (Join-Path $Root 'Cargo.toml') --format-version 1 --no-deps 2>$null |
            ConvertFrom-Json
        $targetDir = if ($located) { $located.target_directory } else { $null }
        if ($targetDir) { $runner = Join-Path $targetDir 'debug/inillucent-testrun.exe' }
    }
    if (-not (Test-Path -LiteralPath $runner)) { throw "inillucent-testrun built and then could not be found. Looked at $runner." }

    Write-Host "   $runner --strict"
    & $runner --strict
    $code = $LASTEXITCODE
    switch ($code) {
        0 { Write-Host '   the suite ran and passed.' -ForegroundColor Green; return '' }
        1 { throw 'the suite is red: a target failed, or --strict named a suite whose prerequisite was absent. A release is not cut over it. Read the run above, fix it, and run this again.' }
        2 { throw 'the run did not happen - the build failed, a selection matched nothing, or cargo could not say what it had built. Nothing was graded, so this is not evidence of anything. It is not a failing test and should not be treated as one.' }
        default { throw "inillucent-testrun answered $code, which is not one of its three exit codes. Read the run above." }
    }
}

function Publish-GitHubRelease {
    <#
    .SYNOPSIS
        Creates or updates the GitHub release with every artifact in dist/.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    # **The notary's container is not a download.** `rcodesign` zips the macOS binaries to upload
    # them to Apple, and `publish-site.ps1` ships the .pkg and the .tar.gz instead. Attaching the
    # zip would offer a third macOS file that no page links and no checksum covers.
    $assets = @(Get-ChildItem -LiteralPath $script:Dist -File |
        Where-Object { $_.Name -like "*$Version*" -or $_.Name -like 'SHA256SUMS*' } |
        Where-Object { $_.Name -notlike '*-apple-darwin.zip' } |
        ForEach-Object { $_.FullName })
    if ($assets.Count -eq 0) { throw "dist/ holds no artifact naming $Version" }

    $repo = @('--repo', (Get-MirrorRepo))
    $exists = & gh release view "v$Version" @repo --json tagName 2>$null
    if ($exists) {
        & gh release upload "v$Version" @assets @repo --clobber
    } else {
        # The untested sentence rides in the notes rather than being printed once on the machine
        # that cut the release, because the people who need it are the ones downloading the file.
        $notes = if ($script:UntestedNote) { "inillucent $Version`n`n$script:UntestedNote" } else { "inillucent $Version" }
        & gh release create "v$Version" @assets @repo --title "inillucent $Version" --notes $notes
    }
    if ($LASTEXITCODE -ne 0) { throw "the GitHub release for v$Version failed" }

    # **A draft is not a release, and uploading into one says nothing (task-1995).** `gh release
    # view` finds a draft, so the branch above quietly puts every asset into it and reports success
    # while the release stays invisible and untagged. inillucent 0.1.3 had a draft open from
    # 2026-09-15 and the CHANGELOG recorded the release as "tagged and not published" for four days.
    # Asked and fixed, rather than assumed: a draft is published, and a release that is already
    # public is left alone.
    $draft = & gh release view "v$Version" @repo --json isDraft --jq '.isDraft' 2>$null
    if ($draft -eq 'true') {
        & gh release edit "v$Version" @repo --draft=false
        if ($LASTEXITCODE -ne 0) { throw "v$Version was uploaded but could not be published" }
    }
}

function Publish-GoModule {
    <#
    .SYNOPSIS
        Tags the Go module, which is how a Go module is published.

    .DESCRIPTION
        There is no upload: `proxy.golang.org` fetches the tag. The tag is prefixed with the module's
        directory because the module is a subdirectory of the repository.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    $tag = "packages/go/v$Version"
    # **On the mirror, at the mirror's own release commit (task-1995).** The module path is
    # `github.com/Black-Rainbow-Labs/Inillucent/packages/go`, so proxy.golang.org reads this tag off
    # the mirror and nowhere else - pushing it to `origin` published nothing, and inillucent 0.1.3
    # sat on the site and on PyPI while `go get` still resolved 0.1.2.
    #
    # It points at the mirror's v<version> commit rather than at a local one. The mirror is one
    # commit per release, built from the release tag's tree and checked to equal it, so that commit
    # carries the same packages/go as the tag does. Pushing a local tag instead pushes the local
    # commit with it, which is how the 0.1.0, 0.1.1 and 0.1.2 Go tags came to point at development
    # history inside a repository whose whole design is one commit per release.
    $mirrorCommit = (& git -C $root ls-remote brl "refs/tags/v$Version^{}" | ForEach-Object { ($_ -split '\s+')[0] } | Select-Object -First 1)
    if (-not $mirrorCommit) {
        $mirrorCommit = (& git -C $root ls-remote brl "refs/tags/v$Version" | ForEach-Object { ($_ -split '\s+')[0] } | Select-Object -First 1)
    }
    if (-not $mirrorCommit) { throw "the mirror has no v$Version tag yet, so the Go module cannot be tagged. Run the mirror route first." }
    & git -C $root push brl "${mirrorCommit}:refs/tags/$tag"
    if ($LASTEXITCODE -ne 0) { throw "pushing $tag to the mirror failed" }
}

# ---------------------------------------------------------------------------
# Phase 4's checks: the destination is asked, rather than the command believed.
# ---------------------------------------------------------------------------

function Test-GitHubRelease {
    <#
    .SYNOPSIS
        Whether the release exists and carries assets.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    $repo = Get-MirrorRepo
    $names = & gh release view "v$Version" --repo $repo --json assets --jq '.assets[].name' 2>$null
    if (-not $names) { return "$repo has no assets on the v$Version release" }
    return $null
}

function Get-SitePlatforms {
    <#
    .SYNOPSIS
        Every platform this release ships, and the artifact each one is downloaded as.

    .DESCRIPTION
        **The list is here so a platform cannot quietly stop being offered.** Checking that every
        name in SHA256SUMS resolves catches a missing file and not a download page that dropped a
        row, because the page is never consulted. A platform that is not built belongs out of this
        table rather than reported missing on every release.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    return @(
        @{ Platform = 'Windows x86-64';   File = "inillucent-$Version-x86_64-pc-windows-msvc.zip" },
        @{ Platform = 'macOS installer';  File = "inillucent-$Version.pkg" },
        @{ Platform = 'macOS archive';    File = "inillucent-$Version-universal-apple-darwin.tar.gz" },
        @{ Platform = 'Linux x86-64';     File = "inillucent-$Version-x86_64-unknown-linux-gnu.tar.gz" },
        @{ Platform = 'Linux aarch64';    File = "inillucent-$Version-aarch64-unknown-linux-gnu.tar.gz" },
        @{ Platform = 'Debian x86-64';    File = "inillucent_${Version}_amd64.deb" },
        @{ Platform = 'Debian aarch64';   File = "inillucent_${Version}_arm64.deb" },
        @{ Platform = 'Fedora x86-64';    File = "inillucent-$Version.x86_64.rpm" },
        @{ Platform = 'Fedora aarch64';   File = "inillucent-$Version.aarch64.rpm" }
    )
}

function Test-SiteVersion {
    <#
    .SYNOPSIS
        Whether inillucent.com offers this release, on every platform it ships.

    .DESCRIPTION
        **The site is where a person actually gets the software**, so a release that reached GitHub
        and the registries and left the site behind is a release most people cannot get. Four things
        are asked, and each one has been wrong at least once:

        1. `downloads/VERSION` names this release.
        2. The **home page links an artifact for every platform in the table.** A page that stopped
           offering a platform passes every check that only reads SHA256SUMS.
        3. Every name in the published SHA256SUMS is served. A name with nothing behind it reads, to
           anyone running `sha256sum -c`, exactly like a download that was tampered with.
        4. What is served is **this build**: each artifact's Content-Length matches the file in
           dist/, and the smallest one is fetched and hashed in full. A 200 says a file is there, not
           that it is this release's file - a stale artifact of the right name passes a HEAD.

        Hashing all nine would pull about 240 MB on every release. Length catches a truncated or
        stale file, the full SHA256SUMS is published for anyone who wants certainty, and one artifact
        is hashed end to end so the published checksums are known to describe what is served.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    $base = 'https://inillucent.com/downloads'

    try {
        $served = (Invoke-WebRequest -Uri "$base/VERSION" -UseBasicParsing -TimeoutSec 30).Content.Trim()
    } catch {
        return "inillucent.com/downloads/VERSION could not be read: $($_.Exception.Message)"
    }
    if ($served -ne $Version) { return "inillucent.com serves $served, not $Version" }

    try {
        $page = (Invoke-WebRequest -Uri 'https://inillucent.com/' -UseBasicParsing -TimeoutSec 30).Content
    } catch {
        return "inillucent.com could not be read: $($_.Exception.Message)"
    }
    $unlinked = @(Get-SitePlatforms -Version $Version | Where-Object { $page -notlike "*$($_.File)*" })
    if ($unlinked.Count -gt 0) {
        return "the download page offers no $(($unlinked | ForEach-Object { $_.Platform }) -join ', ')"
    }

    try {
        $sums = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing -TimeoutSec 30).Content
    } catch {
        return "inillucent.com/downloads/SHA256SUMS could not be read: $($_.Exception.Message)"
    }

    $absent = @()
    $wrongSize = @()
    $smallest = $null
    foreach ($line in ($sums -split "`n")) {
        $name = ($line -split '\s+', 2)[1]
        if (-not $name) { continue }
        $name = $name.Trim()
        $local = Join-Path $script:Dist $name
        try {
            $head = Invoke-WebRequest -Uri "$base/$name" -Method Head -UseBasicParsing -TimeoutSec 30
            if ([int] $head.StatusCode -ne 200) { $absent += $name; continue }
            if (Test-Path -LiteralPath $local) {
                $expected = (Get-Item -LiteralPath $local).Length
                $actual = [int64] $head.Headers['Content-Length'][0]
                if ($actual -ne $expected) { $wrongSize += "$name is $actual bytes, built as $expected" }
                if (-not $smallest -or $expected -lt $smallest.Size) {
                    $smallest = @{ Name = $name; Size = $expected; Sum = ($line -split '\s+', 2)[0] }
                }
            }
        } catch {
            $absent += $name
        }
    }
    if ($absent.Count -gt 0) { return "SHA256SUMS names $($absent.Count) file(s) inillucent.com does not serve: $($absent -join ', ')" }
    if ($wrongSize.Count -gt 0) { return "served artifacts differ from the build: $($wrongSize -join '; ')" }

    if ($smallest) {
        $scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("inillucent-verify-" + [guid]::NewGuid().ToString('N'))
        try {
            Invoke-WebRequest -Uri "$base/$($smallest.Name)" -OutFile $scratch -UseBasicParsing -TimeoutSec 300
            $hash = (Get-FileHash -LiteralPath $scratch -Algorithm SHA256).Hash.ToLower()
            if ($hash -ne $smallest.Sum) {
                return "$($smallest.Name) is served with SHA-256 $hash, and SHA256SUMS says $($smallest.Sum)"
            }
        } catch {
            return "$($smallest.Name) could not be fetched to hash: $($_.Exception.Message)"
        } finally {
            Remove-Item -LiteralPath $scratch -Force -ErrorAction SilentlyContinue
        }
    }
    return $null
}

function Test-Registry {
    <#
    .SYNOPSIS
        Whether a registry's own metadata names this version.

    .DESCRIPTION
        A substring match over the response rather than a parse per registry: four registries answer
        four shapes, and what is being asked is only whether the version is in there at all.

    .PARAMETER Url
        The metadata endpoint.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Url, [string] $Version)
    # **Retried, because these registries are eventually consistent (task-1995).** npm answers
    # `Your package is being processed and may take a few minutes to become available` on the
    # publish itself, and PyPI's JSON and crates.io's index lag their uploads too. Asking once, the
    # instant the upload returned, reported "does not name 0.1.5 yet" for six npm packages that had
    # all published successfully - a report that says a route failed when it did not is worse than
    # no report, because the obvious next move is to publish again.
    $deadline = (Get-Date).AddSeconds(180)
    $last = $null
    while ($true) {
        try {
            $body = (Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 30).Content
            if ($body -match [regex]::Escape($Version)) { return $null }
            $last = "$Url does not name $Version yet"
        } catch {
            $last = "$Url could not be read: $($_.Exception.Message)"
        }
        if ((Get-Date) -gt $deadline) { return "$last (asked for 3 minutes)" }
        Start-Sleep -Seconds 10
    }
}

# ---------------------------------------------------------------------------
# The release.
# ---------------------------------------------------------------------------

$current = Get-WorkspaceVersion -Root $root
if ($Part) { $Version = Get-NextVersion -Current $current -Which $Part }
if (-not $Version) { $Version = $current }

Write-Host "inillucent $Version" -ForegroundColor Green
if ($Version -ne $current) { Write-Host "  (from $current)" }
if ($WhatIf) { Write-Host '  -WhatIf: nothing will be written' -ForegroundColor Yellow }

# **Backwards is refused (task-1995).** The version phase runs whatever `-Only` says, because every
# later route reads the version it writes. So `ship.ps1 -Only github -Version 0.1.3`, the obvious way
# to attach an asset to a release that already exists, rewrites Cargo.toml, the npm wrapper, the
# Python package and four more files to 0.1.3 on a tree that is on 0.1.4 - and the first thing the
# publish phase does with that is commit it. Re-publishing an old release is a real thing to want;
# doing it from this script is not, because this script's second phase is "write the new version
# everywhere".
$comparable = { param($v) [version]($v -replace '[^0-9.].*$', '') }
if ($Version -and -not $WhatIf) {
    if ((& $comparable $Version) -lt (& $comparable $current)) {
        throw "this checkout is on $current and -Version says $Version. The version phase would rewrite every version file backwards, and the publish phase would commit that. To add to an already published release, run the step itself: packaging/publish-site.ps1, or gh release upload."
    }
}

$dirty = & git -C $root status --porcelain
if ($dirty -and -not $AllowDirty -and -not $WhatIf) {
    throw "the working tree has uncommitted changes. A release built from one cannot be rebuilt. Commit them, or pass -AllowDirty."
}

Write-Phase 'preflight'
Import-SigningSecrets
$routes = Get-Routes -Version $Version
$plan = @()
foreach ($route in $routes) {
    $reason = & $route.Needs
    $wanted = (-not $Only -or $Only -contains $route.Name) -and (-not $Skip -or $Skip -notcontains $route.Name)
    $state = if (-not $wanted) { 'not asked for' } elseif ($reason) { $reason } else { $null }
    $plan += @{ Route = $route; Blocked = $state }
    $mark = if ($state) { 'skip' } else { 'run ' }
    $detail = if ($state) { " - $state" } else { '' }
    if (-not $state -and $route.Name -eq 'npm' -and $script:NpmAccount) {
        $detail = " - as $script:NpmAccount"
    } elseif (-not $state -and $route.Name -eq 'github' -and $script:GitHubAccount) {
        $detail = " - as $script:GitHubAccount"
    }
    Write-Host ("   [{0}] {1,-16} {2}{3}" -f $mark, $route.Name, $route.What, $detail)
}

Write-Phase 'tests'
if ($WhatIf) {
    # The plan says what would run, and running the suite is not a plan. What it prints is the one
    # thing a reader of `-WhatIf` needs: whether this release would be graded.
    if ($SkipTests) {
        Write-Host '   would NOT run the suite (-SkipTests), and would say so in the release notes.' -ForegroundColor Yellow
    } else {
        Write-Host '   would run inillucent-testrun --strict, and refuse the release on any non-zero exit.'
    }
} else {
    $script:UntestedNote = Invoke-ReleaseTests -Root $root -Skip ([bool]$SkipTests)
}

if ($WhatIf) {
    Write-Phase 'version'
    Set-ReleaseVersion -Version $Version -Previous $current
    Remove-SigningSecrets
    Write-Host ''
    Write-Host 'nothing was written.' -ForegroundColor Yellow
    return
}

Write-Phase 'version'
Set-ReleaseVersion -Version $Version -Previous $current

foreach ($entry in $plan) {
    $route = $entry.Route
    if ($entry.Blocked) {
        $script:Outcomes[$route.Name] = @{ State = 'skipped'; Why = $entry.Blocked }
        continue
    }
    Write-Phase $route.Name
    try {
        & $route.Run
        $why = if ($route.Verify) { & $route.Verify } else { $null }
        if ($why) {
            $script:Outcomes[$route.Name] = @{ State = 'failed'; Why = $why }
            Write-Warning $why
        } else {
            $script:Outcomes[$route.Name] = @{ State = 'published'; Why = '' }
        }
    } catch {
        $script:Outcomes[$route.Name] = @{ State = 'failed'; Why = $_.Exception.Message }
        Write-Warning "$($route.Name): $($_.Exception.Message)"
    }
}

Remove-SigningSecrets

Write-Phase "report - inillucent $Version"
foreach ($name in $script:Outcomes.Keys) {
    $outcome = $script:Outcomes[$name]
    $colour = switch ($outcome.State) {
        'published' { 'Green' }
        'skipped' { 'Yellow' }
        default { 'Red' }
    }
    $line = "   {0,-16} {1,-10} {2}" -f $name, $outcome.State, $outcome.Why
    Write-Host $line -ForegroundColor $colour
}

$failed = @($script:Outcomes.Values | Where-Object { $_.State -eq 'failed' })
Write-Host ''
if ($script:UntestedNote) {
    Write-Host $script:UntestedNote -ForegroundColor Yellow
}
if ($failed.Count -gt 0) {
    Write-Host "$($failed.Count) route(s) failed. Re-run just those with -Only." -ForegroundColor Red
    exit 1
}
Write-Host 'every route that could run, ran.' -ForegroundColor Green
