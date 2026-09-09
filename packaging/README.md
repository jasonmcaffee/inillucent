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
dist/SHA256SUMS
```

and every package below is a different way of getting that directory onto a
machine. Six packaging systems with six *builds* would be six things that can
differ; six wrappers around one build cannot.

The one exception is `cargo install inillucent-cli`, which builds from source
because that is what cargo does. It is also the fallback the other five point at
when a platform has no prebuilt archive.

## Cutting a release

```powershell
# 1. Build the archive for this platform.
pwsh packaging/release.ps1

# 2. Do the same on a Mac (both architectures) and on Linux, and collect the
#    archives into one dist/ - every step below reads SHA256SUMS.
#    ./packaging/release.sh --target aarch64-apple-darwin
#    ./packaging/release.sh --target x86_64-apple-darwin
#    ./packaging/release.sh --target x86_64-unknown-linux-gnu

# 3. Tag and publish the GitHub release, with every archive and SHA256SUMS.
git tag v0.1.0 ; git push --tags
gh release create v0.1.0 dist/*.zip dist/*.tar.gz dist/SHA256SUMS

# 4. The registries.
pwsh packaging/cargo-publish.ps1                 # dry run first
pwsh packaging/cargo-publish.ps1 -Execute
node packages/npm/build.mjs --publish
python packages/python/build.py --publish        # once per platform
./packaging/homebrew/update.sh --tap ../homebrew-inillucent
```

Step 3 comes before step 4 and that ordering is not arbitrary: the npm platform
packages, the Homebrew formula, the Go installer and the PHP installer all fetch
from the GitHub release, so publishing them first publishes a package that
cannot install.

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

- **Cross-compiling.** Each platform's archive is built on that platform. Cross
  builds are possible and are a second thing that can be subtly wrong; a release
  is cut rarely enough that running the script on three machines is cheaper than
  maintaining a cross-compilation setup nobody exercises between releases.
- **Signing.** Both the Windows and macOS stories need a certificate that costs
  money and belongs to a person. `windows/README.md` and `macos/README.md` each
  record exactly what theirs takes, so the decision can be made with the numbers
  in front of whoever makes it.
- **A Linux `manylinux` wheel.** PyPI refuses a plain `linux_x86_64` wheel; the
  Linux one has to be built in a `manylinux` container. `packages/python/build.py`
  says so rather than uploading something that will be rejected after the fact.
