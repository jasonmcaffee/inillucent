# Downloads, verifies, and builds the pinned SQLite 3.53.4 oracle on Windows.
#
# The oracle is a separate child process compiled from the official amalgamation.
# It is the only form in which SQLite appears in this workspace, and nothing
# here is a production dependency: the artifacts land in the gitignored
# .sqlite-ref/ directory and no inillucent crate links against them.
#
# Every download is checked against the SHA3-256 sum SQLite publishes, using
# inillucent's own implementation, so a corrupted or substituted archive cannot
# become the thing every parity claim is measured against.
#
# Usage: pwsh tools/sqlite-reference.ps1 [-Force]

param([switch]$Force)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$version = '3.53.4'
$release = '3530400'
$refDir = Join-Path $root ".sqlite-ref/$version"
$srcDir = Join-Path $refDir 'src'
New-Item -ItemType Directory -Force -Path $refDir | Out-Null

function Get-Artifact {
    param([string]$Name, [string]$Url)
    $target = Join-Path $refDir $Name
    if ((Test-Path $target) -and -not $Force) {
        Write-Host "have $Name"
    } else {
        Write-Host "downloading $Name"
        Invoke-WebRequest -Uri $Url -OutFile $target -UseBasicParsing
    }
    Push-Location $root
    try {
        # Out-Host keeps the verifier's own output off the pipeline, so this
        # function returns the path and nothing else.
        & cargo run --quiet -p inillucent-compat --bin inillucent-manifest -- verify-artifact $Name $target | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "$Name failed its pinned checksum" }
    } finally {
        Pop-Location
    }
    return $target
}

$amalgamation = Get-Artifact "sqlite-amalgamation-$release.zip" "https://sqlite.org/2026/sqlite-amalgamation-$release.zip"
$tools = Get-Artifact "sqlite-tools-win-x64-$release.zip" "https://sqlite.org/2026/sqlite-tools-win-x64-$release.zip"

if (-not (Test-Path $srcDir) -or $Force) {
    Remove-Item -Recurse -Force -Path $srcDir -ErrorAction SilentlyContinue
    Expand-Archive -Path $amalgamation -DestinationPath $refDir -Force
    Move-Item -Path (Join-Path $refDir "sqlite-amalgamation-$release") -Destination $srcDir -Force
}
$shellDir = Join-Path $refDir 'shell'
# Keyed on this platform's own binary rather than on the directory, for the
# reason the POSIX script gives: the two share `shell/`, and testing the
# directory made whichever ran second skip its own extraction.
if (-not (Test-Path (Join-Path $shellDir 'sqlite3.exe')) -or $Force) {
    Expand-Archive -Path $tools -DestinationPath $shellDir -Force
}

# Locate the MSVC toolchain the same way cargo does, so the driver is built with
# the compiler the rest of the workspace already requires.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw 'vswhere.exe not found; install the Visual Studio C++ build tools' }
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw 'no MSVC toolchain found' }
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'

$driver = Join-Path $root 'compat/oracle/sqlite_driver.c'
$output = Join-Path $refDir 'sqlite-oracle.exe'
$defines = '/DSQLITE_ENABLE_FTS5 /DSQLITE_ENABLE_RTREE /DSQLITE_ENABLE_MATH_FUNCTIONS /DSQLITE_ENABLE_COLUMN_METADATA /DSQLITE_ENABLE_PREUPDATE_HOOK /DSQLITE_ENABLE_SESSION /DSQLITE_ENABLE_DBSTAT_VTAB /DSQLITE_THREADSAFE=1'
# cl is run from the reference directory so that its object files land there
# without a /Fo argument; a trailing backslash inside a quoted /Fo path escapes
# the quote and cl then reads the rest of the command line as part of it.
$command = "call `"$vcvars`" >nul && cd /d `"$refDir`" && cl /nologo /O2 /MD $defines /I `"$srcDir`" `"$driver`" `"$srcDir\sqlite3.c`" /Fe:`"$output`" /link /INCREMENTAL:NO"
Write-Host 'building the oracle driver'
& cmd.exe /c $command
if ($LASTEXITCODE -ne 0) { throw 'the oracle driver failed to build' }

# The performance scorecard's SQLite arm. It reads the same plan file the
# inillucent arm reads, so the fairness contract is one copy of the SQL rather than
# two that are meant to agree.
$bench = Join-Path $root 'compat/oracle/sqlite_bench.c'
$benchOutput = Join-Path $refDir 'sqlite-bench.exe'
$benchCommand = "call `"$vcvars`" >nul && cd /d `"$refDir`" && cl /nologo /O2 /MD $defines /I `"$srcDir`" `"$bench`" `"$srcDir\sqlite3.c`" /Fe:`"$benchOutput`" /link /INCREMENTAL:NO"
Write-Host 'building the benchmark driver'
& cmd.exe /c $benchCommand
if ($LASTEXITCODE -ne 0) { throw 'the benchmark driver failed to build' }

Write-Host "oracle: $output"
Write-Host "bench: $benchOutput"
Write-Host "set INILLUCENT_SQLITE_ORACLE=$output to run the differential tests"
