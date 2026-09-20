<#
.SYNOPSIS
    Builds inillucent.com's static export and restarts the service that serves it.

.DESCRIPTION
    **Staging the files is not publishing them.** `publish-site.ps1` copies the artifacts into the
    site checkout and rewrites the download list, and then prints "now rebuild and deploy the site so
    the change is live" - which is a person's job that nothing in a release does. Measured on the
    0.1.5 run: every artifact was staged and linked, and `inillucent.com/downloads/VERSION` still
    answered 0.1.3, because the site is a Next.js static export served by a small Rust binary out of
    `out/` and neither had been rebuilt.

    `public/downloads/` is gitignored, so the artifacts do not travel through git at all: the build
    is what copies them into `out/`, and the restart is what makes the running server see them.

    The service is found by name through the Service Manager rather than by an id written down here,
    because an id is per machine and a name is not.

.PARAMETER SitePath
    The inillucent-site checkout. Defaults to a sibling of this repository.

.PARAMETER ServiceName
    The Service Manager service that serves the site.

.PARAMETER ServiceManager
    The Service Manager's base URL.

.EXAMPLE
    pwsh packaging/deploy-site.ps1
    pwsh packaging/deploy-site.ps1 -SitePath C:/jason/dev/inillucent-site
#>
[CmdletBinding()]
param(
    [string] $SitePath,
    [string] $ServiceName = 'Inillucent Site',
    [string] $ServiceManager = 'http://127.0.0.1:4000'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $SitePath) { $SitePath = Join-Path (Split-Path -Parent $root) 'inillucent-site' }
if (-not (Test-Path -LiteralPath $SitePath)) { throw "$SitePath does not exist. Pass -SitePath." }

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

Write-Host "restarting '$ServiceName'"
try {
    $services = Invoke-RestMethod -Uri "$ServiceManager/api/services" -TimeoutSec 30
} catch {
    throw "the Service Manager at $ServiceManager could not be reached: $($_.Exception.Message). The site is built; restart '$ServiceName' by hand."
}
$list = if ($services -is [array]) { $services } else { $services.services }
$service = $list | Where-Object { $_.name -eq $ServiceName } | Select-Object -First 1
if (-not $service) { throw "the Service Manager knows no service named '$ServiceName'." }

# A restart can outlast a short client timeout, and the call is cancellable - a timeout kills the
# work the handler was doing rather than only the waiting. 120s is well past what this takes.
$answer = Invoke-RestMethod -Uri "$ServiceManager/api/services/$($service.id)/control" -Method Post `
    -ContentType 'application/json' -Body '{"action":"restart"}' -TimeoutSec 120
Write-Host "  $($service.name) is $($answer.status), pid $($answer.pid)"
