# Windows packaging

`packaging/install.ps1` installs inillucent on Windows. It downloads the release archive for the
machine's architecture, checks its SHA-256 against the release's `SHA256SUMS`, unpacks it into
`%LOCALAPPDATA%\Programs\inillucent`, and adds its `bin` folder to the user `PATH`. It needs no
administrator rights.

```powershell
irm https://inillucent.com/downloads/install.ps1 | iex
```

| Option | What it does |
|---|---|
| `-Version` | installs that version instead of the latest |
| `-FromDist` | installs the archive `packaging/release.ps1` just built, to test the script before a release exists |
| `-Prefix` | installs into another folder |
| `-NoPath` | leaves `PATH` unchanged |
| `-Uninstall` | removes the install and its `PATH` entry |

## Why there is no MSI

An MSI installer would need three things:

1. **The WiX toolset in the build.** That means `dotnet tool install --global wix` and a `.wxs` file
   that describes the component, the folder, the `PATH` entry and the upgrade code. It is a second
   build system for one file.
2. **A code signing certificate.** Without one, SmartScreen shows "Windows protected your PC" the
   first time the installer runs. An OV certificate costs about $200 to $400 a year and needs an
   organisation identity. An EV certificate costs more and needs a hardware token. The macOS `.pkg`
   has the same cost (see `../macos/README.md`).
3. **Administrator rights**, because an MSI that writes to `Program Files` needs them.

For a command line tool, none of these helps compared with a folder on the user's `PATH`. `rustup`,
`gh`, `deno` and `bun` install on Windows the same way this script does.

An MSI becomes necessary when someone wants to deploy inillucent with Group Policy or Intune, which
accept only an MSI. The certificate is then needed anyway, and writing the `.wxs` file is a small
job.

## How a signed build would work

The file layout would not change. The four programs and the DLL would be signed after
`packaging/release.ps1` stages them and before it makes the archive:

```powershell
$files = Get-ChildItem -Path dist/inillucent-*/bin, dist/inillucent-*/lib -File
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /a $files
```

`/a` picks the certificate from the personal certificate store. The certificate is installed once on
the release machine, and the command names no thumbprint.

## Architectures

The release builds `x86_64-pc-windows-msvc`. The installer already asks for
`aarch64-pc-windows-msvc` on an ARM machine, and building it only needs `rustup target add`. It is
not built because there is no ARM Windows machine to test it on before it is published.
