<#
.SYNOPSIS
    Builds inillucent.com's static export and deploys it to Cloudflare and to the origin.

.DESCRIPTION
    **Staging the files is not publishing them.** `publish-site.ps1` copies the artifacts into the
    site checkout and rewrites the download list, and then prints "now rebuild and deploy the site so
    the change is live" - which is a person's job that nothing in a release does. Measured on the
    0.1.5 run: every artifact was staged and linked, and `inillucent.com/downloads/VERSION` still
    answered 0.1.3, because the site is a Next.js static export and it had not been rebuilt.

    `public/downloads/` is gitignored, so the artifacts do not travel through git at all: the build
    is what copies them into `out/`.

    Since task-2128 the site lives at `sites/inillucent` in the black-rainbow-labs-sites repository.
    Cloudflare serves its pages from Workers static assets, and the `brl-sites` origin on this machine
    serves `/downloads/` through Cloudflare's cache. `tools/deploy.mjs` in that repository uploads the
    export, purges any download whose bytes changed from the edge cache, waits for the origin to pick
    up the new export (it reloads by itself, so there is no restart), and checks every file at the
    public URL.

.PARAMETER SitePath
    The site folder. Defaults to `black-rainbow-labs-sites/sites/inillucent` beside this repository.

.EXAMPLE
    pwsh packaging/deploy-site.ps1
    pwsh packaging/deploy-site.ps1 -SitePath C:/jason/dev/black-rainbow-labs-sites/sites/inillucent
#>
[CmdletBinding()]
param(
    [string] $SitePath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $SitePath) { $SitePath = Join-Path (Split-Path -Parent $root) 'black-rainbow-labs-sites/sites/inillucent' }
if (-not (Test-Path -LiteralPath $SitePath)) { throw "$SitePath does not exist. Pass -SitePath." }
$sitesRepo = (Resolve-Path (Join-Path $SitePath '../..')).Path
$deploy = Join-Path $sitesRepo 'tools/deploy.mjs'
if (-not (Test-Path -LiteralPath $deploy)) { throw "$deploy does not exist. Is $SitePath inside the black-rainbow-labs-sites repository?" }

Write-Host "building $SitePath"
& npm --prefix $SitePath run build
if ($LASTEXITCODE -ne 0) { throw "the site build failed with $LASTEXITCODE" }

# The export has to carry the downloads, or the build succeeded and published nothing. Checked
# rather than assumed: `next build` is perfectly happy to produce an export with an empty
# public/downloads, and the only symptom is a 404 on the file the download page links.
$exported = Join-Path $SitePath 'out/downloads'
$count = @(Get-ChildItem -Path $exported -File -ErrorAction SilentlyContinue).Count
if ($count -lt 1) { throw "$exported is empty after the build, so nothing would be served." }
Write-Host "  $count files in out/downloads"

Write-Host "deploying with $deploy"
& node $deploy inillucent --skip-build
if ($LASTEXITCODE -ne 0) { throw "tools/deploy.mjs failed with $LASTEXITCODE. The site is built; run 'node $deploy inillucent --skip-build' again." }
