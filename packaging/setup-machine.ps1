<#
.SYNOPSIS
    Sets the build settings that belong to a machine rather than to the repository: the linker, the
    compiler cache, and the antivirus exclusion.

.DESCRIPTION
    A committed `.cargo/config.toml` cannot carry these. The Tasks board keeps a project's own config
    file when it creates a ticket's worktree and writes its own beside it, and a committed one would
    be replaced there and lose the per ticket target directory. So each setting is a user environment
    variable that every new cargo process on this machine reads, set once by this script.

    -Linker     CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER to the toolchain's own rust-lld.exe.
                rust-lld links the same objects as link.exe, faster. Measured on the development box
                in the build time design: 15 percent faster on a cold test compile with the default
                debuginfo, about 7 percent on top of line tables only. The release build is linked by
                the same variable, and rust-lld is what rustc's own Windows builds use.

    -Sccache    RUSTC_WRAPPER=sccache, with SCCACHE_DIR on D: and a 50 GB cap. cargo compiles
                registry crates without incremental compilation, so sccache caches every third party
                library across worktrees: a new worktree's cold build then compiles only the
                workspace's own crates. Workspace crates keep incremental compilation and are not
                cached, which is right because they are what changed. Installs sccache with
                `cargo install` if it is missing.

    -Defender   Adds the worktree target directories to Windows Defender's exclusions. Needs an
                administrator shell, so the script prints the command when it is not one rather than
                failing. The one public measurement is 143 s to 92 s on a cargo build.

    -Remove     Takes every setting above back out.

    Nothing is set without a switch naming it, because each one changes every Rust build on the
    machine, other projects included.

.PARAMETER Linker
    Set the rust-lld linker.

.PARAMETER Sccache
    Set sccache as the compiler wrapper.

.PARAMETER Defender
    Exclude the build directories from Windows Defender.

.PARAMETER Remove
    Remove the linker and sccache settings.

.PARAMETER CacheDirectory
    Where sccache keeps its cache. Default D:/sccache.

.PARAMETER WhatIf
    Print what would change and change nothing.

.EXAMPLE
    pwsh packaging/setup-machine.ps1 -Linker -WhatIf
    pwsh packaging/setup-machine.ps1 -Linker -Sccache
    pwsh packaging/setup-machine.ps1 -Remove
#>
[CmdletBinding()]
param(
    [switch] $Linker,
    [switch] $Sccache,
    [switch] $Defender,
    [switch] $Remove,
    [string] $CacheDirectory = 'D:/sccache',
    [switch] $WhatIf
)

$ErrorActionPreference = 'Stop'

function Set-UserVariable {
    <#
    .SYNOPSIS
        Sets or clears one user environment variable, and this process's copy of it.

    .PARAMETER Name
        The variable.

    .PARAMETER Value
        The value; $null clears it.
    #>
    param([string] $Name, [string] $Value)
    $before = [Environment]::GetEnvironmentVariable($Name, 'User')
    if ($WhatIf) {
        Write-Host "would set $Name from '$before' to '$Value'"
        return
    }
    [Environment]::SetEnvironmentVariable($Name, $Value, 'User')
    Set-Item -Path "env:$Name" -Value $Value -ErrorAction SilentlyContinue
    Write-Host "$Name = '$Value' (was '$before')"
}

if ($Remove) {
    foreach ($name in @('CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER', 'RUSTC_WRAPPER', 'SCCACHE_DIR', 'SCCACHE_CACHE_SIZE')) {
        Set-UserVariable -Name $name -Value $null
    }
    return
}

if (-not ($Linker -or $Sccache -or $Defender)) {
    Write-Host 'nothing asked for: pass -Linker, -Sccache, -Defender or -Remove. See Get-Help for what each does.'
    return
}

if ($Linker) {
    $sysroot = (& rustc --print sysroot).Trim()
    $lld = Join-Path $sysroot 'lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe'
    if (-not (Test-Path -LiteralPath $lld)) { throw "the toolchain has no rust-lld at $lld" }
    Set-UserVariable -Name 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER' -Value $lld
}

if ($Sccache) {
    $found = Get-Command sccache -ErrorAction SilentlyContinue
    if (-not $found) {
        if ($WhatIf) {
            Write-Host 'would install sccache: cargo install sccache --locked'
        } else {
            & cargo install sccache --locked
            if ($LASTEXITCODE -ne 0) { throw 'cargo install sccache failed' }
            $found = Get-Command sccache -ErrorAction SilentlyContinue
            if (-not $found) { throw 'sccache installed and is not on PATH; open a new shell and run this again' }
        }
    }
    Set-UserVariable -Name 'RUSTC_WRAPPER' -Value 'sccache'
    Set-UserVariable -Name 'SCCACHE_DIR' -Value $CacheDirectory
    Set-UserVariable -Name 'SCCACHE_CACHE_SIZE' -Value '50G'
}

if ($Defender) {
    $paths = @('D:/agent-worktrees/cargo-target', 'J:/build')
    $administrator = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    $command = "Add-MpPreference -ExclusionPath $(($paths | ForEach-Object { "'$_'" }) -join ',')"
    if ($WhatIf -or -not $administrator) {
        Write-Host 'the Defender exclusion needs an administrator shell. Run this in one:'
        Write-Host "   $command"
    } else {
        Add-MpPreference -ExclusionPath $paths
        Write-Host "excluded from Defender: $($paths -join ', ')"
    }
}

Write-Host 'new shells and new agent terminals read these; a shell that is already open does not.'
