<#
.SYNOPSIS
    Fetches the five programs a release is cut with, and verifies each against a
    pinned SHA-256.

.DESCRIPTION
    A release needs a cross compiler, an Apple signer, a Linux packager and two
    signature tools, and none of them is installed on a developer machine by
    default. Rather than a page of instructions that go stale, this fetches
    exactly the versions the release was tested with into tools/cross/bin/.

        zig 0.15.2               the linker cargo-zigbuild drives
        cargo-zigbuild 0.23.4    cross compiles to Linux and macOS
        rcodesign 0.29.0         Apple signing, notarisation and lipo
        nfpm 2.47.0              .deb and .rpm, on Windows, with no dpkg
        minisign 0.12            the detached signature over SHA256SUMS

    Every download is checked against the hash recorded below before it is
    unpacked. The hashes are pinned rather than read from the publisher, because
    a checksum a publisher serves beside the file it describes proves only that
    the two came from the same place.

    tools/cross/bin/ is ignored by git: these are somebody else's binaries and
    they are re-fetchable in a minute.

.PARAMETER Force
    Re-download even when the program is already present and its hash matches.

.EXAMPLE
    pwsh tools/cross/fetch-toolchain.ps1
#>
[CmdletBinding()]
param([switch] $Force)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$bin = Join-Path $here 'bin'
$cache = Join-Path $here 'cache'

# name, url, sha256, and the path inside the archive that is wanted. `Extract`
# is a directory when the program needs the files beside it (zig carries its own
# lib/ and will not run without it) and a single file otherwise.
$tools = @(
    @{
        Name     = 'zig'
        Url      = 'https://ziglang.org/download/0.15.2/zig-x86_64-windows-0.15.2.zip'
        Sha256   = '3a0ed1e8799a2f8ce2a6e6290a9ff22e6906f8227865911fb7ddedc3cc14cb0c'
        Inner    = 'zig-x86_64-windows-0.15.2'
        Produces = 'zig/zig.exe'
    },
    @{
        Name     = 'cargo-zigbuild'
        Url      = 'https://github.com/rust-cross/cargo-zigbuild/releases/download/v0.23.4/cargo-zigbuild-x86_64-pc-windows-msvc.zip'
        Sha256   = 'cd1226091f9f99ac7b46fb413968fb0b46edbe3ea961817d31b968352de4d4a6'
        Inner    = 'cargo-zigbuild.exe'
        Produces = 'cargo-zigbuild.exe'
    },
    @{
        Name     = 'rcodesign'
        Url      = 'https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F0.29.0/apple-codesign-0.29.0-x86_64-pc-windows-msvc.zip'
        Sha256   = '54bb500e2da7a8de02fcae0f331d1cac6e6d7173b4281042ff9c528ba3159aaa'
        Inner    = 'apple-codesign-0.29.0-x86_64-pc-windows-msvc/rcodesign.exe'
        Produces = 'rcodesign.exe'
    },
    @{
        Name     = 'nfpm'
        Url      = 'https://github.com/goreleaser/nfpm/releases/download/v2.47.0/nfpm_2.47.0_Windows_x86_64.zip'
        Sha256   = '788f88a3bba0d89baa639aba54ba28b384958878700d440ce199a7ff4a567f11'
        Inner    = 'nfpm.exe'
        Produces = 'nfpm.exe'
    },
    @{
        Name     = 'minisign'
        Url      = 'https://github.com/jedisct1/minisign/releases/download/0.12/minisign-0.12-win64.zip'
        Sha256   = '37b600344e20c19314b2e82813db2bfdcc408b77b876f7727889dbd46d539479'
        Inner    = 'minisign-win64/x86_64/minisign.exe'
        Produces = 'minisign.exe'
    }
)

function Get-Verified {
    <#
    .SYNOPSIS
        Downloads one archive and refuses it unless its SHA-256 is the pinned one.

    .PARAMETER Url
        Where to fetch it from.

    .PARAMETER Sha256
        The hash the file must have.

    .PARAMETER Into
        The file to write.
    #>
    param([string] $Url, [string] $Sha256, [string] $Into)

    if (-not (Test-Path -LiteralPath $Into)) {
        Write-Host "  downloading $Url"
        Invoke-WebRequest -Uri $Url -OutFile $Into -UseBasicParsing
    }
    $actual = (Get-FileHash -LiteralPath $Into -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $Sha256.ToLower()) {
        Remove-Item -LiteralPath $Into -Force -Confirm:$false
        throw "$Url has SHA-256 $actual, expected $Sha256. The download was deleted."
    }
}

function Expand-One {
    <#
    .SYNOPSIS
        Pulls one entry out of a zip and puts it where the release scripts look.

    .PARAMETER Archive
        The downloaded zip.

    .PARAMETER Inner
        The file or directory inside it that is wanted.

    .PARAMETER Destination
        Where it lands under tools/cross/bin.
    #>
    param([string] $Archive, [string] $Inner, [string] $Destination)

    $temp = Join-Path $cache ("unpack-" + [System.IO.Path]::GetFileNameWithoutExtension($Archive))
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force -Confirm:$false }
    Expand-Archive -LiteralPath $Archive -DestinationPath $temp -Force

    $source = Join-Path $temp $Inner
    if (-not (Test-Path -LiteralPath $source)) { throw "$Archive does not contain $Inner" }

    $parent = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    if (Test-Path -LiteralPath $Destination) { Remove-Item -LiteralPath $Destination -Recurse -Force -Confirm:$false }
    Move-Item -LiteralPath $source -Destination $Destination
    Remove-Item -LiteralPath $temp -Recurse -Force -Confirm:$false
}

New-Item -ItemType Directory -Force -Path $bin, $cache | Out-Null

foreach ($tool in $tools) {
    $produced = Join-Path $bin $tool.Produces
    if ((Test-Path -LiteralPath $produced) -and -not $Force) {
        Write-Host "$($tool.Name): already present"
        continue
    }
    Write-Host "$($tool.Name):"
    $archive = Join-Path $cache ("$($tool.Name).zip")
    Get-Verified -Url $tool.Url -Sha256 $tool.Sha256 -Into $archive

    # zig is a directory because it loads its own lib/ at run time; the rest are
    # single executables.
    $destination = if ($tool.Produces -like '*/*') {
        Join-Path $bin (Split-Path -Parent $tool.Produces)
    } else {
        $produced
    }
    Expand-One -Archive $archive -Inner $tool.Inner -Destination $destination
    Write-Host "  -> $produced"
}

Write-Host ''
Write-Host "the release toolchain is in $bin"
