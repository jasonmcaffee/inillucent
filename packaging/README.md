# Packaging

Everything that turns a build into something somebody can install.

```
packaging/
  release.ps1  release.sh      build the archive every installer is made from
  install.ps1  install.sh      install it, on Windows / on macOS and Linux
  cargo-publish.ps1            publish the workspace to crates.io
  sign-sums.ps1                the minisign signature over SHA256SUMS
  mirror-github.ps1            build the public mirror's commit for a release
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

One machine. The Windows box builds Windows, both Linux architectures and both
Apple architectures, signs and notarises macOS, packages everything, signs the
checksums and publishes the site.

That was not true until task-1995. The macOS half used to run on a MacBook,
because `lipo`, `codesign`, `pkgbuild`, `productbuild`, `notarytool` and
`stapler` are macOS programs. Each of them now has a replacement that runs here:
`rcodesign` for five of them, `tools/macos-pkg` for the two that build the
`.pkg`. Apple's notary service is an HTTPS API and answers a Windows client the
same way it answers a Mac. `packaging/macos/README.md` is the detail, including
how the Developer ID certificates are obtained without a Mac.

```powershell
pwsh tools/cross/fetch-toolchain.ps1        # once: zig, cargo-zigbuild, rcodesign, nfpm, minisign
pwsh packaging/release-all.ps1              # every target, including the signed and notarised macOS half
pwsh packaging/linux/package-linux.ps1      # the .deb and the .rpm, signed
pwsh packaging/sign-sums.ps1                # minisign over SHA256SUMS; needs packaging/inillucent.pub
bash tools/release-verify-linux.sh --version 0.1.4   # from WSL
pwsh packaging/publish-site.ps1 -Version 0.1.4 -Stage  # on the site, not yet linked
```

`0.1.4` throughout this section is an example. Pass the version being cut.
**0.1.0 was withdrawn** — `PUBLISHING.md` says why — so that one is never a
version to pass here.

The five targets `release-all.ps1` needs are named in `rust-toolchain.toml`,
which is what installs them; the two Apple ones were missing from that list until
task-1951, so on a machine holding only the pinned toolchain the default run
stopped at its macOS step with `error[E0463]: can't find crate for std`.

`packaging/macos/release-macos.ps1` runs on its own too, for a macOS-only
rebuild, and it prints at the end which checks it ran and which four it could
not. The four it cannot run are the ones that execute a Mach-O, and no tooling
changes that — see "What cannot be checked here" below.

**The Mac path still works and is still in the repository.**
`packaging/macos/release-macos.sh` produces the same four artifacts on a Mac,
and `packaging/fetch-macos-artifacts.ps1` still collects and verifies them. A
machine with a Mac available loses nothing; a machine without one is no longer
stopped.

Then, on any Mac, against the bytes the site is now serving:

```sh
curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version 0.1.2
```

and only once that passes:

```powershell
pwsh packaging/publish-site.ps1 -Version 0.1.2 -Link
```

The order is the point. A download link that points at an artifact nobody has
run is worse than no link, so the artifacts are staged where the verifier can
reach them before anything on the site mentions them.

**The distribution point is inillucent.com.** Both GitHub repositories are
private, and a private repository's release assets are private too: an
unauthenticated request for one answers 404, which was checked with no credential
of any kind in task-1951. So GitHub carries nothing a user downloads. It used to
be the transport that carried the macOS artifacts from the MacBook to the Windows
box; since task-1995 there is nothing to carry, because the machine that builds
them is the machine that publishes them.

One route does not survive that: `go install` resolves through
`proxy.golang.org`, which clones the repository with no credential and gets a
404, so the Go package cannot be installed by anybody. `node tools/check-public-urls.mjs`
reports every link in the shipped packages that a signed-out reader cannot open,
and `packaging/mirror-github.ps1` builds the public mirror commit for a release.
`PUBLISHING.md` has the decision that goes with them.

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

- **Running a macOS binary.** Everything else about a macOS build is checked
  from Windows - the architectures, the signature, the hardened runtime, the
  timestamp, and the package's own structure - but whether it *runs* cannot be,
  and neither can whether Gatekeeper accepts it. Two things stand in for that.
  Apple's notary service unpacks the submission, walks every Mach-O in it and
  rejects an unsigned binary, a missing hardened runtime, a missing timestamp or
  a package it cannot parse, so an `Accepted` is a statement by Apple about the
  exact bytes submitted. And `packaging/macos/verify-macos.sh` still exists, runs
  against the published bytes, and is a one-line check on any Mac that can be
  borrowed.
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
same machine.
