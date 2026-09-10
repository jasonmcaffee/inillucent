<#
.SYNOPSIS
    The one description of what a release archive contains, dot-sourced by every
    script that builds one.

.DESCRIPTION
    packaging/release.ps1 built this layout for Windows and packaging/release.sh
    built it for everything else. Once the same machine started producing all
    four targets, two copies of the layout became two things that could differ,
    so it lives here and both callers dot-source it:

        . "$PSScriptRoot/stage-layout.ps1"

    Nothing in this file runs on import; it defines functions and returns.
#>

$ErrorActionPreference = 'Stop'

# GNU tar rather than the bsdtar in System32, because the release needs
# --mode, --sort and --owner and bsdtar has none of them. Git for Windows ships
# it and this repository already assumes Git.
$script:GnuTar = 'C:\Program Files\Git\usr\bin\tar.exe'

function Get-WorkspaceVersion {
    <#
    .SYNOPSIS
        Reads the version out of the workspace manifest's [workspace.package].

    .PARAMETER Root
        The repository root.
    #>
    param([string] $Root)
    $manifest = Get-Content -Path (Join-Path $Root 'Cargo.toml') -Raw
    if ($manifest -match '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        return $Matches[1]
    }
    throw 'Cargo.toml does not declare [workspace.package] version'
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
    if (-not (Test-Path -LiteralPath $From)) { throw "the build did not produce $From" }
    New-Item -ItemType Directory -Force -Path $Into | Out-Null
    Copy-Item -LiteralPath $From -Destination $Into -Force
}

function New-InillucentStage {
    <#
    .SYNOPSIS
        Builds the staged directory an archive is made from, and returns its path.

    .DESCRIPTION
        bin/, lib/ and include/ come from the build; the documentation comes from
        the repository, because README.md links into docs/ for every subject it
        does not cover itself and an archive without it ships dead links.

    .PARAMETER Root
        The repository root.

    .PARAMETER Version
        The version this archive claims.

    .PARAMETER Target
        The target triple, which names the archive.

    .PARAMETER BuiltDir
        The directory the four programs and the shared library were built into.

    .PARAMETER Dist
        Where the staged directory goes.
    #>
    param(
        [string] $Root,
        [string] $Version,
        [string] $Target,
        [string] $BuiltDir,
        [string] $Dist
    )

    $exe = if ($Target -like '*windows*') { '.exe' } else { '' }
    $stage = Join-Path $Dist "inillucent-$Version-$Target"
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force -Confirm:$false }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    foreach ($program in @('inillucent', 'inillucent-shell', 'inillucent-mcp', 'inillucent-migrate')) {
        Copy-Artifact -From (Join-Path $BuiltDir "$program$exe") -Into (Join-Path $stage 'bin')
    }

    # The C ABI, which is how every language that is not Rust reaches the engine.
    # Each platform's file name is tried and the one that exists is this
    # platform's.
    $found = $false
    foreach ($library in @('inillucent_driver_capi.dll', 'libinillucent_driver_capi.so', 'libinillucent_driver_capi.dylib')) {
        $candidate = Join-Path $BuiltDir $library
        if (Test-Path -LiteralPath $candidate) {
            Copy-Artifact -From $candidate -Into (Join-Path $stage 'lib')
            $found = $true
        }
    }
    if (-not $found) { throw "the build in $BuiltDir produced no C ABI shared library" }

    Copy-Artifact -From (Join-Path $Root 'drivers/inillucent-driver-capi/include/inillucent_driver.h') `
        -Into (Join-Path $stage 'include')
    Copy-Item -LiteralPath (Join-Path $Root 'README.md') -Destination $stage -Force
    Copy-Item -LiteralPath (Join-Path $Root 'drivers/README.md') -Destination (Join-Path $stage 'DRIVER.md') -Force
    Copy-Item -LiteralPath (Join-Path $Root 'docs') -Destination (Join-Path $stage 'docs') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $Root 'AGENTS.md') -Destination $stage -Force
    Copy-Item -LiteralPath (Join-Path $Root 'agent-skills') -Destination (Join-Path $stage 'agent-skills') -Recurse -Force

    # Two documents live under tests/ because they describe assets that sit
    # beside them, and README.md links to both. Only the prose travels.
    New-Item -ItemType Directory -Force -Path (Join-Path $stage 'tests') | Out-Null
    Copy-Item -LiteralPath (Join-Path $Root 'tests/synthetic-corpus.md') -Destination (Join-Path $stage 'tests') -Force
    Copy-Item -LiteralPath (Join-Path $Root 'tests/inillucent-testing-tdd.md') -Destination (Join-Path $stage 'tests') -Force

    Set-Content -Path (Join-Path $stage 'VERSION') -Value $Version -NoNewline

    # MIT requires the notice to accompany every copy, so an archive without one
    # is not a distributable one.
    $license = Join-Path $Root 'LICENSE'
    if (Test-Path -LiteralPath $license) {
        Copy-Item -LiteralPath $license -Destination $stage -Force
    } else {
        Write-Warning 'LICENSE is missing from the repository root; the archive will not carry one'
    }

    return $stage
}

function New-InillucentTarGz {
    <#
    .SYNOPSIS
        Writes the .tar.gz for a staged directory, with the modes a Unix machine
        needs.

    .DESCRIPTION
        NTFS carries no execute bit, so a tar written from a Windows staging
        directory has every program at 0644 and the archive is inert on the
        machine it is extracted on. The modes are therefore set by tar rather
        than read from disk, in two passes: everything at 0644 with directories
        traversable, then bin/ and lib/ appended at 0755.

    .PARAMETER Stage
        The staged directory.

    .PARAMETER Dist
        The directory the archive is written to.
    #>
    param([string] $Stage, [string] $Dist)

    if (-not (Test-Path -LiteralPath $script:GnuTar)) {
        throw "GNU tar is not at $($script:GnuTar). Install Git for Windows, which ships it."
    }
    $name = Split-Path -Leaf $Stage
    $tar = Join-Path $Dist "$name.tar"
    $archive = "$tar.gz"
    foreach ($stale in @($tar, $archive)) {
        if (Test-Path -LiteralPath $stale) { Remove-Item -LiteralPath $stale -Force -Confirm:$false }
    }

    # Forward slashes, because Git's tar is an MSYS program: it reads a
    # backslash as an escape, so `...\66a6...` arrives as `...6a6...` and the
    # path does not exist. --force-local is what stops it reading `C:` as a
    # remote host once the slashes are the other way round.
    $tarArgument = $tar -replace '\\', '/'
    $distArgument = $Dist -replace '\\', '/'

    $common = @('--force-local', '--owner=0', '--group=0', '--numeric-owner', '--sort=name')
    & $script:GnuTar @common '--mode=a+rX,u+w' "--exclude=$name/bin" "--exclude=$name/lib" -cf $tarArgument -C $distArgument $name
    if ($LASTEXITCODE -ne 0) { throw "tar failed with $LASTEXITCODE" }
    & $script:GnuTar @common '--mode=0755' -rf $tarArgument -C $distArgument "$name/bin" "$name/lib"
    if ($LASTEXITCODE -ne 0) { throw "tar append failed with $LASTEXITCODE" }
    # gzip the tar that was just written rather than letting tar compress as it
    # archives, because the modes are set across two passes and only an
    # uncompressed archive can be appended to.
    $input = [System.IO.File]::OpenRead($tar)
    try {
        $output = [System.IO.File]::Create($archive)
        try {
            $gzip = New-Object System.IO.Compression.GZipStream($output, [System.IO.Compression.CompressionLevel]::Optimal)
            try { $input.CopyTo($gzip) } finally { $gzip.Dispose() }
        } finally { $output.Dispose() }
    } finally { $input.Dispose() }
    Remove-Item -LiteralPath $tar -Force -Confirm:$false

    return $archive
}

function New-InillucentZip {
    <#
    .SYNOPSIS
        Writes the .zip for a staged directory.

    .PARAMETER Stage
        The staged directory.

    .PARAMETER Dist
        The directory the archive is written to.
    #>
    param([string] $Stage, [string] $Dist)
    $archive = Join-Path $Dist ((Split-Path -Leaf $Stage) + '.zip')
    if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force -Confirm:$false }
    Compress-Archive -Path $Stage -DestinationPath $archive -CompressionLevel Optimal
    return $archive
}

function Update-Sha256Sums {
    <#
    .SYNOPSIS
        Rewrites dist/SHA256SUMS over every archive present.

    .DESCRIPTION
        One file, rewritten each time, because every installer in packaging/
        verifies what it downloaded against exactly this file.

    .PARAMETER Dist
        The dist directory.
    #>
    param([string] $Dist)
    $sums = Join-Path $Dist 'SHA256SUMS'
    $lines = @()
    foreach ($pattern in @('*.zip', '*.tar.gz', '*.deb', '*.rpm')) {
        Get-ChildItem -Path $Dist -Filter $pattern -File -ErrorAction SilentlyContinue |
            Sort-Object Name | ForEach-Object {
                $lines += "$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
            }
    }
    Set-Content -Path $sums -Value $lines
    return $sums
}
