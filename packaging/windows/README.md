# Windows packaging

`packaging/install.ps1` is the installer. It downloads the release archive for
the machine's architecture, checks its SHA-256 against the release's
`SHA256SUMS`, unpacks it into `%LOCALAPPDATA%\Programs\inillucent`, and puts
`bin` on the user `PATH`.

```powershell
irm https://raw.githubusercontent.com/Black-Rainbow-Labs/Inillucent/main/packaging/install.ps1 | iex
```

`-FromDist` installs the archive `packaging/release.ps1` just built, which is
how the script is tested before a release exists. `-Uninstall` reverses it,
`PATH` entry included. `-Prefix` puts it somewhere else. `-NoPath` leaves the
environment alone.

## Why there is no MSI

An MSI is the obvious thing to reach for and it is worth writing down why it is
not here, so the decision can be revisited with its reasons in front of whoever
revisits it rather than being made twice.

An MSI would need:

1. **The WiX toolset in the build.** `dotnet tool install --global wix` plus a
   `.wxs` authoring the component, the directory, the `PATH` fragment and the
   upgrade code. That is a second build system for one artifact.
2. **A code-signing certificate.** Without one, SmartScreen shows *"Windows
   protected your PC"* on first run and the installer looks like malware. An OV
   certificate is roughly $200-400 a year and needs an organisation identity;
   an EV one is more and needs a hardware token. This is the real cost, and it
   is the same cost the macOS `.pkg` has (see `../macos/README.md`).
3. **Elevation**, because an MSI writing to `Program Files` needs it - which
   turns a command a person can run into a command a person has to approve.

For a command-line tool, all three buy nothing over a per-user directory on
`PATH`. `rustup`, `gh`, `deno` and `bun` all install on Windows exactly the way
this script does, and none of them ships an MSI as its primary route.

**When it would become worth it**: when somebody wants inillucent deployed by
Group Policy or Intune, which take an MSI and nothing else. At that point the
certificate has to be bought anyway, and the `.wxs` is an afternoon.

## What a signed build would change

Nothing about the layout. Signing is applied to the four executables and the
DLL after `packaging/release.ps1` stages them and before the archive is made:

```powershell
$files = Get-ChildItem -Path dist/inillucent-*/bin, dist/inillucent-*/lib -File
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /a $files
```

`/a` picks the certificate from the personal store, so the certificate is
installed once on the machine that does releases and the command does not carry
a thumbprint anybody could paste into a public script.

## Architectures

`x86_64-pc-windows-msvc` is what the release builds today, and it is what this
box is. `aarch64-pc-windows-msvc` is a `rustup target add` away and the
installer already asks for it by name - the only reason it is not built is that
there is no ARM Windows machine here to run the result on before publishing it.
