<#
.SYNOPSIS
    Runs every cargo-fuzz target for a bounded time and records what it found.

.DESCRIPTION
    The sixteen targets in `fuzz/` have existed since phase 1 and nothing ever
    ran them (task-2066 section 4.4.7). A target that is never run is a file,
    not a test: it cannot fail, it rots against the API it fuzzes, and the
    corpus it would have built never exists.

    This runs each one for `-Seconds` and appends a row per target to
    `tests/fuzz-history.tsv`. The row is the evidence: a run that found nothing
    is worth recording, because "nothing" only means something beside how long
    it looked and how many inputs it got through.

    Every prerequisite is a **skip carrying the sentence that fixes it**, never
    a failure - the same rule `packaging/ship.ps1` follows for a credential it
    has not got. A script that refuses without a nightly toolchain is a script
    nobody runs, and this one has to be runnable on a machine that has only
    just cloned the repository.

    A crash is kept. `cargo fuzz` writes the input under
    `fuzz/artifacts/<target>/`, and the rule from `fuzz/README.md` still holds:
    add the input as a regression case in the target's stable counterpart, so it
    is checked forever rather than only while somebody is fuzzing.

.PARAMETER Seconds
    How long to run each target. The default is deliberately small: the point of
    a run in the ordinary course of things is that the targets still build and
    still run, and a long hunt is something a person asks for.

.PARAMETER Only
    Run these targets rather than all of them.

.PARAMETER WhatIf
    Print the plan and write nothing.

.EXAMPLE
    pwsh tools/run-fuzz.ps1 -WhatIf
    pwsh tools/run-fuzz.ps1 -Seconds 60
    pwsh tools/run-fuzz.ps1 -Only sql_text,fts5_query -Seconds 600
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [int] $Seconds = 30,
    [string[]] $Only = @(),
    [switch] $Install
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$fuzzDirectory = Join-Path $root 'fuzz'
$history = Join-Path $root 'tests/fuzz-history.tsv'

<#
.SYNOPSIS
    Returns every target name the fuzz crate declares.
.DESCRIPTION
    Read out of `fuzz/Cargo.toml` rather than listed here, so a target added
    without a line in this script is still run. `cargo fuzz list` would answer
    the same question and needs the toolchain this function runs before.
#>
function Get-FuzzTargets {
    $manifest = Join-Path $fuzzDirectory 'Cargo.toml'
    if (-not (Test-Path $manifest)) { return @() }
    $names = [System.Collections.Generic.List[string]]::new()
    $inBin = $false
    foreach ($line in Get-Content $manifest) {
        if ($line -match '^\s*\[\[bin\]\]\s*$') { $inBin = $true; continue }
        if ($line -match '^\s*\[') { $inBin = $false; continue }
        if ($inBin -and $line -match '^\s*name\s*=\s*"([^"]+)"') { $names.Add($Matches[1]) }
    }
    return $names.ToArray()
}

<#
.SYNOPSIS
    Reports what this machine is missing, as sentences that fix it.
#>
function Get-MissingPrerequisites {
    # **A list, returned as an array, and wrapped in `@()` by every caller.** A
    # PowerShell function returns a bare string when the collection it built
    # holds one element, and `$missing.Count` on a string is a property that is
    # not there - which is how the first version of this failed with "The
    # property 'Count' cannot be found on this object" on a machine that was
    # missing exactly one prerequisite.
    $missing = [System.Collections.Generic.List[string]]::new()
    $toolchains = & rustup toolchain list 2>$null
    if ($LASTEXITCODE -ne 0) {
        $missing.Add('no rustup on PATH; install it from https://rustup.rs')
    }
    elseif (-not ($toolchains | Where-Object { $_ -like 'nightly*' })) {
        $missing.Add('no nightly toolchain; run `rustup toolchain install nightly`')
    }
    & cargo fuzz --version *> $null
    if ($LASTEXITCODE -ne 0) {
        $missing.Add('no cargo-fuzz; run `cargo +nightly install cargo-fuzz`, or pass -Install')
    }
    if (-not (Get-SanitizerRuntimeDirectory)) {
        $missing.Add('no clang_rt.asan_dynamic-x86_64.dll; install the MSVC C++ AddressSanitizer component in the Visual Studio Installer')
    }
    return $missing.ToArray()
}

<#
.SYNOPSIS
    Returns the directory holding the AddressSanitizer runtime, if it is here.
.DESCRIPTION
    **cargo-fuzz builds with AddressSanitizer and Windows does not ship its
    runtime on the PATH**, so every target exits `0xc0000135
    STATUS_DLL_NOT_FOUND` before it reads an input - which is what the first run
    of this script recorded as sixteen crashes. The DLL is beside `cl.exe` in
    the MSVC toolchain, and putting that directory on the PATH for the run is
    the fix.

    Dropping the sanitizer instead does not work: `--sanitizer none` still leaves
    libfuzzer's coverage instrumentation looking for `__stop___sancov_pcs`, and
    the link fails. So the runtime is found rather than the sanitizer avoided.
#>
function Get-SanitizerRuntimeDirectory {
    $roots = @(
        'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC',
        'C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Tools\MSVC',
        'C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Tools\MSVC',
        'C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC'
    )
    foreach ($root in $roots) {
        if (-not (Test-Path $root)) { continue }
        $found = Get-ChildItem -Path $root -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName 'bin\Hostx64\x64' } |
            Where-Object { Test-Path (Join-Path $_ 'clang_rt.asan_dynamic-x86_64.dll') } |
            Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
}

<#
.SYNOPSIS
    Appends one target's result to the history file.
.PARAMETER Target
    The target that ran.
.PARAMETER Seconds
    How long it was given.
.PARAMETER Outcome
    `clean`, `crash` or `skipped`.
.PARAMETER Detail
    The artifact path for a crash, or the sentence that fixes a skip.
#>
function Add-HistoryRow {
    param(
        [string] $Target,
        [int] $Seconds,
        [string] $Outcome,
        [string] $Detail
    )
    if (-not (Test-Path $history)) {
        $header = "when`tcommit`tmachine`ttarget`tseconds`toutcome`tdetail"
        Set-Content -Path $history -Value $header -Encoding utf8
    }
    $when = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $commit = (& git -C $root rev-parse --short HEAD 2>$null)
    if (-not $commit) { $commit = 'unknown' }
    $machine = $env:COMPUTERNAME
    if (-not $machine) { $machine = 'unknown' }
    $row = "$when`t$commit`t$machine`t$Target`t$Seconds`t$Outcome`t$Detail"
    Add-Content -Path $history -Value $row -Encoding utf8
}

$targets = @(Get-FuzzTargets)
# **Split on commas, because a shell hands `-Only a,b,c` over as one string.**
# PowerShell splits a comma list written at a PowerShell prompt and does not
# split one that arrived through `pwsh -File`, so `-Only` matched nothing and
# the script reported "no targets to run" - which reads as a filter that found
# nothing rather than a filter that was never applied.
$asked = @($Only | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
if ($asked.Count -gt 0) {
    $targets = @($targets | Where-Object { $asked -contains $_ })
    if ($targets.Count -eq 0) {
        Write-Host "none of $($asked -join ', ') is a target; the targets are: $((Get-FuzzTargets) -join ', ')"
        exit 1
    }
}
if ($targets.Count -eq 0) {
    Write-Host 'no targets to run'
    exit 0
}

Write-Host "fuzz targets: $($targets -join ', ')"
Write-Host "each one for $Seconds second(s); history goes to tests/fuzz-history.tsv"

$missing = @(Get-MissingPrerequisites)
if ($Install -and ($missing | Where-Object { $_ -like 'no cargo-fuzz*' })) {
    if ($PSCmdlet.ShouldProcess('cargo-fuzz', 'install')) {
        Write-Host 'installing cargo-fuzz on the nightly toolchain'
        & cargo +nightly install cargo-fuzz
        $missing = @(Get-MissingPrerequisites)
    }
}
if ($missing.Count -gt 0) {
    Write-Host ''
    Write-Host 'skipped, and here is what would make it run:'
    foreach ($sentence in $missing) { Write-Host "  - $sentence" }
    if (-not $WhatIfPreference) {
        foreach ($target in $targets) {
            Add-HistoryRow -Target $target -Seconds 0 -Outcome 'skipped' -Detail ($missing -join '; ')
        }
        Write-Host ''
        Write-Host "recorded one skipped row per target in $history"
    }
    exit 0
}

$sanitizer = Get-SanitizerRuntimeDirectory
$originalPath = $env:PATH
Write-Host "sanitizer runtime: $sanitizer"

$crashed = [System.Collections.Generic.List[string]]::new()
foreach ($target in $targets) {
    if (-not $PSCmdlet.ShouldProcess($target, "fuzz for $Seconds s")) { continue }
    Write-Host ''
    Write-Host "--- $target"
    $artifacts = Join-Path $fuzzDirectory "artifacts/$target"
    $before = @(Get-ChildItem -Path $artifacts -File -ErrorAction SilentlyContinue).Count
    Push-Location $fuzzDirectory
    try {
        # **The bound is built as a string first.** Written inline as
        # `-max_total_time=$Seconds`, PowerShell passes the token through
        # literally - a leading `-` makes it a parameter name rather than an
        # expandable argument - and the target was handed the characters
        # `$Seconds`, which libfuzzer rejects. Every one of the sixteen was then
        # recorded as a crash.
        $bound = "-max_total_time=$Seconds"
        # The runtime goes on the PATH for the run and nowhere else, so nothing
        # this script does outlives it. See `Get-SanitizerRuntimeDirectory`.
        $env:PATH = "$sanitizer;$originalPath"
        # AddressSanitizer stays on, because dropping it does not work: with
        # `--sanitizer none` libfuzzer's coverage instrumentation still looks
        # for `__stop___sancov_pcs` and the link fails.
        & cargo +nightly fuzz run $target -- $bound
        $code = $LASTEXITCODE
    }
    finally {
        Pop-Location
        $env:PATH = $originalPath
    }
    $after = @(Get-ChildItem -Path $artifacts -File -ErrorAction SilentlyContinue).Count
    if ($code -eq 0) {
        Add-HistoryRow -Target $target -Seconds $Seconds -Outcome 'clean' -Detail ''
    }
    elseif ($after -gt $before) {
        # **An artifact is what tells a finding from a target that would not
        # start.** libfuzzer writes the input that crashed; a target that failed
        # to build, or exited before it ran an input, writes nothing - and
        # recording that as a crash is a history that says the engine is broken
        # when the machine is.
        $crashed.Add($target)
        Add-HistoryRow -Target $target -Seconds $Seconds -Outcome 'crash' -Detail $artifacts
        Write-Host "  crash; the input is under $artifacts"
    }
    else {
        Add-HistoryRow -Target $target -Seconds $Seconds -Outcome 'did-not-run' -Detail "exit $code, no artifact written"
        Write-Host "  did not run: exit $code, and no input was written"
    }
}

Write-Host ''
$stalled = @(Get-Content $history -ErrorAction SilentlyContinue | Where-Object { $_ -like "*`tdid-not-run`t*" })
if ($crashed.Count -eq 0 -and $stalled.Count -gt 0) {
    Write-Host 'some targets did not run; see the `did-not-run` rows in tests/fuzz-history.tsv'
}
if ($crashed.Count -gt 0) {
    Write-Host "crashes: $($crashed -join ', ')"
    Write-Host 'Keep the input, and add it as a regression case in the target''s stable counterpart.'
    exit 1
}
Write-Host 'every target ran clean'
exit 0
