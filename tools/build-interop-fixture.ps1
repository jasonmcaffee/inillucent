<#
.SYNOPSIS
    Builds tests/interop/<version>/ with that release's own binary.

.DESCRIPTION
    A format change that nobody notices is one that costs an application its
    database. The only way to find out whether today's build can still read
    what 0.1.1 wrote is to have 0.1.1 write it, so this script downloads that
    release, verifies it, runs `tests/interop/build.sql` with it, and checks in
    what it produced.

    Five steps, and the order is the point:

    1. **Download** the release's Windows archive and its `SHA256SUMS` into
       `tools/cross/bin/releases/<version>/`, which is gitignored. A file that
       is already there is used as it stands unless -Force is passed.
    2. **Verify** the archive against `SHA256SUMS`, and `SHA256SUMS` against its
       minisign signature when the release published one. A release that fails
       either check is not run.
    3. **Build** by running `build.sql` with that release's `inillucent.exe`,
       then checkpointing, then writing one more row - so the fixture's newest
       row exists only in the log segment and a reader that ignores the log
       gives a different answer rather than the same one.
    4. **Ask** every question in `tests/interop/verify.sql` and record the
       answers as `expected.tsv`.
    5. **Publish** into `tests/interop/<version>/`, where
       `crates/inillucent-compat/tests/release_format.rs` reads them.

    `packaging/ship.ps1` calls this in its publish phase for the version being
    shipped, so the directory never lags a release.

.PARAMETER Version
    The released version to build a fixture from, such as 0.1.1.

.PARAMETER Repository
    The GitHub repository the release is published on.

.PARAMETER Force
    Download and extract again even when the files are already there.

.EXAMPLE
    pwsh tools/build-interop-fixture.ps1 -Version 0.1.1

.EXAMPLE
    pwsh tools/build-interop-fixture.ps1 -Version 0.1.7 -Force
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Version,
    [string] $Repository = 'Black-Rainbow-Labs/Inillucent',
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

. (Join-Path $root 'packaging/stage-layout.ps1')

function Get-ReleaseDirectory {
    <#
    .SYNOPSIS
        Where a downloaded release lives, beside the cross toolchain.

    .DESCRIPTION
        The same directory `Get-CrossBin` resolves, so a `git worktree` shares
        the copy the main checkout downloaded rather than fetching 20 MB of its
        own. It is gitignored for the same reason the toolchain is: it is
        downloaded, not source.

    .PARAMETER Root
        The repository root the caller is working in.

    .PARAMETER Version
        The release version.
    #>
    param([string] $Root, [string] $Version)
    $releases = Join-Path (Get-CrossBin -Root $Root) 'releases'
    return (Join-Path $releases $Version)
}

function Save-ReleaseAsset {
    <#
    .SYNOPSIS
        Downloads one asset of a release, or reports that it was not published.

    .DESCRIPTION
        Returns the path when the asset arrived and $null when the release does
        not have one. A missing asset is an answer rather than a failure: 0.1.1
        and 0.1.2 published no `SHA256SUMS.minisig`, because the release was not
        signed until 0.1.3, and refusing to build their fixtures over it would
        mean never testing the two oldest formats.

    .PARAMETER Repository
        The GitHub repository.

    .PARAMETER Version
        The release version.

    .PARAMETER Name
        The asset's file name.

    .PARAMETER Into
        The directory to save it in.

    .PARAMETER Force
        Download again even when the file is already there.
    #>
    param([string] $Repository, [string] $Version, [string] $Name, [string] $Into, [switch] $Force)
    $path = Join-Path $Into $Name
    if ((Test-Path -LiteralPath $path) -and -not $Force) { return $path }
    $url = "https://github.com/$Repository/releases/download/v$Version/$Name"
    try {
        Invoke-WebRequest -Uri $url -OutFile $path -UseBasicParsing -ErrorAction Stop
    } catch {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -Confirm:$false }
        return $null
    }
    return $path
}

function Test-ArchiveDigest {
    <#
    .SYNOPSIS
        Checks one file against the digest `SHA256SUMS` records for it.

    .DESCRIPTION
        Throws when the digest differs or when the file is not named in
        `SHA256SUMS` at all. The second case matters as much as the first: a
        checksum file that does not mention the archive proves nothing about it,
        and a script that read a missing line as a pass would verify every
        future release by accident.

    .PARAMETER Archive
        The downloaded file.

    .PARAMETER Sums
        The `SHA256SUMS` published beside it.
    #>
    param([string] $Archive, [string] $Sums)
    $name = Split-Path -Leaf $Archive
    $line = Get-Content -LiteralPath $Sums | Where-Object { $_ -match "\s\*?$([regex]::Escape($name))$" } | Select-Object -First 1
    if (-not $line) { throw "SHA256SUMS does not name $name, so it says nothing about it." }
    $wanted = ($line -split '\s+')[0]
    $got = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($got -ne $wanted.ToLowerInvariant()) {
        throw "$name does not match SHA256SUMS: published $wanted, downloaded $got."
    }
    Write-Host "  SHA256SUMS      $name matches"
}

function Test-SumsSignature {
    <#
    .SYNOPSIS
        Verifies `SHA256SUMS` against the project's minisign key, when it can.

    .DESCRIPTION
        The digest check above protects against a truncated transfer and
        against nothing else, because whoever could replace the archive could
        replace the checksum file beside it. The signature is what closes that,
        and it is the same check `packaging/verify-installs.sh` runs.

        Two reasons it is skipped, and both are printed rather than passed over
        in silence: the release published no signature (0.1.1 and 0.1.2), or
        minisign is not on this machine.

    .PARAMETER Sums
        The `SHA256SUMS` file.

    .PARAMETER Signature
        Its detached signature, or $null when the release published none.

    .PARAMETER Root
        The repository root, for finding minisign and the public key.
    #>
    param([string] $Sums, [string] $Signature, [string] $Root)
    if (-not $Signature) {
        Write-Host '  signature       not published for this release; the digest check is the whole of it'
        return
    }
    $minisign = Join-Path (Get-CrossBin -Root $Root) 'minisign.exe'
    if (-not (Test-Path -LiteralPath $minisign)) {
        Write-Host '  signature       minisign is not installed; run pwsh tools/cross/fetch-toolchain.ps1'
        return
    }
    $key = Join-Path $Root 'packaging/inillucent.pub'
    & $minisign -Vm $Sums -p $key 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "minisign rejected $Sums against packaging/inillucent.pub." }
    Write-Host '  signature       minisign accepted SHA256SUMS'
}

function Expand-Release {
    <#
    .SYNOPSIS
        Extracts the archive and returns the path of its `inillucent.exe`.

    .PARAMETER Archive
        The verified zip.

    .PARAMETER Into
        Where to extract it.

    .PARAMETER Force
        Extract again over an existing copy.
    #>
    param([string] $Archive, [string] $Into, [switch] $Force)
    if ($Force -and (Test-Path -LiteralPath $Into)) {
        Remove-Item -LiteralPath $Into -Recurse -Force -Confirm:$false
    }
    if (-not (Test-Path -LiteralPath $Into)) {
        Expand-Archive -LiteralPath $Archive -DestinationPath $Into -Force
    }
    $exe = Get-ChildItem -Path $Into -Recurse -Filter 'inillucent.exe' | Select-Object -First 1
    if (-not $exe) { throw "the archive holds no inillucent.exe: $Archive" }
    return $exe.FullName
}

function Read-VerifyQueries {
    <#
    .SYNOPSIS
        Reads `verify.sql` as an ordered list of name and statement pairs.

    .DESCRIPTION
        The same file `release_format.rs` parses, read the same way: a
        `-- name: <label>` line, then the one statement that answers it.

    .PARAMETER Path
        `tests/interop/verify.sql`.
    #>
    param([string] $Path)
    $queries = @()
    $name = $null
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        $text = $line.Trim()
        if ($text -like '-- name:*') {
            $name = $text.Substring(8).Trim()
        } elseif ($name -and $text -and -not $text.StartsWith('--')) {
            $queries += [pscustomobject]@{ Name = $name; Sql = $text.TrimEnd(';') }
            $name = $null
        }
    }
    if (-not $queries) { throw "no queries in $Path" }
    return $queries
}

function Invoke-ReleaseQuery {
    <#
    .SYNOPSIS
        Runs one query with a release's binary and returns the single value.

    .DESCRIPTION
        Every statement in `verify.sql` answers with one row of one column, so
        what comes back is a value rather than a rendering. A failure throws
        with the engine's own status name, because a fixture built from a
        refused query is a fixture that proves nothing.

    .PARAMETER Exe
        That release's `inillucent.exe`.

    .PARAMETER Database
        The database to ask.

    .PARAMETER Sql
        The statement.
    #>
    param([string] $Exe, [string] $Database, [string] $Sql)
    $raw = & $Exe query $Sql --db $Database --output json 2>&1 | Out-String
    $answer = $raw | ConvertFrom-Json
    if (-not $answer.ok) {
        throw "the release refused a verify query [$($answer.status)]: $($answer.message)`n  $Sql"
    }
    $rows = @($answer.rows)
    if ($rows.Count -lt 1) { throw "a verify query answered no rows: $Sql" }
    $value = @($rows[0])[0]
    if ($null -eq $value) { return '' }
    return [string] $value
}

function Build-Fixture {
    <#
    .SYNOPSIS
        Writes the database this release contributes, and returns its path.

    .DESCRIPTION
        The last two steps are the design. The checkpoint moves everything
        written so far into the database file; the row written after it exists
        only in the log segment. A later build that read the file and ignored
        the log would answer 120 rows where the fixture says 121, which is a
        failure somebody can act on rather than a silent difference.

    .PARAMETER Exe
        That release's `inillucent.exe`.

    .PARAMETER Build
        `tests/interop/build.sql`.

    .PARAMETER Into
        A scratch directory to build in.
    #>
    param([string] $Exe, [string] $Build, [string] $Into)
    $database = Join-Path $Into 'app.rdb'
    & $Exe create $database | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "the release could not create $database" }
    & $Exe run ".read $Build" --db $database | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'the release refused build.sql' }
    & $Exe checkpoint --db $database | Out-Null
    $row = "INSERT INTO note (id, title, body, weight, tag) VALUES (9001, " +
           "'the row that only the log holds', " +
           "'written after the checkpoint, so a reader that ignores the log cannot see it', " +
           "9.5, 'Ledger')"
    & $Exe exec $row --db $database | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'the release refused the row written after the checkpoint' }
    return $database
}

# ---------------------------------------------------------------------------

$archiveName = "inillucent-$Version-x86_64-pc-windows-msvc.zip"
$into = Get-ReleaseDirectory -Root $root -Version $Version
New-Item -ItemType Directory -Force -Path $into | Out-Null

Write-Host "inillucent $Version -> tests/interop/$Version"

$archive = Save-ReleaseAsset -Repository $Repository -Version $Version -Name $archiveName -Into $into -Force:$Force
if (-not $archive) { throw "v$Version published no $archiveName." }
$sums = Save-ReleaseAsset -Repository $Repository -Version $Version -Name 'SHA256SUMS' -Into $into -Force:$Force
if (-not $sums) { throw "v$Version published no SHA256SUMS, so nothing about the archive can be checked." }
$signature = Save-ReleaseAsset -Repository $Repository -Version $Version -Name 'SHA256SUMS.minisig' -Into $into -Force:$Force

Test-ArchiveDigest -Archive $archive -Sums $sums
Test-SumsSignature -Sums $sums -Signature $signature -Root $root

$exe = Expand-Release -Archive $archive -Into (Join-Path $into 'extract') -Force:$Force
Write-Host "  binary          $exe"

$scratch = Join-Path ([System.IO.Path]::GetTempPath()) "inillucent-interop-$Version-$PID"
if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force -Confirm:$false }
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

try {
    $database = Build-Fixture -Exe $exe -Build (Join-Path $root 'tests/interop/build.sql') -Into $scratch

    $queries = Read-VerifyQueries -Path (Join-Path $root 'tests/interop/verify.sql')
    $answers = foreach ($query in $queries) {
        $value = Invoke-ReleaseQuery -Exe $exe -Database $database -Sql $query.Sql
        "$($query.Name)`t$value"
    }
    Write-Host "  verify.sql      $($queries.Count) questions answered"

    $published = Join-Path $root "tests/interop/$Version"
    New-Item -ItemType Directory -Force -Path $published | Out-Null
    Copy-Item -LiteralPath $database -Destination (Join-Path $published 'app.rdb') -Force
    $segments = Get-ChildItem -Path $scratch -Filter 'app.rdb-wal.*' | Sort-Object Name
    if (-not $segments) { throw 'the release wrote no log segment, so the fixture would hold no log' }
    foreach ($segment in $segments) {
        Copy-Item -LiteralPath $segment.FullName -Destination (Join-Path $published $segment.Name) -Force
    }
    # **LF, written as one string rather than as lines.** `Set-Content` on
    # Windows ends every line with CRLF, and `expected.tsv` is compared byte for
    # byte against what the next release produces - so a fixture built on this
    # machine and one built anywhere else would differ in every line while
    # holding the same answers.
    $text = ($answers -join "`n") + "`n"
    [System.IO.File]::WriteAllText((Join-Path $published 'expected.tsv'), $text, (New-Object System.Text.UTF8Encoding $false))

    foreach ($file in (Get-ChildItem -Path $published -File | Sort-Object Name)) {
        Write-Host ("  {0,-38} {1,9:N0} bytes" -f $file.Name, $file.Length)
    }
} finally {
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force -Confirm:$false }
}
