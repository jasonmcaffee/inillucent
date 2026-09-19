<#
.SYNOPSIS
    Finds the Apple signing certificates and the notary key, and hands them to
    rcodesign without a private key ever sitting in plain text on a disk or a
    password ever appearing in an argument list.

.DESCRIPTION
    Dot-sourced by packaging/macos/release-macos.ps1 and by
    packaging/macos/new-apple-csr.ps1. Nothing here runs on import; it defines
    functions and returns.

    WHAT LIVES OUTSIDE THE REPOSITORY

        %LOCALAPPDATA%\inillucent\apple\
            developer-id-application.key.sealed   the private key, sealed by DPAPI
            developer-id-application.cer          Apple's certificate, not a secret
            developer-id-installer.key.sealed     the same pair for the .pkg
            developer-id-installer.cer
            notary-key.json.sealed                the App Store Connect API key, sealed

    $env:INILLUCENT_APPLE_DIR overrides the directory, which is how a second
    machine or a test uses a different set.

    A .p12 exported from a Mac's keychain works too, as developer-id-<kind>.p12
    beside a developer-id-<kind>.p12.pw holding its sealed password. Both routes
    end at the same rcodesign arguments, so the release does not care which one
    a machine has.

    WHY DPAPI

    `ConvertFrom-SecureString` encrypts under the current Windows account, so
    what is on disk is worthless on another machine or to another user, and
    there is no key of its own to store somewhere else. It is built into
    PowerShell, so nothing has to be installed and nothing has to be kept in
    sync. It is the same mechanism the ai-service secret store uses.

    THE PLAIN TEXT WINDOW

    rcodesign reads a private key from a file, so for the seconds it is signing,
    the key is a file. It is written to the RAM disk at R:\, which is memory:
    nothing about it survives a reboot and nothing about it lands on a disk that
    could be read afterwards. Every caller deletes it in a `finally`.

.NOTES
    A machine with no certificates yet is not an error here. Each function names
    the file that is missing and the command that creates it, because the
    alternative is a release that fails part way through with a message from
    rcodesign about a file nobody named.
#>

$ErrorActionPreference = 'Stop'

function Get-AppleCredentialDir {
    <#
    .SYNOPSIS
        The directory holding the certificates and the notary key.
    #>
    if ($env:INILLUCENT_APPLE_DIR) { return $env:INILLUCENT_APPLE_DIR }
    return (Join-Path $env:LOCALAPPDATA 'inillucent\apple')
}

function Get-AppleScratchDir {
    <#
    .SYNOPSIS
        Where an unsealed secret is written for the seconds rcodesign needs it.

    .DESCRIPTION
        R:\ is a RAM disk on this machine. A machine without one falls back to
        the temp directory, which is a worse guarantee and is why the fallback
        says so rather than being silent.
    #>
    $scratch = if (Test-Path -LiteralPath 'R:\') {
        'R:\inillucent-release'
    } else {
        Write-Warning 'no RAM disk at R:\; unsealed key material will be written to the temp directory instead'
        Join-Path $env:TEMP 'inillucent-release'
    }
    New-Item -ItemType Directory -Force -Path $scratch | Out-Null
    return $scratch
}

function Protect-AppleSecret {
    <#
    .SYNOPSIS
        Seals a string with DPAPI, under this Windows account.

    .PARAMETER Value
        The text to seal, such as a PEM private key or a .p12 password.

    .PARAMETER Path
        Where the sealed form is written.
    #>
    param([string] $Value, [string] $Path)
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    $secure = ConvertTo-SecureString -String $Value -AsPlainText -Force
    ConvertFrom-SecureString -SecureString $secure | Set-Content -Path $Path -NoNewline
}

function Unprotect-AppleSecret {
    <#
    .SYNOPSIS
        Reads a sealed file back.

    .PARAMETER Path
        The sealed file.
    #>
    param([string] $Path)
    $secure = Get-Content -Path $Path -Raw | ConvertTo-SecureString
    return [System.Net.NetworkCredential]::new('', $secure).Password
}

function Set-AppleSigningKey {
    <#
    .SYNOPSIS
        Seals a PEM private key into the credential directory and removes the
        plain text copy.

    .PARAMETER Kind
        `application` for the programs and the library, `installer` for the .pkg.

    .PARAMETER PemPath
        The PEM file holding the private key.
    #>
    param(
        [ValidateSet('application', 'installer')] [string] $Kind,
        [string] $PemPath
    )
    if (-not (Test-Path -LiteralPath $PemPath)) { throw "$PemPath does not exist" }
    $sealed = Join-Path (Get-AppleCredentialDir) "developer-id-$Kind.key.sealed"
    Protect-AppleSecret -Value (Get-Content -Path $PemPath -Raw) -Path $sealed
    Remove-Item -LiteralPath $PemPath -Force -Confirm:$false
    Write-Host "sealed to $sealed, and the plain text key at $PemPath was removed"
}

function Set-AppleP12Password {
    <#
    .SYNOPSIS
        Seals the password of a .p12 exported from a Mac's keychain.

    .DESCRIPTION
        Prompts, so the password is never in an argument list, a script, or the
        PowerShell history.

    .PARAMETER Kind
        `application` or `installer`.
    #>
    param([ValidateSet('application', 'installer')] [string] $Kind)
    $secure = Read-Host -AsSecureString "password for developer-id-$Kind.p12"
    $path = Join-Path (Get-AppleCredentialDir) "developer-id-$Kind.p12.pw"
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
    ConvertFrom-SecureString -SecureString $secure | Set-Content -Path $path -NoNewline
    Write-Host "sealed to $path"
}

function Test-AppleCredentials {
    <#
    .SYNOPSIS
        Reports which credentials are present, without unsealing any of them.

    .DESCRIPTION
        The release calls this first, so a missing certificate stops it before
        the compile rather than after it.
    #>
    $dir = Get-AppleCredentialDir
    $kinds = @{}
    foreach ($kind in @('application', 'installer')) {
        $kinds[$kind] = (Test-Path -LiteralPath (Join-Path $dir "developer-id-$kind.key.sealed")) `
            -or (Test-Path -LiteralPath (Join-Path $dir "developer-id-$kind.p12"))
    }
    return [pscustomobject]@{
        Directory   = $dir
        Application = $kinds['application']
        Installer   = $kinds['installer']
        NotaryKey   = (Test-Path -LiteralPath (Join-Path $dir 'notary-key.json.sealed')) `
            -or (Test-Path -LiteralPath (Join-Path $dir 'notary-key.json'))
    }
}

function New-AppleSigningSession {
    <#
    .SYNOPSIS
        Unseals one identity and returns the rcodesign arguments that use it.

    .DESCRIPTION
        The returned object carries `Arguments`, which is spliced into an
        rcodesign call, and `Scratch`, the list of files the caller must delete
        in a `finally`. Close it with Remove-AppleSigningSession.

    .PARAMETER Kind
        `application` or `installer`.
    #>
    param([ValidateSet('application', 'installer')] [string] $Kind)

    $dir = Get-AppleCredentialDir
    $scratch = Get-AppleScratchDir
    $stamp = [guid]::NewGuid().ToString('N')

    $p12 = Join-Path $dir "developer-id-$Kind.p12"
    if (Test-Path -LiteralPath $p12) {
        $sealedPassword = "$p12.pw"
        if (-not (Test-Path -LiteralPath $sealedPassword)) {
            throw "$p12 has no sealed password. Run: pwsh -c `". packaging/macos/apple-credentials.ps1; Set-AppleP12Password -Kind $Kind`""
        }
        $passwordFile = Join-Path $scratch "p12-$Kind-$stamp.txt"
        Set-Content -Path $passwordFile -Value (Unprotect-AppleSecret -Path $sealedPassword) -NoNewline
        return [pscustomobject]@{
            Arguments = @('--p12-file', $p12, '--p12-password-file', $passwordFile)
            Scratch   = @($passwordFile)
        }
    }

    $sealedKey = Join-Path $dir "developer-id-$Kind.key.sealed"
    $certificate = Join-Path $dir "developer-id-$Kind.cer"
    if (-not (Test-Path -LiteralPath $sealedKey)) {
        throw "no Developer ID $Kind key at $sealedKey. Run: pwsh packaging/macos/new-apple-csr.ps1 -Kind $Kind"
    }
    if (-not (Test-Path -LiteralPath $certificate)) {
        throw "no certificate at $certificate. It is the .cer downloaded from developer.apple.com for the request new-apple-csr.ps1 wrote."
    }

    $keyFile = Join-Path $scratch "key-$Kind-$stamp.pem"
    Set-Content -Path $keyFile -Value (Unprotect-AppleSecret -Path $sealedKey) -NoNewline
    return [pscustomobject]@{
        Arguments = @('--pem-file', $keyFile, '--certificate-der-file', $certificate)
        Scratch   = @($keyFile)
    }
}

function Remove-AppleSigningSession {
    <#
    .SYNOPSIS
        Deletes whatever New-AppleSigningSession unsealed.

    .PARAMETER Session
        The object it returned. A null session is accepted, so this can be
        called from a `finally` that may run before the session was opened.
    #>
    param([object] $Session)
    if ($null -eq $Session) { return }
    foreach ($path in $Session.Scratch) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -Confirm:$false }
    }
}

function New-AppleNotarySession {
    <#
    .SYNOPSIS
        Unseals the App Store Connect key rcodesign notarises with, and returns
        its path plus what has to be deleted afterwards.

    .DESCRIPTION
        The file holds an ECDSA private key, so it is kept sealed with DPAPI and
        exists in the clear only on the RAM disk for the length of a submission,
        the same way the signing key does. Close it with
        Remove-AppleNotarySession.

        The three values it is made from come from appstoreconnect.apple.com,
        Users and Access, Integrations, and are folded into one file with:

            rcodesign encode-app-store-connect-api-key -o notary-key.json `
              <issuer-id> <key-id> AuthKey_<key-id>.p8

        which is then sealed with Protect-AppleSecret. An App Store Connect key
        is used rather than an Apple ID and an app-specific password because it
        is revocable on its own and does not carry the password to the Apple ID
        itself.
    #>
    $dir = Get-AppleCredentialDir
    $sealed = Join-Path $dir 'notary-key.json.sealed'
    if (Test-Path -LiteralPath $sealed) {
        $path = Join-Path (Get-AppleScratchDir) ('notary-' + [guid]::NewGuid().ToString('N') + '.json')
        Set-Content -Path $path -Value (Unprotect-AppleSecret -Path $sealed) -NoNewline
        return [pscustomobject]@{ Path = $path; Scratch = @($path) }
    }

    # A plain notary-key.json is still accepted, because that is what
    # `rcodesign encode-app-store-connect-api-key` writes and a machine may have
    # one already. It is not deleted afterwards, because it was not created here.
    $plain = Join-Path $dir 'notary-key.json'
    if (Test-Path -LiteralPath $plain) {
        Write-Warning "$plain holds an unsealed private key. Seal it with Protect-AppleSecret and delete it."
        return [pscustomobject]@{ Path = $plain; Scratch = @() }
    }

    throw "no notary key in $dir. packaging/macos/README.md has the three values it is made from."
}

function Remove-AppleNotarySession {
    <#
    .SYNOPSIS
        Deletes whatever New-AppleNotarySession unsealed.

    .PARAMETER Session
        The object it returned; a null session is accepted, so this can be
        called from a `finally` that may run before the session was opened.
    #>
    param([object] $Session)
    if ($null -eq $Session) { return }
    foreach ($path in $Session.Scratch) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -Confirm:$false }
    }
}
