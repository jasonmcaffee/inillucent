# Packaging

Everything that turns a build into something somebody can install.

```
packaging/
  release.ps1  release.sh      build the archive every installer is made from
  install.ps1  install.sh      install it, on Windows / on macOS and Linux
  cargo-publish.ps1            publish the workspace to crates.io
  windows/README.md            why there is no MSI, and what one would take
  macos/                       the .pkg: build, sign, notarise, and what each costs
  homebrew/                    the formula, and a script that fills its checksums
packages/
  npm/                         the `inillucent` package and its four platform packages
  python/                      the PyPI wheel: binaries plus the real driver binding
  go/                          a Go package, and `go install` as a way to get the binaries
  php/                         a Composer package (its composer.json is at the repo root)
```

## The shape of it

**One archive, six ways of delivering it.** `release.ps1`/`release.sh` produce

```
dist/inillucent-<version>-<target>/
    bin/     inillucent  inillucent-shell  inillucent-mcp  inillucent-migrate
    lib/     the C ABI shared library
    include/ inillucent_driver.h
    docs/    the reference pages README.md links into
    tests/   the synthetic corpus recipe and the testing standard
    agent-skills/ one page per job, for an AI agent
    README.md  AGENTS.md  DRIVER.md  LICENSE  VERSION
dist/inillucent-<version>-<target>.zip   (or .tar.gz)
dist/provenance.json
dist/SHA256SUMS
```

and every package below is a different way of getting that directory onto a
machine. Six packaging systems with six *builds* would be six things that can
differ; six wrappers around one build cannot.

The one exception is `cargo install inillucent-cli`, which builds from source
because that is what cargo does. It is also the fallback the other five point at
when a platform has no prebuilt archive.

## What a release refuses, and why (task-1894)

Before task-1894 the release scripts staged whatever was in the working tree and
labelled it with whatever `--version` said. Four things were possible and none
of them was detectable from the archive afterwards:

- a release built from a **dirty checkout**, so the commit it records is not the
  code inside it;
- `--version 2.0.0` on a tree whose binaries answer `0.1.0`;
- a **tag pointing at different code** than the commit that was built;
- an archive **nobody opened** — a release that has only been checked for having
  produced an archive.

So a release now refuses unless the checkout is clean, `v<version>` exists and
points at HEAD, the compiler is the one `rust-toolchain.toml` pins, and the
staged archive **installs into an empty directory and works**. The smoke test
runs the installed copy rather than `target/release`, because a library found by
being beside the build passes there and fails on somebody's machine. It:

1. runs `inillucent --version` and checks it answers with the version on the tin;
2. creates a database, writes, **reopens** and reads — reopened, because a write
   that never reached the file passes every check that does not close first;
3. drives `inillucent-mcp` over JSON-RPC through `initialize`, `tools/list` and a
   `tools/call`, which is the surface an agent is handed and the one nothing
   else tests;
4. compiles a C program against the **shipped** header and links the **shipped**
   library, which is what every binding that is not Rust does with this archive.

Each refusal has a named override — `-AllowDirty`, `-AllowUntagged`,
`-AllowVersionMismatch`, `-SkipSmoke` — because a refusal nobody can get past on
a bad afternoon is a refusal somebody deletes. **Every override used is recorded
in `provenance.json`**, which `SHA256SUMS` covers, so a release made with one
says so and a downloader who verified the archive has verified the claims about
it too.

`--smoke-only` builds, stages and smokes without any of the tag or
clean-checkout requirements. That is what CI runs on every commit, so the
packaging is checked continuously rather than once at a tag.

## Cutting a release

Two machines, and each does only what it alone can do. The Windows box builds
Windows and Linux, packages everything, signs the checksums and publishes the
site. The MacBook builds, signs and notarises macOS, because Apple's linker,
`codesign` and `notarytool` run nowhere else.

```powershell
# --- on the Windows box -------------------------------------------------
pwsh tools/cross/fetch-toolchain.ps1        # once: zig, cargo-zigbuild, rcodesign, nfpm, minisign
pwsh packaging/release-all.ps1              # Windows and both Linux architectures
pwsh packaging/linux/package-linux.ps1      # the .deb and the .rpm, signed
```

```sh
# --- on the MacBook -----------------------------------------------------
./packaging/macos/release-macos.sh --version 0.1.0 --upload
```

```powershell
# --- back on the Windows box --------------------------------------------
pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.0   # collect and verify what the Mac made
pwsh packaging/sign-sums.ps1                              # minisign over SHA256SUMS
bash tools/release-verify-linux.sh --version 0.1.0        # from WSL
pwsh packaging/publish-site.ps1 -Version 0.1.0 -Stage     # on the site, not yet linked
```

Then, on any Mac, against the bytes the site is now serving:

```sh
curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version 0.1.0
```

and only once that passes:

```powershell
pwsh packaging/publish-site.ps1 -Version 0.1.0 -Link
```

The order is the point. A download link that points at an artifact nobody has
run is worse than no link, so the artifacts are staged where the verifier can
reach them before anything on the site mentions them.

**The distribution point is inillucent.com, not GitHub.** The repository is
private, and a private repository's release assets are private too: an
unauthenticated request for one answers 404. GitHub is used for exactly one
thing here, which is carrying the macOS artifacts from the MacBook to the
Windows box, and `packaging/fetch-macos-artifacts.ps1 -FromDirectory` skips even
that when the two machines are on the same network.

## The two rules every installer here follows

**Verify the checksum.** Every downloader - `install.ps1`, `install.sh`, the Go
`cmd/inillucent`, the PHP `bin/inillucent-install` - reads the release's own
`SHA256SUMS` and refuses an archive that does not match. A downloader that skips
this has turned a truncated or tampered transfer into an installed program, and
it is four lines to not do that.

**Never install outside the user's own space unless asked.** `install.ps1` writes
to `%LOCALAPPDATA%\Programs\inillucent` and the user `PATH`; `install.sh` writes
to `~/.local`. Neither needs elevation and neither touches a machine-wide
location. The macOS `.pkg` is the exception, because a `.pkg` installs to
`/usr/local` and asks for the password itself.

## What is not automated, and why

- **Running a macOS binary.** Everything else about a macOS build can be checked
  from Windows - the architectures, the signature, the hardened runtime, the
  timestamp - but whether it runs cannot be, and neither can whether Gatekeeper
  accepts it. `packaging/macos/verify-macos.sh` is both of those checks and it is
  the release gate.
- **The registry uploads.** Every registry needs an interactive login: a browser,
  an OAuth redirect, and for PyPI a mandatory second factor. That is deliberate on
  their part and it is what stops somebody else publishing under your name.
  `PUBLISHING.md` records where each one stands.
- **A Linux `manylinux` wheel.** PyPI refuses a plain `linux_x86_64` wheel; the
  Linux one has to be built in a `manylinux` container. `packages/python/build.py`
  says so rather than uploading something that will be rejected after the fact.

What *used* to be here, and is not any more: cross-compiling and signing. Both
are automated now. `cargo-zigbuild` builds every Linux target on the Windows box
with a chosen glibc floor of 2.28, and the macOS half is a single command on the
MacBook.
