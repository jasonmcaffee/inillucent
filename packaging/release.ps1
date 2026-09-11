<#
.SYNOPSIS
    Builds the release archive every installer and every package is made from.

.DESCRIPTION
    One layout, one set of binaries, six ecosystems downstream. That is the
    whole point: npm, PyPI, Homebrew, the Windows installer and the Go installer
    are five different ways of getting *this* directory onto a machine, and if
    each built its own there would be five things that could differ.

    It produces, under dist/:

        inillucent-<version>-<target>/          the staged layout
            bin/inillucent(.exe)                the verb-shaped CLI
            bin/inillucent-shell(.exe)          the sqlite3-shaped shell
            bin/inillucent-mcp(.exe)            the MCP server
            bin/inillucent-migrate(.exe)        the SQLite importer
            lib/inillucent_driver_capi.dll      the C ABI, for every binding
            include/inillucent_driver.h         the header a binding compiles against
            docs/  tests/  agent-skills/  README.md  AGENTS.md  LICENSE  VERSION
        inillucent-<version>-<target>.zip       the archive
        SHA256SUMS                              over the archive

    The build is --release --locked, because a release built from a resolved
    lockfile is a release somebody else can reproduce.

    WHAT IT REFUSES, AND WHY THAT IS THE POINT (task-1894)

    Before task-1894 this script built and staged whatever was in the working
    tree and labelled it with whatever -Version said. Every one of these was
    possible and none of them was detectable from the archive:

      * a release built from a dirty checkout, so the commit it claims is not
        the code inside it;
      * `-Version 2.0.0` on a tree whose binaries answer 0.1.0;
      * a tag that names a different commit than the one that was built;
      * an archive nobody opened, which is a release that has only been checked
        for having produced an archive.

    So a release now refuses unless: the checkout is clean, the tag `v<version>`
    exists and points at HEAD, the compiler is the pinned one, and the staged
    archive installs into an empty directory and answers. `provenance.json`
    records what was checked, and SHA256SUMS covers it.

    Each refusal has a named override, because a refusal nobody can get past on
    a bad afternoon is a refusal somebody deletes. The overrides are recorded in
    the provenance, so a release made with one says so.

.PARAMETER Version
    Overrides the version taken from the workspace manifest. It must still match
    what the manifest says unless -AllowVersionMismatch is passed as well: a
    version that differs from the binaries is the failure this parameter used to
    cause.

.PARAMETER Target
    The Rust target triple. Defaults to the host's.

.PARAMETER SkipBuild
    Stage from whatever is already in target/release. For iterating on the
    packaging itself; never for a real release, and recorded in the provenance.

.PARAMETER SmokeOnly
    Build, stage and smoke, then stop. No tag, no clean-checkout requirement and
    no provenance. This is what CI runs on every commit, so the packaging is
    checked continuously rather than once at a tag.

.PARAMETER AllowDirty
    Package from a working tree with uncommitted changes.

.PARAMETER AllowUntagged
    Package a commit that carries no matching tag.

.PARAMETER AllowVersionMismatch
    Package with a -Version that the workspace manifest does not agree with.

.PARAMETER SkipSmoke
    Do not install and run the staged archive. Recorded in the provenance,
    because an archive nobody opened is the thing the smoke test exists for.

.EXAMPLE
    pwsh packaging/release.ps1
    pwsh packaging/release.ps1 -SmokeOnly
    pwsh packaging/release.ps1 -SkipBuild -AllowDirty -AllowUntagged
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $Target,
    [switch] $SkipBuild,
    [switch] $SmokeOnly,
    [switch] $AllowDirty,
    [switch] $AllowUntagged,
    [switch] $AllowVersionMismatch,
    [switch] $SkipSmoke
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
$waived = @()

function Get-WorkspaceVersion {
    <#
    .SYNOPSIS
        Reads the version out of the workspace manifest's [workspace.package].
    #>
    $manifest = Get-Content -Path (Join-Path $root 'Cargo.toml') -Raw
    if ($manifest -match '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw 'Cargo.toml does not declare [workspace.package] version'
}

function Get-HostTarget {
    <#
    .SYNOPSIS
        Asks rustc what it builds for by default.
    #>
    $line = (& rustc -vV) | Where-Object { $_ -like 'host:*' }
    return ($line -split ':\s*')[1].Trim()
}

function Copy-Artifact {
    <#
    .SYNOPSIS
        Copies one built file into the staging layout, failing loudly if absent.

    .PARAMETER From
        The built file.

    .PARAMETER Into
        The directory to place it in.
    #>
    param([string] $From, [string] $Into)
    if (-not (Test-Path -LiteralPath $From)) {
        throw "the build did not produce $From"
    }
    New-Item -ItemType Directory -Force -Path $Into | Out-Null
    Copy-Item -LiteralPath $From -Destination $Into -Force
}

function Write-Sums {
    <#
    .SYNOPSIS
        Writes SHA256SUMS with LF line endings, whatever platform this is.

    .DESCRIPTION
        The file is read by sha256sum, shasum and awk on machines that are not
        this one. A carriage return becomes part of the file name those tools
        see, so the entry matches nothing and the installer reports that the
        platform was never published while listing it.

    .PARAMETER Path
        The SHA256SUMS file.

    .PARAMETER Lines
        The lines, without terminators.
    #>
    param([string] $Path, [string[]] $Lines)
    $text = ($Lines -join "`n") + "`n"
    [System.IO.File]::WriteAllText($Path, $text, (New-Object System.Text.UTF8Encoding $false))
}

function Get-PinnedToolchain {
    <#
    .SYNOPSIS
        Reads the channel out of rust-toolchain.toml.
    #>
    $path = Join-Path $root 'rust-toolchain.toml'
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    $text = Get-Content -Path $path -Raw
    if ($text -match '(?ms)channel\s*=\s*"([^"]+)"') { return $Matches[1] }
    return $null
}

function Deny-Unless {
    <#
    .SYNOPSIS
        Stops the release unless an override was passed, and records the waiver.

    .PARAMETER Allowed
        Whether the operator passed the override.

    .PARAMETER Name
        The waiver's name, recorded in the provenance.

    .PARAMETER Because
        What is wrong, and which flag gets past it.
    #>
    param([bool] $Allowed, [string] $Name, [string] $Because)
    if (-not $Allowed) { throw $Because }
    Write-Warning "release: $Because (waived with -$Name)"
    $script:waived += $Name
}

$manifestVersion = Get-WorkspaceVersion
if (-not $Version) { $Version = $manifestVersion }
if (-not $Target) { $Target = Get-HostTarget }
$exe = if ($IsWindows -or $env:OS -eq 'Windows_NT') { '.exe' } else { '' }

# **The version comes from one place, and an override has to agree with it.**
# `-Version 2.0.0` on a tree whose binaries answer 0.1.0 produced an archive
# named for a release that did not exist, and nothing downstream could tell.
if ($Version -ne $manifestVersion) {
    Deny-Unless -Allowed:$AllowVersionMismatch -Name 'AllowVersionMismatch' -Because @"
-Version $Version does not match the workspace manifest's $manifestVersion, so the archive would be
named for a version its binaries do not answer with.
"@
}

$commit = ''
if (-not $SmokeOnly) {
    $commit = (& git -C $root rev-parse HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $commit) {
        throw 'a release has to be made from a git checkout, and this is not one'
    }
    $commit = $commit.Trim()

    # A dirty checkout means the commit the archive claims is not the code
    # inside it, which makes every other check here a check on the wrong thing.
    $dirty = (& git -C $root status --porcelain)
    if ($dirty) {
        Deny-Unless -Allowed:$AllowDirty -Name 'AllowDirty' -Because @"
the working tree has uncommitted changes, so the commit this release records is not the code it
contains. Commit or stash first.
"@
    }

    # The tag is what a person downloading the archive resolves back to source.
    $tag = "v$Version"
    $tagged = (& git -C $root rev-list -n 1 $tag 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $tagged) {
        Deny-Unless -Allowed:$AllowUntagged -Name 'AllowUntagged' -Because @"
there is no tag $tag, so nothing in the repository resolves this archive back to a commit.
"@
    } elseif ($tagged.Trim() -ne $commit) {
        Deny-Unless -Allowed:$AllowUntagged -Name 'AllowUntagged' -Because @"
the tag $tag names $($tagged.Trim()) and HEAD is $commit, so the archive would carry a version whose
tag points at different code.
"@
    }

    # The pinned compiler is what every published number was produced with.
    $pinned = Get-PinnedToolchain
    $running = ((& rustc --version) -split ' ')[1]
    if ($pinned -and $running -and $pinned -ne $running) {
        Deny-Unless -Allowed:$AllowDirty -Name 'AllowDirty' -Because @"
rust-toolchain.toml pins $pinned and this is rustc $running, so the release would not be the build
the repository grades itself against.
"@
    }

    if ($SkipBuild) { $script:waived += 'SkipBuild' }
}

Write-Host "inillucent $Version for $Target"

if (-not $SkipBuild) {
    Write-Host 'building (release, locked)...'
    & cargo build --manifest-path (Join-Path $root 'Cargo.toml') --release --locked `
        -p inillucent-cli -p inillucent-migrate -p inillucent-driver-capi
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with $LASTEXITCODE" }
}

$built = Join-Path $root 'target/release'
$dist = Join-Path $root 'dist'
$name = "inillucent-$Version-$Target"
$stage = Join-Path $dist $name

if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

foreach ($program in @('inillucent', 'inillucent-shell', 'inillucent-mcp', 'inillucent-migrate')) {
    Copy-Artifact -From (Join-Path $built "$program$exe") -Into (Join-Path $stage 'bin')
}

# The C ABI, which is how every language that is not Rust reaches the engine.
# Its file name is the platform's, so each is tried and the one that exists is
# the one this platform builds.
$libraries = @('inillucent_driver_capi.dll', 'libinillucent_driver_capi.so', 'libinillucent_driver_capi.dylib')
$found = $false
foreach ($library in $libraries) {
    $candidate = Join-Path $built $library
    if (Test-Path -LiteralPath $candidate) {
        Copy-Artifact -From $candidate -Into (Join-Path $stage 'lib')
        $found = $true
    }
}
if (-not $found) { throw 'the build produced no C ABI shared library' }

Copy-Artifact -From (Join-Path $root 'drivers/inillucent-driver-capi/include/inillucent_driver.h') `
    -Into (Join-Path $stage 'include')
Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $stage -Force
Copy-Item -LiteralPath (Join-Path $root 'drivers/README.md') -Destination (Join-Path $stage 'DRIVER.md') -Force

# README.md links into docs/ for every subject it does not cover itself, so the
# archive carries that directory or the front page it ships is full of dead
# links. AGENTS.md and agent-skills/ travel with it for the same reason: the
# readme sends an AI agent to both.
Copy-Item -LiteralPath (Join-Path $root 'docs') -Destination (Join-Path $stage 'docs') -Recurse -Force
Copy-Item -LiteralPath (Join-Path $root 'AGENTS.md') -Destination $stage -Force
Copy-Item -LiteralPath (Join-Path $root 'agent-skills') -Destination (Join-Path $stage 'agent-skills') -Recurse -Force

# Two documents live under tests/ rather than under docs/, because they describe
# assets that sit beside them. README.md and three of the docs/ pages link to
# both, so they travel as well. Only the prose: the fixtures and the schedules
# are not part of a binary archive.
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'tests') | Out-Null
Copy-Item -LiteralPath (Join-Path $root 'tests/synthetic-corpus.md') -Destination (Join-Path $stage 'tests') -Force
Copy-Item -LiteralPath (Join-Path $root 'tests/inillucent-testing-tdd.md') -Destination (Join-Path $stage 'tests') -Force

Set-Content -Path (Join-Path $stage 'VERSION') -Value $Version -NoNewline

# The licence has to travel with the binaries: MIT requires the notice to
# accompany every copy, and an archive without one is not a distributable one.
$license = Join-Path $root 'LICENSE'
if (Test-Path -LiteralPath $license) {
    Copy-Item -LiteralPath $license -Destination $stage -Force
} else {
    Write-Warning 'LICENSE is missing from the repository root; the archive will not carry one'
}

# --------------------------------------------------------------------------
# The smoke test: install what was staged, into an empty directory, and use it.
#
# **This is the check the review asked for and the one the script did not
# have.** Everything above it verifies that files were produced; this is the
# only part that finds out whether they work. It runs against the *staged copy*
# rather than against `target/release`, because what a person downloads is the
# staged copy - a library that was found by being beside the build would pass a
# test run in the build directory and fail on their machine.
# --------------------------------------------------------------------------

function Invoke-Smoke {
    <#
    .SYNOPSIS
        Installs the staged layout into a scratch directory and exercises it.

    .PARAMETER Stage
        The staged layout.

    .PARAMETER Version
        The version the binaries should answer with.
    #>
    param([string] $Stage, [string] $Version)

    $scratch = Join-Path ([System.IO.Path]::GetTempPath()) "inillucent-smoke-$(Get-Random)"
    New-Item -ItemType Directory -Force -Path $scratch | Out-Null
    try {
        # Copied rather than run in place: an installed tree is what a person
        # gets, and a binary that only works beside its build directory is a
        # binary that works for nobody else.
        Copy-Item -LiteralPath $Stage -Destination (Join-Path $scratch 'inillucent') -Recurse -Force
        $installed = Join-Path $scratch 'inillucent'
        $bin = Join-Path $installed 'bin'
        $cli = Join-Path $bin "inillucent$exe"

        # 1. The command line answers, and answers with the version on the tin.
        $reported = (& $cli --version) -join ' '
        if ($LASTEXITCODE -ne 0) { throw "the installed CLI did not run: $reported" }
        if ($reported -notmatch [regex]::Escape($Version)) {
            throw "the installed CLI reports '$reported', not $Version - the archive is labelled for a build it does not contain"
        }

        # 2. A database is created, written, reopened and read. Reopened, because
        #    a write that never reached the file passes every check that does not
        #    close the database first.
        $database = Join-Path $scratch 'smoke.rdb'
        & $cli --db $database exec 'CREATE TABLE t (n INTEGER, s TEXT)' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'the installed CLI could not create a table' }
        & $cli --db $database exec "INSERT INTO t (n, s) VALUES (1, 'one'), (2, 'two')" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'the installed CLI could not insert' }
        $counted = (& $cli --db $database query 'SELECT count(*) FROM t' --output json) -join ''
        if ($LASTEXITCODE -ne 0 -or $counted -notmatch '2') {
            throw "the installed CLI read back '$counted' rather than 2 rows"
        }
        $checked = (& $cli --db $database integrity-check) -join ' '
        if ($LASTEXITCODE -ne 0) { throw "the installed database failed its integrity check: $checked" }

        # 3. The MCP server initializes and answers a tool call, which is the
        #    surface an agent is handed and the one nothing downstream tests.
        $mcp = Join-Path $bin "inillucent-mcp$exe"
        $requests = @(
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}',
            '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}',
            ('{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inillucent_query","arguments":{"db":"' +
             $database.Replace('\', '/') + '","sql":"SELECT count(*) FROM t"}}}')
        ) -join "`n"
        $answered = $requests | & $mcp
        $answered = $answered -join "`n"
        if ($answered -notmatch 'inillucent_query') {
            throw "the installed MCP server did not list its tools: $answered"
        }
        if ($answered -notmatch '"isError":\s*false') {
            throw "the installed MCP server refused a query it should have answered: $answered"
        }

        # 4. The shipped header and shared library compile and link, which is
        #    what every binding that is not Rust does with this archive.
        Test-CApi -Installed $installed -Scratch $scratch
    } finally {
        Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Test-CApi {
    <#
    .SYNOPSIS
        Compiles a C program against the shipped header and links the shipped library.

    .PARAMETER Installed
        The installed layout.

    .PARAMETER Scratch
        Where to build.
    #>
    param([string] $Installed, [string] $Scratch)

    # **`cl` is not on the path outside a developer command prompt**, and a
    # release is made from an ordinary shell. So Windows finds `vcvars64.bat`
    # and runs the compile through a batch file that calls it first, which is
    # what `crates/inillucent-compat/tests/capi.rs` already does for the same
    # reason. Without it the C ABI check warned and waived on every Windows
    # release, which is a check that never runs.
    $vcvars = $null
    if ($IsWindows -or $env:OS -eq 'Windows_NT') {
        foreach ($visual in @(
            'C:/Program Files/Microsoft Visual Studio/2022/Community',
            'C:/Program Files/Microsoft Visual Studio/2022/Professional',
            'C:/Program Files/Microsoft Visual Studio/2022/Enterprise',
            'C:/Program Files/Microsoft Visual Studio/2022/BuildTools'
        )) {
            $candidate = Join-Path $visual 'VC/Auxiliary/Build/vcvars64.bat'
            if (Test-Path -LiteralPath $candidate) { $vcvars = $candidate; break }
        }
    }
    $compiler = Get-Command cc -ErrorAction SilentlyContinue
    if (-not $vcvars -and -not $compiler) {
        # A missing compiler is a check that could not run, and it is said out
        # loud rather than skipped quietly: CI installs one, so this line
        # appearing there means the CI image changed.
        Write-Warning 'smoke: no C compiler, so the shipped header and library were not linked'
        $script:waived += 'NoCCompiler'
        return
    }

    $source = Join-Path $Scratch 'smoke.c'
    Set-Content -Path $source -Value @'
#include <stdio.h>
#include "inillucent_driver.h"

/* The smallest program that proves the shipped header and library agree: open,
 * connect, run, read one value back, and close in the documented order. */
int main(void)
{
    inillucent_db *db = NULL;
    inillucent_conn *conn = NULL;
    inillucent_rows *rows = NULL;
    inillucent_error *error = NULL;
    if (inillucent_open("smoke-capi.rdb", INILLUCENT_OPEN_CREATE, &db, &error) != INILLUCENT_OK) {
        printf("open failed\n");
        return 1;
    }
    if (inillucent_connect(db, &conn, &error) != INILLUCENT_OK) {
        printf("connect failed\n");
        return 1;
    }
    if (inillucent_execute(conn, "SELECT 41 + 1", 10, &rows, &error) != INILLUCENT_OK) {
        printf("query failed\n");
        return 1;
    }
    if (inillucent_value_int(rows, 0, 0) != 42) {
        printf("wrong answer\n");
        return 1;
    }
    inillucent_rows_free(rows);
    inillucent_conn_free(conn);
    inillucent_close(db, &error);
    printf("capi ok\n");
    return 0;
}
'@

    $include = Join-Path $Installed 'include'
    $lib = Join-Path $Installed 'lib'
    $exeOut = Join-Path $Scratch "smoke_capi$exe"
    if ($vcvars) {
        # The import library, which Windows needs to link a DLL and which is not
        # part of the archive: a *binding* loads the DLL at run time rather than
        # linking it, so shipping one would be shipping something only this
        # check uses.
        $import = Join-Path (Join-Path $root 'target/release') 'inillucent_driver_capi.dll.lib'
        if (-not (Test-Path -LiteralPath $import)) {
            Write-Warning 'smoke: no import library beside the build, so the C ABI was not linked'
            $script:waived += 'NoImportLibrary'
            return
        }
        # A batch file rather than a command line, because `cmd /c` will not
        # take a quoted path with forward slashes in it - the same reason
        # `capi.rs` writes one.
        $script = Join-Path $Scratch 'build_smoke.bat'
        $body = @(
            '@echo off',
            ('call "' + $vcvars.Replace('/', '\') + '" >nul'),
            ('cl /nologo /W3 /MD /I "' + $include.Replace('/', '\') + '" "' +
             $source.Replace('/', '\') + '" "' + $import.Replace('/', '\') +
             '" /Fe:"' + $exeOut.Replace('/', '\') + '" /Fo:"' +
             $Scratch.Replace('/', '\') + '\\" /link /INCREMENTAL:NO')
        ) -join "`r`n"
        Set-Content -Path $script -Value $body
        # Captured rather than discarded: a compile that fails here is the
        # only evidence of why, and `| Out-Null` threw it away.
        $said = (& cmd /c $script) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            throw "the shipped header and library did not compile and link:`n$said"
        }
    } else {
        # The arguments are built as an array rather than written inline:
        # PowerShell parses `-Wl,-rpath,$lib` as a parameter name and stops.
        $arguments = @(
            '-O0', '-I', $include, $source,
            '-L', $lib, '-linillucent_driver_capi',
            "-Wl,-rpath,$lib",
            '-o', $exeOut
        )
        & cc @arguments
    }
    if ($LASTEXITCODE -ne 0) { throw 'the shipped header and library did not compile and link' }

    # Beside the program, so it is found the way an installed one is.
    Get-ChildItem -Path $lib -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $Scratch -Force
    }
    $said = (& $exeOut) -join ' '
    if ($LASTEXITCODE -ne 0 -or $said -notmatch 'capi ok') {
        throw "the C program built from the shipped archive did not run: $said"
    }
}

if (-not $SkipSmoke) {
    Write-Host 'smoking the staged archive...'
    Invoke-Smoke -Stage $stage -Version $Version
    Write-Host 'smoke ok'
} else {
    $waived += 'SkipSmoke'
}

if ($SmokeOnly) {
    Write-Host ''
    Write-Host "staged  $stage"
    Write-Host 'smoke only: no archive, no checksums, no provenance'
    exit 0
}

$archive = Join-Path $dist "$name.zip"
if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
Compress-Archive -Path $stage -DestinationPath $archive -CompressionLevel Optimal

# One SHA256SUMS for the whole dist directory, rewritten each time, so an
# installer can verify what it downloaded against one file.
#
# **Written with LF, on every platform.** Set-Content uses the platform's line
# ending, and a CRLF SHA256SUMS is read by `awk` in packaging/install.sh on
# Linux - where `$NF` then carries a trailing carriage return and matches
# nothing. Windows awk opens files in text mode and drops the \r, so the whole
# class of failure was invisible from the machine that produced the file.
#
# **Wrapped in @(...).** A single-match `ForEach-Object` pipeline unwraps to a
# scalar string rather than a one-element array, so a dist directory holding
# exactly one archive turned the later `$lines += ...` into string
# concatenation instead of appending a line - SHA256SUMS came out as one line
# with the provenance hash glued onto the end of the archive hash, and every
# installer's checksum lookup failed to find either entry.
$sums = Join-Path $dist 'SHA256SUMS'
$lines = @(Get-ChildItem -Path $dist -Filter '*.zip' | ForEach-Object {
    "$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
})
Get-ChildItem -Path $dist -Filter '*.tar.gz' -ErrorAction SilentlyContinue | ForEach-Object {
    $lines += "$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
}
Write-Sums -Path $sums -Lines $lines

# --------------------------------------------------------------------------
# Provenance: what this archive is, and what was checked before it existed.
#
# **It records the waivers.** A release made with -AllowDirty is a legitimate
# thing to want on a bad afternoon and an illegitimate thing to forget, so the
# file says so and SHA256SUMS covers the file. A provenance that only recorded
# the happy path would be a provenance that is true of every release and
# therefore says nothing about any of them.
# --------------------------------------------------------------------------
$provenance = Join-Path $dist 'provenance.json'
$archiveHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLower()
$record = [ordered]@{
    version   = $Version
    target    = $Target
    commit    = $commit
    tag       = "v$Version"
    toolchain = ((& rustc --version) -join ' ')
    built_at  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    archive   = [ordered]@{
        name   = "$name.zip"
        sha256 = $archiveHash
        bytes  = (Get-Item -LiteralPath $archive).Length
    }
    checks    = [ordered]@{
        clean_checkout    = (-not ($waived -contains 'AllowDirty'))
        tag_matches_head  = (-not ($waived -contains 'AllowUntagged'))
        version_agrees    = (-not ($waived -contains 'AllowVersionMismatch'))
        built_from_source = (-not ($waived -contains 'SkipBuild'))
        installed_and_run = (-not ($waived -contains 'SkipSmoke'))
        c_abi_linked      = (-not ($waived -contains 'NoCCompiler') -and
                             -not ($waived -contains 'NoImportLibrary'))
    }
    waived    = @($waived | Sort-Object -Unique)
}
$record | ConvertTo-Json -Depth 5 | Set-Content -Path $provenance

# The provenance goes into SHA256SUMS as well, so a downloader who verified the
# archive has also verified the claims made about it.
$lines += "$((Get-FileHash -LiteralPath $provenance -Algorithm SHA256).Hash.ToLower())  provenance.json"
Write-Sums -Path $sums -Lines $lines

Write-Host ''
Write-Host "staged     $stage"
Write-Host "archive    $archive"
Write-Host "provenance $provenance"
Write-Host "sums       $sums"
if ($waived.Count -gt 0) {
    Write-Warning "this release waived: $(($waived | Sort-Object -Unique) -join ', ')"
}
Get-Content -Path $sums
