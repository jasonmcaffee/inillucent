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

function Repair-StagedLink {
    <#
    .SYNOPSIS
        Rewrites the Markdown links that name a repository path the archive does
        not carry.

    .DESCRIPTION
        The archive is a subset of the repository: `drivers/README.md` is staged
        as `DRIVER.md`, and `drivers/`, `compat/` and `tools/` are not staged at
        all. Five links in README.md, AGENTS.md and docs/feature-comparison.md
        named those paths, so they were dead in every archive a person
        downloaded. The repository copies keep the links that work on GitHub;
        only the staged copies are rewritten.

    .PARAMETER Stage
        The staged directory, already holding the documents.
    #>
    param([string] $Stage)

    $rewrites = @(
        @{ File = 'README.md';                   From = '](drivers/README.md)';                To = '](DRIVER.md)' },
        @{ File = 'AGENTS.md';                   From = '](drivers/README.md)';                To = '](DRIVER.md)' },
        @{ File = 'README.md';                   From = '[`drivers/conformance/suite.json`](drivers/conformance/suite.json)'; To = '`drivers/conformance/suite.json`, in the repository,' },
        @{ File = 'docs/feature-comparison.md';  From = '[`tools/feature-probe/`](../tools/feature-probe/README.md)'; To = '`tools/feature-probe/`, in the repository,' },
        @{ File = 'docs/feature-comparison.md';  From = '[`drivers/README.md`](../drivers/README.md)'; To = '[`DRIVER.md`](../DRIVER.md)' },
        @{ File = 'docs/feature-comparison.md';  From = '[`compat/README.md`](../compat/README.md)'; To = '`compat/README.md`, in the repository' },
        # task-1962 added a link from the glossary's Transaction entry to
        # drivers/README.md, which the archive stages as DRIVER.md. No release
        # was cut between then and task-1995, so the dead-link check below has
        # been failing every target's staging - Windows and Linux as well as
        # macOS - since that commit.
        @{ File = 'docs/glossary.md';            From = '[`Transaction`](../drivers/README.md)'; To = '[`Transaction`](../DRIVER.md)' },
        @{ File = 'DRIVER.md';                   From = '](inillucent-driver-capi/include/inillucent_driver.h)'; To = '](include/inillucent_driver.h)' }
    )

    # Directories the archive deliberately does not carry, and what to do about a link into one.
    #
    # **This is the third time the same defect has stopped a release.** task-1962 linked
    # docs/glossary.md to drivers/README.md; task-1998 linked docs/roadmap.md to its own TDD under
    # tasks/. Each was a reasonable thing to write, each was invisible until somebody cut a release,
    # and each needed a row of its own in the table above. A row per link does not scale: a
    # documentation change in any ticket can break the packaging in a ticket nobody is working on.
    #
    # So a link into one of these directories becomes its own text - the sentence still reads, and
    # the reader is not sent to a file the archive was never going to contain. Every other dead link
    # still fails the staging, because those are mistakes rather than policy.
    $neverStaged = @('tasks', 'crates', 'drivers', 'compat', 'tools', 'fuzz', 'examples', 'packaging', 'scripts', 'runs', 'design')
    $intoUnstaged = '\[([^\]]+)\]\((?:\.\./)*(?:' + ($neverStaged -join '|') + ')/[^)]*\)'

    foreach ($rewrite in $rewrites) {
        $path = Join-Path $Stage $rewrite.File
        if (-not (Test-Path -LiteralPath $path)) { continue }
        $text = [System.IO.File]::ReadAllText($path)
        if (-not $text.Contains($rewrite.From)) { continue }
        $text = $text.Replace($rewrite.From, $rewrite.To)
        [System.IO.File]::WriteAllText($path, $text)
    }

    foreach ($document in (Get-ChildItem -LiteralPath $Stage -Recurse -Filter '*.md')) {
        $text = [System.IO.File]::ReadAllText($document.FullName)
        $flattened = [regex]::Replace($text, $intoUnstaged, '$1')
        if ($flattened -ne $text) { [System.IO.File]::WriteAllText($document.FullName, $flattened) }
    }

    # A dead link in the archive is the defect this function exists to prevent,
    # so the staging fails rather than shipping one.
    $dead = @()
    foreach ($document in (Get-ChildItem -LiteralPath $Stage -Recurse -Filter '*.md')) {
        $text = [System.IO.File]::ReadAllText($document.FullName)
        foreach ($match in [regex]::Matches($text, '\]\(([^)\s]+)\)')) {
            $href = $match.Groups[1].Value
            if ($href -match '^(https?:|mailto:|#)') { continue }
            $file = ($href -split '#')[0]
            if (-not $file) { continue }
            $target = Join-Path $document.DirectoryName $file
            if (-not (Test-Path -LiteralPath $target)) {
                $dead += "$($document.Name) -> $href"
            }
        }
    }
    if ($dead.Count -gt 0) {
        throw "the staged documentation carries $($dead.Count) link(s) to a path the archive does not have: $($dead -join '; ')"
    }
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

    Repair-StagedLink -Stage $stage

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

function Import-MsvcEnvironment {
    <#
    .SYNOPSIS
        Puts the MSVC compiler's own environment variables into this process, if they are missing.

    .DESCRIPTION
        **A C dependency cannot compile without them (task-1995).** The Windows target is built with
        native cargo, and `onig_sys` compiles oniguruma with `cl.exe`. cl.exe finds its headers
        through INCLUDE, LIB and PATH, which `vcvars64.bat` sets - a developer shell has run it, and
        an agent terminal, a scheduled task and a service have not. The failure names the header
        rather than the cause:

            regenc.h(39): fatal error C1083: Cannot open include file: 'stddef.h'

        vswhere.exe ships with every Visual Studio since 2017 and is always at the same absolute
        path, so the installation is found rather than guessed at. Nothing happens when INCLUDE is
        already set, so a developer shell is left exactly as it is.
    #>
    if ($env:INCLUDE) { return }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio' | Join-Path -ChildPath 'Installer' | Join-Path -ChildPath 'vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) {
        throw "INCLUDE is not set and $vswhere does not exist, so the MSVC environment cannot be found. Build from a Developer PowerShell, or install Visual Studio's C++ tools."
    }
    $install = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1)
    if (-not $install) { throw 'vswhere found no Visual Studio with the C++ tools installed.' }
    $vcvars = Join-Path $install 'VC' | Join-Path -ChildPath 'Auxiliary' | Join-Path -ChildPath 'Build' | Join-Path -ChildPath 'vcvars64.bat'
    if (-not (Test-Path -LiteralPath $vcvars)) { throw "$vcvars does not exist." }
    # `set` after the batch file prints the environment it produced; each line is copied in.
    & cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($Matches[1])" -Value $Matches[2] }
    }
    if (-not $env:INCLUDE) { throw "running $vcvars did not set INCLUDE." }
    Write-Host "  MSVC environment from $install"
}

function Get-CrossBin {
    <#
    .SYNOPSIS
        Where zig, rcodesign, minisign and nfpm are.

    .DESCRIPTION
        **It is not always inside this checkout (task-1995).** The toolchain is gitignored - a few
        gigabytes of downloaded zig, rcodesign and the macOS SDK, not source - so a `git worktree`
        has an empty `tools/cross/bin`, and a release is normally cut from a worktree because the
        ordinary checkout is where work happens and is frequently dirty. Every script that reached
        for a tool then stopped with "run fetch-toolchain", which reads as a machine that was never
        set up rather than as a checkout that shares one.

        INILLUCENT_CROSS_BIN wins, then this checkout if it has anything in it, then the repository
        the worktree belongs to - `git rev-parse --git-common-dir` names its .git from inside any
        worktree.

    .PARAMETER Root
        The repository root the caller is working in.
    #>
    param([string] $Root)
    if ($env:INILLUCENT_CROSS_BIN) { return $env:INILLUCENT_CROSS_BIN }
    $here = Join-Path $Root 'tools/cross/bin'
    if (Get-ChildItem -Path $here -File -ErrorAction SilentlyContinue) { return $here }
    $commonDir = (& git -C $Root rev-parse --git-common-dir 2>$null)
    if ($commonDir) {
        $resolved = if ([System.IO.Path]::IsPathRooted($commonDir)) { $commonDir } else { Join-Path $Root $commonDir }
        $shared = Join-Path (Split-Path -Parent ([System.IO.Path]::GetFullPath($resolved))) 'tools/cross/bin'
        if (Get-ChildItem -Path $shared -File -ErrorAction SilentlyContinue) { return $shared }
    }
    return $here
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

    .PARAMETER Version
        Limit the file to this release's artifacts. Omitted, every archive in Dist is named.

    .NOTES
        **Written with LF, not CRLF (task-1932, H12).** `Set-Content` ends every
        line the way Windows does, and every program that reads this file runs
        somewhere else: `packaging/install.sh` had to add `tr -d '\r'` to stop
        awk keeping the carriage return in the archive name, and a reader
        without that workaround reports a correct download as unpublished.
        `PUBLISHING.md` records this as fixed in `release.ps1`; it was not fixed
        here, which is the copy every archive's checksum actually comes from.

        **provenance.json goes in too (task-1951).** It did not, and the effect
        was silent: `release.ps1` and `release.sh` both write the archives and
        then append the provenance's hash, and every caller of this function
        rewrote the file over the archives alone and dropped that line. So
        running `packaging/release-all.ps1` or `packaging/linux/package-linux.ps1`
        after a release turned a four line SHA256SUMS into a three line one, and
        `packaging/publish-site.ps1` copies this file to the site verbatim. The
        site's own comment says provenance.json is published because SHA256SUMS
        names it; without the line, the provenance is served with nothing
        stating its hash, and a downloader who verified the archive has not
        verified the claims about it. Measured on 2026-09-14: `release-all.ps1
        -Targets macos` rewrote a file that matched the published one into one
        that did not.
    #>
    param([string] $Dist, [string] $Version)
    $sums = Join-Path $Dist 'SHA256SUMS'
    $lines = @()
    # **.pkg is in the list because the site publishes one (task-1995).** The signed, notarised
    # macOS installer is the first thing the download page offers a Mac, and it was the one
    # published artifact with no line in this file - so `packaging/install.sh`, which verifies what
    # it downloaded against exactly this file, had nothing to check it against. Measured on the
    # live site for 0.1.3: five artifacts served, four hashes published.
    foreach ($pattern in @('*.zip', '*.tar.gz', '*.deb', '*.rpm', '*.pkg')) {
        Get-ChildItem -Path $Dist -Filter $pattern -File -ErrorAction SilentlyContinue |
            Sort-Object Name | ForEach-Object {
                # **Two ways this file came to name something nobody can download**, both measured
                # on the live 0.1.3 (task-1995), and both serious because `publish-site.ps1` copies
                # this file to the site verbatim and `packaging/install.sh` verifies against exactly
                # it. A name in here with no file behind it is indistinguishable, to anyone checking,
                # from a download that was tampered with.
                #
                # One: dist/ is not emptied between releases - this machine's held 0.1.1, 0.1.2 and
                # 0.1.3 archives at once - so without $Version the list spans every release while
                # the site holds one.
                #
                # Two: the macOS zip is the notary's container rather than a download. rcodesign
                # uploads a zip because Apple's notary takes an archive and not a directory, and the
                # site then publishes the .pkg and the .tar.gz.
                $mine = -not $Version -or $_.Name -like "*$Version*"
                if ($mine -and $_.Name -notlike '*-apple-darwin.zip') {
                    $lines += "$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLower())  $($_.Name)"
                }
            }
    }
    # Last, and by name rather than by pattern, because the release scripts
    # append it last and a downloader comparing two copies of this file should
    # not see the lines in a different order.
    $provenance = Join-Path $Dist 'provenance.json'
    if (Test-Path -LiteralPath $provenance) {
        # **Named only when it describes what is being published (task-1995).** provenance.json
        # states the version, commit, toolchain and the archive's own hash, and dist/ keeps the last
        # one written. The 0.1.3 release shipped a Windows zip of da39cba... while the provenance
        # beside it claimed f6d3fb..., because the archives were rebuilt from the tag and the
        # provenance was not - and the site was still serving the 0.1.2 provenance next to 0.1.3
        # downloads. Publishing either would put a signed, checksummed claim about the build behind
        # a different build, which is worse than publishing none.
        $claim = Get-Content -LiteralPath $provenance -Raw | ConvertFrom-Json
        $named = Join-Path $Dist $claim.archive.name
        $stale = @()
        if ($Version -and $claim.version -ne $Version) { $stale += "it describes $($claim.version)" }
        if (Test-Path -LiteralPath $named) {
            $actual = (Get-FileHash -LiteralPath $named -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $claim.archive.sha256) { $stale += "$($claim.archive.name) hashes to $($actual.Substring(0, 12))... and it claims $($claim.archive.sha256.Substring(0, 12))..." }
        }
        if ($stale.Count -gt 0) {
            Write-Host "  provenance.json is left out of SHA256SUMS: $($stale -join '; '). Re-run packaging/release.ps1 to write one for this build."
        } else {
            $lines += "$((Get-FileHash -LiteralPath $provenance -Algorithm SHA256).Hash.ToLower())  provenance.json"
        }
    }
    # The text is assembled and written whole, because there is no switch on
    # Set-Content that changes the line ending it uses.
    $text = ($lines -join "`n")
    if ($lines.Count -gt 0) { $text += "`n" }
    [System.IO.File]::WriteAllText($sums, $text, (New-Object System.Text.UTF8Encoding($false)))
    return $sums
}
