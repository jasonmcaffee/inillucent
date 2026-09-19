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
        2. version     write the new version into every file that carries it, in one step, and
                       refuse to continue if a seventh file is found holding the old one.
        3. build       compile, sign, package, notarise. Local; nothing has left the machine.
        4. publish     tag, push, GitHub, the mirror, the site, the registries.
        5. report      route by route: published, skipped or failed, and why.

    Preflight is first and separate because a tag is the one step that cannot be taken back quietly.
    Nothing reaches it until the script knows which routes will run.

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
    [switch] $AllowDirty,
    [switch] $WhatIf
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'stage-layout.ps1')

$script:Outcomes = [ordered]@{}
$script:Dist = Join-Path $root 'dist'

# **Script scope, and that is not a style choice.** Each route's `Needs`, `Run` and `Verify` is a
# scriptblock held in a table and invoked long after `Get-Routes` has returned. PowerShell resolves a
# scriptblock's free variables when it runs, not where it was written, so a local of `Get-Routes` is
# gone by then and reads as $null - which `Join-Path` then refuses with "Cannot bind argument to
# parameter 'Path' because it is null", from inside preflight, naming nothing useful.
$script:CrossBin = Join-Path $root 'tools/cross/bin'
$script:SitePath = Join-Path (Split-Path -Parent $root) 'inillucent-site'
$script:TapPath = Join-Path (Split-Path -Parent $root) 'homebrew-inillucent'
$script:Packaging = $PSScriptRoot
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
            Pattern = '(?m)^(    "@inillucent/cli-[a-z0-9-]+": ")[^"]+(")'
            Replace = "`${1}$Version`${2}"
            Check   = "@inillucent/cli-win32-x64"": ""$escaped"""
            What    = 'the five platform packages the wrapper pins'
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
    $ignore = @('CHANGELOG.md', 'Cargo.lock', 'tasks/', 'docs/', 'dist/', 'packages/go/')
    $hits = & git -C $root grep -l --fixed-strings -- $Previous 2>$null
    $unexpected = @($hits | Where-Object {
            $path = $_
            $path -notlike '*.md' -and
            $carriers -notcontains $path -and
            -not ($ignore | Where-Object { $path -like "$_*" })
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
        @{
            Name   = 'github'
            What   = 'the GitHub release, with every asset'
            Needs  = {
                if (-not (Test-Tool 'gh')) { return 'gh is not installed. https://cli.github.com' }
                $null
            }
            Run    = { Publish-GitHubRelease -Version $Version }
            Verify = { Test-GitHubRelease -Version $Version }
        },
        @{
            Name  = 'mirror'
            What  = 'the public source mirror'
            Needs = { $null }
            Run   = { & (Join-Path $script:Packaging 'mirror-github.ps1') -Version $Version -Push -Verify }
        },
        @{
            Name   = 'site'
            What   = 'inillucent.com: the artifacts, then the links'
            Needs  = {
                if (-not (Test-Path -LiteralPath $script:SitePath)) { return "$script:SitePath does not exist." }
                $null
            }
            Run    = {
                & (Join-Path $script:Packaging 'publish-site.ps1') -Version $Version -Stage
                & (Join-Path $script:Packaging 'publish-site.ps1') -Version $Version -Link
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
            Run    = { & (Join-Path $script:Packaging 'cargo-publish.ps1') -Execute }
            Verify = { Test-Registry -Url "https://crates.io/api/v1/crates/inillucent" -Version $Version }
        },
        @{
            Name   = 'npm'
            What   = 'the npm wrapper and its four platform packages'
            Needs  = {
                if (-not (Test-Tool 'npm')) { return 'npm is not installed.' }
                if (-not $env:NPM_TOKEN -and -not (Test-Path -LiteralPath (Join-Path $HOME '.npmrc'))) {
                    return 'no npm credential: NPM_TOKEN is unset and there is no ~/.npmrc.'
                }
                $null
            }
            Run    = { & node (Join-Path $script:Root 'packages/npm/build.mjs') --publish }
            Verify = { Test-Registry -Url "https://registry.npmjs.org/inillucent" -Version $Version }
        },
        @{
            Name   = 'pypi'
            What   = 'the Python wheel'
            Needs  = {
                if (-not $env:TWINE_PASSWORD -and -not (Test-Path -LiteralPath (Join-Path $HOME '.pypirc'))) {
                    return 'no PyPI credential: TWINE_PASSWORD is unset and there is no ~/.pypirc.'
                }
                $null
            }
            Run    = { & python (Join-Path $script:Root 'packages/python/build.py') --publish }
            Verify = { Test-Registry -Url "https://pypi.org/pypi/inillucent/json" -Version $Version }
        },
        @{
            Name   = 'go'
            What   = 'the Go module tag'
            Needs  = { $null }
            Run    = { Publish-GoModule -Version $Version }
            Verify = { Test-Registry -Url "https://proxy.golang.org/github.com/black-rainbow-labs/inillucent/packages/go/@latest" -Version $Version }
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
            Run   = { & bash (Join-Path $script:Packaging 'homebrew/update.sh') --tap $script:TapPath }
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
    $paths = @('Cargo.toml', 'Cargo.lock', 'packages/npm/inillucent/package.json',
        'packages/python/pyproject.toml', 'packaging/homebrew/inillucent.rb')
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
    & git -C $root push
    & git -C $root push origin "v$Version"
    if ($LASTEXITCODE -ne 0) { throw "pushing v$Version failed" }
}

function Publish-GitHubRelease {
    <#
    .SYNOPSIS
        Creates or updates the GitHub release with every artifact in dist/.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    $assets = @(Get-ChildItem -LiteralPath $script:Dist -File |
        Where-Object { $_.Name -like "*$Version*" -or $_.Name -like 'SHA256SUMS*' } |
        ForEach-Object { $_.FullName })
    if ($assets.Count -eq 0) { throw "dist/ holds no artifact naming $Version" }

    $exists = & gh release view "v$Version" --json tagName 2>$null
    if ($exists) {
        & gh release upload "v$Version" @assets --clobber
    } else {
        & gh release create "v$Version" @assets --title "inillucent $Version" --notes "inillucent $Version"
    }
    if ($LASTEXITCODE -ne 0) { throw "the GitHub release for v$Version failed" }
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
    if (-not (& git -C $root tag --list $tag)) {
        & git -C $root tag -a $tag -m "inillucent Go module $Version"
        if ($LASTEXITCODE -ne 0) { throw "tagging $tag failed" }
    }
    & git -C $root push origin $tag
    if ($LASTEXITCODE -ne 0) { throw "pushing $tag failed" }
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
    $names = & gh release view "v$Version" --json assets --jq '.assets[].name' 2>$null
    if (-not $names) { return "gh release view v$Version lists no assets" }
    return $null
}

function Test-SiteVersion {
    <#
    .SYNOPSIS
        Whether the live site says it serves this version.

    .PARAMETER Version
        The version being released.
    #>
    param([string] $Version)
    try {
        $served = (Invoke-WebRequest -Uri 'https://inillucent.com/downloads/VERSION' -UseBasicParsing -TimeoutSec 30).Content.Trim()
    } catch {
        return "inillucent.com/downloads/VERSION could not be read: $($_.Exception.Message)"
    }
    if ($served -ne $Version) { return "inillucent.com serves $served, not $Version" }
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
    try {
        $body = (Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 30).Content
    } catch {
        return "$Url could not be read: $($_.Exception.Message)"
    }
    if ($body -notmatch [regex]::Escape($Version)) { return "$Url does not name $Version yet" }
    return $null
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

$dirty = & git -C $root status --porcelain
if ($dirty -and -not $AllowDirty -and -not $WhatIf) {
    throw "the working tree has uncommitted changes. A release built from one cannot be rebuilt. Commit them, or pass -AllowDirty."
}

Write-Phase 'preflight'
$routes = Get-Routes -Version $Version
$plan = @()
foreach ($route in $routes) {
    $reason = & $route.Needs
    $wanted = (-not $Only -or $Only -contains $route.Name) -and (-not $Skip -or $Skip -notcontains $route.Name)
    $state = if (-not $wanted) { 'not asked for' } elseif ($reason) { $reason } else { $null }
    $plan += @{ Route = $route; Blocked = $state }
    $mark = if ($state) { 'skip' } else { 'run ' }
    $detail = if ($state) { " - $state" } else { '' }
    Write-Host ("   [{0}] {1,-16} {2}{3}" -f $mark, $route.Name, $route.What, $detail)
}

if ($WhatIf) {
    Write-Phase 'version'
    Set-ReleaseVersion -Version $Version -Previous $current
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
if ($failed.Count -gt 0) {
    Write-Host "$($failed.Count) route(s) failed. Re-run just those with -Only." -ForegroundColor Red
    exit 1
}
Write-Host 'every route that could run, ran.' -ForegroundColor Green
