<#
.SYNOPSIS
    Puts a built release on inillucent.com.

.DESCRIPTION
    The site is the distribution point. The repository is private, so a GitHub
    release cannot be one: its assets are private too. inillucent.com already
    serves the Windows archive out of a static export, and this copies the rest
    of a release in beside it.

    It runs in two steps on purpose.

        -Stage   copies the artifacts, SHA256SUMS, its signature, the public key
                 and VERSION into the site's downloads directory. The files are
                 then fetchable, but nothing on the site links to them.
        -Link    rewrites the download section of the site's content so the
                 links appear.

    Staging first is what makes it possible to run the macOS smoke test against
    the published bytes - packaging/macos/verify-macos.sh downloads from the
    site - before any reader can find them. A download link pointing at an
    artifact nobody has run is worse than no link.

.PARAMETER Version
    The version to publish.

.PARAMETER Stage
    Copy the artifacts to the site without linking them.

.PARAMETER Link
    Rewrite the site's download entries to point at this version.

.PARAMETER SitePath
    The site folder. Defaults to `black-rainbow-labs-sites/sites/inillucent` beside this repository
    (task-2128 moved the site there).

.EXAMPLE
    pwsh packaging/publish-site.ps1 -Version 0.1.1 -Stage
    pwsh packaging/publish-site.ps1 -Version 0.1.1 -Link
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Version,
    [switch] $Stage,
    [switch] $Link,
    [string] $SitePath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# --------------------------------------------------------------------------
# The cmdlets this script cannot do its job without.
#
# An agent terminal can inherit a PSModulePath where the PowerShell 7 module
# directories shadow Microsoft.PowerShell.Utility. Cmdlets from it then resolve
# to nothing, the script carries on, and it exits 0 - so a SHA256SUMS comes out
# empty and the release reports success. Checked here, before anything is
# built, because a silent failure is only discovered by someone downloading the
# result.
# --------------------------------------------------------------------------
$required = @('Get-FileHash', 'Get-ChildItem', 'Set-Content', 'Copy-Item', 'ConvertTo-Json')
$absent = @($required | Where-Object { -not (Get-Command $_ -ErrorAction SilentlyContinue) })
if ($absent.Count -gt 0) {
    throw @"
these cmdlets are not resolvable in this session: $($absent -join ', ')
PSModulePath is shadowing the module they live in, and they would silently do nothing rather than
fail. Start pwsh with -NoProfile and a PSModulePath that reaches
C:\Program Files\PowerShell\7\Modules, then run this again.
"@
}

if (-not $Stage -and -not $Link) { throw 'pass -Stage, -Link, or both' }
if (-not $SitePath) { $SitePath = Join-Path (Split-Path -Parent $root) 'black-rainbow-labs-sites/sites/inillucent' }
if (-not (Test-Path -LiteralPath $SitePath)) {
    throw "$SitePath does not exist. Pass -SitePath."
}

$dist = Join-Path $root 'dist'
$downloads = Join-Path $SitePath 'public/downloads'

# What the site serves, and the label the download section shows for each. A
# missing artifact is reported rather than skipped: a release that quietly
# published three of five files is the failure this exists to prevent.
$artifacts = @(
    @{ Platform = 'Windows'; File = "inillucent-$Version-x86_64-pc-windows-msvc.zip"; Detail = 'x86-64' },
    @{ Platform = 'macOS'; File = "inillucent-$Version.pkg"; Detail = 'Apple silicon and Intel, signed and notarised' },
    @{ Platform = 'macOS archive'; File = "inillucent-$Version-universal-apple-darwin.tar.gz"; Detail = 'Apple silicon and Intel' },
    @{ Platform = 'Linux'; File = "inillucent-$Version-x86_64-unknown-linux-gnu.tar.gz"; Detail = 'x86-64, glibc 2.28 or newer' },
    @{ Platform = 'Linux ARM'; File = "inillucent-$Version-aarch64-unknown-linux-gnu.tar.gz"; Detail = 'aarch64, glibc 2.28 or newer' },
    @{ Platform = 'Debian and Ubuntu'; File = "inillucent_${Version}_amd64.deb"; Detail = 'x86-64' },
    @{ Platform = 'Fedora and RHEL'; File = "inillucent-$Version.x86_64.rpm"; Detail = 'x86-64' },
    # **aarch64 too (task-1995).** `linux/package-linux.ps1` builds four packages and this list
    # named two, so SHA256SUMS - which covers everything built - published hashes for an arm64 .deb
    # and an aarch64 .rpm that the site answered 404 for. The page already offers an ARM Linux
    # tar.gz, so the machines these are for were already being served, just not with a package.
    @{ Platform = 'Debian and Ubuntu ARM'; File = "inillucent_${Version}_arm64.deb"; Detail = 'aarch64' },
    @{ Platform = 'Fedora and RHEL ARM'; File = "inillucent-$Version.aarch64.rpm"; Detail = 'aarch64' }
)

function Get-Sha256 {
    <#
    .SYNOPSIS
        The lowercase SHA-256 of one file, which is what the site prints.

    .PARAMETER Path
        The file.
    #>
    param([string] $Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLower()
}

if ($Stage) {
    New-Item -ItemType Directory -Force -Path $downloads | Out-Null
    Write-Host "staging inillucent $Version into $downloads"

    $missing = @()
    foreach ($artifact in $artifacts) {
        $from = Join-Path $dist $artifact.File
        if (-not (Test-Path -LiteralPath $from)) { $missing += $artifact.File; continue }
        Copy-Item -LiteralPath $from -Destination $downloads -Force
        Write-Host "  $($artifact.File)"
    }

    # install.sh reads VERSION to find out what the current release is, and
    # SHA256SUMS to check what it downloaded.
    #
    # provenance.json is here because SHA256SUMS names it. release.ps1 puts it
    # in the sums so that verifying the sums verifies the provenance too, and
    # publishing the sums without the file they name leaves a line that cannot
    # be checked.
    #
    # **And only when SHA256SUMS names it (task-1995).** This copied it whenever dist/ had one, so
    # the reasoning above ran backwards: the sums file is what decides. `Update-Sha256Sums` leaves
    # out a provenance describing a different build, and copying it anyway is how the site came to
    # serve the 0.1.2 provenance beside the 0.1.3 downloads. A provenance left behind is also not
    # merely absent - it is a specific, wrong claim about how the thing next to it was built.
    $sumsText = ''
    $sumsFile = Join-Path $dist 'SHA256SUMS'
    if (Test-Path -LiteralPath $sumsFile) { $sumsText = Get-Content -LiteralPath $sumsFile -Raw }
    foreach ($extra in @('SHA256SUMS', 'SHA256SUMS.minisig', 'provenance.json')) {
        $from = Join-Path $dist $extra
        if ($extra -eq 'provenance.json' -and $sumsText -notmatch 'provenance\.json') {
            $stale = Join-Path $downloads 'provenance.json'
            if (Test-Path -LiteralPath $stale) {
                Write-Warning "$downloads\provenance.json describes an older release and SHA256SUMS does not name a new one. Delete it by hand, or run packaging/release.ps1 to write a provenance for this build."
            }
            Write-Host '  provenance.json skipped: SHA256SUMS does not name it'
            continue
        }
        if (Test-Path -LiteralPath $from) {
            Copy-Item -LiteralPath $from -Destination $downloads -Force
            Write-Host "  $extra"
        } else {
            Write-Warning "$extra is missing from dist/"
        }
    }
    $publicKey = Join-Path $PSScriptRoot 'inillucent.pub'
    if (Test-Path -LiteralPath $publicKey) {
        Copy-Item -LiteralPath $publicKey -Destination $downloads -Force
        Write-Host '  inillucent.pub'
    }
    Set-Content -Path (Join-Path $downloads 'VERSION') -Value $Version -NoNewline
    Write-Host '  VERSION'

    # The install scripts are fetched from the site as well, because the
    # repository they used to be fetched from is private and answers 404.
    foreach ($script in @('install.sh', 'install.ps1')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $script) -Destination $downloads -Force
        Write-Host "  $script"
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'macos/verify-macos.sh') -Destination $downloads -Force
    Write-Host '  verify-macos.sh'

    if ($missing.Count -gt 0) {
        Write-Host ''
        Write-Warning "not staged, because dist/ does not have them: $($missing -join ', ')"
    }
    Write-Host ''
    Write-Host 'staged but not linked. Verify on a Mac before linking:'
    Write-Host "  curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version $Version"
}

if ($Link) {
    $contentFile = Join-Path $SitePath 'src/data/content.ts'
    if (-not (Test-Path -LiteralPath $contentFile)) { throw "$contentFile does not exist" }

    $entries = @()
    foreach ($artifact in $artifacts) {
        $staged = Join-Path $downloads $artifact.File
        if (-not (Test-Path -LiteralPath $staged)) { continue }
        $entries += [pscustomobject]@{
            platform = $artifact.Platform
            detail   = "$($artifact.Detail) · $([math]::Round((Get-Item -LiteralPath $staged).Length / 1MB, 1)) MB"
            href     = "/downloads/$($artifact.File)"
            sha256   = Get-Sha256 -Path $staged
        }
    }
    if ($entries.Count -eq 0) { throw 'nothing is staged, so there is nothing to link' }

    $json = $entries | ConvertTo-Json -Depth 4 -Compress
    & node (Join-Path $PSScriptRoot 'site/update-downloads.mjs') $contentFile $json $Version
    if ($LASTEXITCODE -ne 0) { throw 'the download section could not be rewritten' }

    Write-Host ''
    Write-Host "linked $($entries.Count) download(s) in $contentFile"
    Write-Host 'now rebuild and deploy the site so the change is live.'
}
