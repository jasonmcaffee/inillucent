<#
.SYNOPSIS
    Installs inillucent for the current user on Windows.

.DESCRIPTION
    Downloads the release archive for this machine, checks its SHA-256 against
    the release's SHA256SUMS, unpacks it into %LOCALAPPDATA%\Programs\inillucent
    and puts that directory's bin on the user PATH.

    No administrator rights, no service, no registry beyond the user's own PATH
    value, and nothing outside one directory. An MSI is deliberately not the
    first move - packaging/windows/README.md records what one would take and
    why it buys nothing here.

    One line, from a fresh machine:

        irm https://inillucent.com/downloads/install.ps1 | iex

.PARAMETER Version
    A specific release to install. Defaults to the latest.

.PARAMETER FromDist
    Install the archive in dist/ instead of downloading. This is how the script
    is tested before a release exists, and how a build from source is installed.

.PARAMETER Prefix
    Where to install. Defaults to %LOCALAPPDATA%\Programs\inillucent.

.PARAMETER NoPath
    Do not touch the user PATH.

.PARAMETER Uninstall
    Remove what this script installed, PATH entry included.

.EXAMPLE
    pwsh packaging/install.ps1 -FromDist
    pwsh packaging/install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $BaseUrl = 'https://inillucent.com/downloads',
    [switch] $FromDist,
    [string] $Prefix,
    [switch] $NoPath,
    [switch] $Uninstall
)

$ErrorActionPreference = 'Stop'
if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA 'Programs\inillucent' }
$binaries = Join-Path $Prefix 'bin'

function Add-ToUserPath {
    <#
    .SYNOPSIS
        Puts a directory on the user PATH, once.

    .DESCRIPTION
        The user PATH, not the machine one: this installs for one person and
        needs no elevation, and a machine PATH edit from a script that did not
        ask for administrator rights would silently do nothing anyway.

    .PARAMETER Directory
        The directory to add.
    #>
    param([string] $Directory)
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @()
    if ($current) { $parts = $current -split ';' | Where-Object { $_ -ne '' } }
    if ($parts -contains $Directory) {
        Write-Host "PATH already has $Directory"
        return
    }
    $parts += $Directory
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
    # And for this session, so the caller can run it without opening a new
    # terminal - which is the first thing anybody tries.
    $env:Path = "$env:Path;$Directory"
    Write-Host "added $Directory to your PATH (new terminals will see it)"
}

function Remove-FromUserPath {
    <#
    .SYNOPSIS
        Takes a directory back off the user PATH.

    .PARAMETER Directory
        The directory to remove.
    #>
    param([string] $Directory)
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { return }
    $parts = $current -split ';' | Where-Object { $_ -ne '' -and $_ -ne $Directory }
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

function Get-Architecture {
    <#
    .SYNOPSIS
        Returns the Rust target triple this machine's releases are built for.
    #>
    switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
        'X64'   { return 'x86_64-pc-windows-msvc' }
        'Arm64' { return 'aarch64-pc-windows-msvc' }
        default {
            throw "there is no inillucent release for $_ yet. Build from source: cargo install inillucent-cli"
        }
    }
}

function Get-LatestVersion {
    <#
    .SYNOPSIS
        Reads which release is current from VERSION, beside the archives.

    .DESCRIPTION
        One file on the same host as everything else here, rather than a second
        service that has to be reachable and can answer differently. The GitHub
        API is not usable for this anyway: the repository is private, so an
        unauthenticated request for it answers 404.
    #>
    return ((Invoke-WebRequest -Uri "$BaseUrl/VERSION" -UseBasicParsing).Content).Trim()
}

function Assert-Checksum {
    <#
    .SYNOPSIS
        Refuses an archive whose hash is not the one the release published.

    .DESCRIPTION
        The whole reason to publish SHA256SUMS. A downloader that skips this has
        turned a compromised or truncated transfer into an installed program,
        and it is four lines to not do that.

    .PARAMETER Archive
        The downloaded file.

    .PARAMETER Sums
        The SHA256SUMS text from the same release.
    #>
    param([string] $Archive, [string] $Sums)
    $name = Split-Path -Leaf $Archive
    $expected = $null
    foreach ($line in ($Sums -split "`n")) {
        $fields = ($line.Trim() -split '\s+')
        if ($fields.Count -ge 2 -and $fields[-1] -eq $name) { $expected = $fields[0].ToLower() }
    }
    if (-not $expected) { throw "SHA256SUMS does not list $name" }
    $actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        throw "$name does not match its published checksum.`n  expected $expected`n  got      $actual"
    }
    Write-Host "checksum ok ($expected)"
}

if ($Uninstall) {
    if (Test-Path -LiteralPath $Prefix) {
        Remove-Item -LiteralPath $Prefix -Recurse -Force -Confirm:$false
        Write-Host "removed $Prefix"
    } else {
        Write-Host "nothing installed at $Prefix"
    }
    Remove-FromUserPath -Directory $binaries
    Write-Host 'uninstalled.'
    return
}

$target = Get-Architecture
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("inillucent-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $temporary | Out-Null

try {
    if ($FromDist) {
        $dist = Join-Path (Split-Path -Parent $PSScriptRoot) 'dist'
        if (-not $Version) {
            $version = Get-Content -LiteralPath (Join-Path $dist "inillucent-*-$target/VERSION") -Raw -ErrorAction SilentlyContinue
            if (-not $version) {
                $found = Get-ChildItem -Path $dist -Filter "inillucent-*-$target.zip" | Select-Object -First 1
                if (-not $found) { throw "no archive for $target in $dist. Run packaging/release.ps1 first." }
                $Version = ($found.BaseName -replace "^inillucent-", '') -replace "-$target$", ''
            } else {
                $Version = $version.Trim()
            }
        }
        $archive = Join-Path $dist "inillucent-$Version-$target.zip"
        if (-not (Test-Path -LiteralPath $archive)) { throw "$archive does not exist. Run packaging/release.ps1 first." }
        $sums = Get-Content -LiteralPath (Join-Path $dist 'SHA256SUMS') -Raw
        Assert-Checksum -Archive $archive -Sums $sums
    } else {
        if (-not $Version) { $Version = Get-LatestVersion }
        $base = $BaseUrl.TrimEnd('/')
        $archive = Join-Path $temporary "inillucent-$Version-$target.zip"
        Write-Host "downloading inillucent $Version for $target..."
        Invoke-WebRequest -Uri "$base/inillucent-$Version-$target.zip" -OutFile $archive
        $sums = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing).Content
        Assert-Checksum -Archive $archive -Sums $sums
    }

    $unpacked = Join-Path $temporary 'unpacked'
    Expand-Archive -LiteralPath $archive -DestinationPath $unpacked -Force
    # The archive holds one directory named after the release; the layout under
    # it is what gets installed, so the prefix does not gain a version in its
    # path and an upgrade replaces rather than accumulating.
    $inner = Get-ChildItem -Path $unpacked -Directory | Select-Object -First 1
    if (-not $inner) { throw 'the archive is not shaped as expected: no directory inside it' }

    if (Test-Path -LiteralPath $Prefix) { Remove-Item -LiteralPath $Prefix -Recurse -Force -Confirm:$false }
    New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
    Copy-Item -Path (Join-Path $inner.FullName '*') -Destination $Prefix -Recurse -Force

    if (-not $NoPath) { Add-ToUserPath -Directory $binaries }

    Write-Host ''
    Write-Host "inillucent $Version is installed in $Prefix"
    & (Join-Path $binaries 'inillucent.exe') --version
    Write-Host ''
    Write-Host 'Try:'
    Write-Host '  inillucent create app.rdb'
    Write-Host '  inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"'
    Write-Host '  inillucent --db app.rdb query "SELECT * FROM notes"'
    Write-Host ''
    Write-Host 'To give an agent the same commands, add this to its MCP configuration:'
    Write-Host ''
    Write-Host '  "inillucent": {'
    Write-Host '    "type": "local",'
    # JSON wants each backslash doubled, and PowerShell's -replace takes its
    # replacement string literally, so one doubling written here is one
    # doubling in the output. Writing four produced four, which is a path no
    # client can open.
    $escaped = ($binaries + '\inillucent-mcp.exe').Replace('\', '\\')
    Write-Host ("    `"command`": [`"$escaped`", `"--db`", `"app.rdb`"],")
    Write-Host '    "enabled": true'
    Write-Host '  }'
} finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force -Confirm:$false
    }
}
