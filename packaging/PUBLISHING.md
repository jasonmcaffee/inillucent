# Publishing: what is ready, and what each step still needs

Everything in this repository is built, tested and verified up to the upload.
This file is the last mile: what each registry needs, in what order, and which
credential is missing.

**Nothing here has been published.** task-1836 built and verified all six
routes; every one of them stops at a credential that belongs to a person, or at
one decision that is not a script's to make.

---

## The decision that comes first

**Three of the six routes require the repository to be public**, and it is
private today:

| route | needs a public repository? | why |
|---|---|---|
| npm | no | it ships binaries; the source never leaves this machine |
| PyPI | no | it ships binaries plus the Python binding, in the wheel |
| **crates.io** | **yes, effectively** | `cargo publish` uploads the crate **source**, and it can never be removed |
| **Homebrew** | **yes** | the formula fetches a release asset, and a private repository's assets are not public |
| **Go** | **yes** | `go install` clones the module from the repository |
| the install scripts | **yes** | they `curl` from `raw.githubusercontent.com` and the releases |

So the first question is not "which registry" but **"does inillucent become
public source?"** It is a one-way door: crates.io versions cannot be deleted
(`cargo yank` hides a version from new resolution and leaves it downloadable
forever), and a public git history cannot be un-published.

If the answer is no for now, **npm and PyPI still work**, and they are the two
that reach the most people fastest.

---

## Step 0 — the release, which everything else reads

```powershell
pwsh packaging/release.ps1                      # this machine: x86_64-pc-windows-msvc
```

and on the MacBook and a Linux box:

```sh
./packaging/release.sh --target aarch64-apple-darwin
./packaging/release.sh --target x86_64-apple-darwin
./packaging/release.sh --target x86_64-unknown-linux-gnu
```

Collect every archive and the merged `SHA256SUMS` into one `dist/`, then:

```sh
git tag v0.1.0 && git push --tags
gh release create v0.1.0 dist/*.zip dist/*.tar.gz dist/SHA256SUMS \
  --title "inillucent 0.1.0" --notes-file packaging/release-notes.md
```

**Done here**: `dist/inillucent-0.1.0-x86_64-pc-windows-msvc.zip`, 13.5 MB,
`SHA256SUMS` written, `install.ps1 -FromDist` tested against it end to end.
**Still needed**: the three non-Windows archives, which have to be built on
those platforms, and a GitHub release to hang them on.

---

## crates.io

```powershell
cargo login                                # opens a browser; GitHub OAuth
pwsh packaging/cargo-publish.ps1           # dry run
pwsh packaging/cargo-publish.ps1 -Execute  # asks you to type PUBLISH
```

**Ready.** `cargo publish --workspace --dry-run` completes for all 30
publishable crates, in dependency order, with no errors — that was verified on
2026-09-08. Two crates are excluded by their own manifests: `inillucent-compat`
(the assurance harness) and `inillucent-bench` (which links PostgreSQL and
pgvector to measure against them).

Getting there needed two fixes that are now in:

- every internal edge in `[workspace.dependencies]` carries a `version` beside
  its `path`, because a published crate cannot resolve a path;
- `inillucent-base`'s build script read `../../compat/errors.toml`, which is
  **above the crate directory and therefore not in the tarball**, so the crate
  could not be published and nothing above it could either. The manifests are
  now vendored into `crates/inillucent-base/manifests/` as well, the workspace
  copy still wins, and the build **fails if the two have drifted** — a vendored
  copy nothing checks is a copy that goes stale, and this one generates the
  engine's error table.

**Missing**: a crates.io API token. `cargo login` needs a browser and a GitHub
account; there is no way to mint one from a terminal.

**Consequence**: this publishes the source of 30 crates under MIT, permanently.

---

## npm

```sh
npm login                                  # browser, plus a one-time code
node packages/npm/build.mjs --publish
```

**Ready.** `npm publish --dry-run` is clean, and the packages were built,
packed, installed from their tarballs into a scratch project and driven end to
end — the shim resolved the platform binary, ran it, passed the exit code back,
and the Node API returned real rows.

The shape is esbuild's: `inillucent` is a shim with four
`@inillucent/cli-<platform>` packages as `optionalDependencies`, so npm installs
only the one that runs on the machine. No postinstall, no download at install
time, so `npm ci` works offline and behind a proxy.

**Missing**: a valid npm token. `~/.npmrc` has one and it is **expired** —
`npm whoami` answers `401 Unauthorized`. The account exists; the token needs
renewing with `npm login`, which needs a browser and the one-time code npm mails
or prompts for.

**Note**: only `@inillucent/cli-win32-x64` can be built here. The other three
platform packages need their archives from step 0. Publishing the wrapper
without them is fine — npm treats a missing `optionalDependency` as a platform
the package does not serve — but the wrapper should go **last**, because it pins
them at an exact version.

---

## PyPI

```sh
python -m pip install build twine
python packages/python/build.py --publish            # or --publish --test for TestPyPI
```

**Ready.** The wheel builds, and it was installed into a clean virtual
environment and exercised both ways: the console scripts run, and
`inillucent.Database(...)` opened a file through the bundled C ABI library and
returned rows. It carries the four binaries, the shared library, the header and
the reference driver binding, so `pip install inillucent` gets a command *and*
an in-process driver with no compiler.

**Missing**: a PyPI account and an API token. PyPI has required two-factor
authentication for uploads since 2024, so this needs an authenticator app that
belongs to a person.

**Note**: PyPI refuses a plain `linux_x86_64` wheel; the Linux one has to be
built in a `manylinux` container. `build.py` says so rather than uploading
something that will be rejected after the fact.

---

## Homebrew

```sh
# once: create the repository jasonmcaffee/homebrew-inillucent on GitHub
./packaging/homebrew/update.sh --tap ../homebrew-inillucent
# then, in the tap:
git add Formula/inillucent.rb && git commit -m "inillucent 0.1.0" && git push
```

`brew install jasonmcaffee/inillucent/inillucent` after that.

**Ready**, except that `update.sh` fills the formula's checksums from
`dist/SHA256SUMS` and **refuses to finish while an archive is missing** — so it
needs step 0's macOS and Linux archives first. It says which are missing rather
than leaving a placeholder in a file somebody would push.

**Missing**: the tap repository, the two macOS archives and the Linux one, and a
public `jasonmcaffee/inillucent` for the formula's URLs to resolve.

**Not verified here**: Homebrew does not run on Windows, so the formula has been
written and reviewed but not executed. `brew audit --strict --new inillucent`
and `brew test inillucent` are the two commands to run on the Mac before pushing
the tap; the formula's `test do` block is an end-to-end one that creates a
database, writes to it, reads it back and asks the MCP server for its tool list.

---

## Go

```sh
git tag packages/go/v0.1.0 && git push --tags
```

That is the whole publish: Go has no registry, and `go install
github.com/jasonmcaffee/inillucent/packages/go/cmd/inillucent@latest` reads the
repository directly. The tag carries the `packages/go/` prefix because the
module is in a subdirectory, which is Go's own rule for a nested module.

**Ready and verified.** `go vet` is clean and all five tests pass against the
installed binary, including one that proves a bound parameter is not
interpolated and one that proves a read-only handle refuses a write while still
allowing a read.

**Missing**: a public repository. `go install` clones it, and `GOPRIVATE` is not
something a stranger can be asked to set.

---

## Packagist (Composer)

Submit `https://github.com/jasonmcaffee/inillucent` at
<https://packagist.org/packages/submit>, then add the GitHub webhook Packagist
offers so a tag updates the package.

`composer.json` is at the **repository root**, because Packagist reads a
repository rather than a subdirectory; it autoloads `Inillucent\` from
`packages/php/src/` and declares the two `bin` scripts.

**Ready and verified.** `composer validate` passes, every file lints under PHP
8.3, and the package was exercised end to end against a real database: a batch,
a bound query, a hostile value round-tripping through binding unchanged,
`describe`, a `not_found` status raised as a typed exception, and a read-only
handle refusing a write while still answering a read.

**Missing**: a Packagist account (GitHub OAuth), and a public repository.

---

## The order, and why it is the order

1. **The GitHub release**, because the npm platform packages, the Homebrew
   formula, the Go installer and the PHP installer all fetch from it. Publishing
   any of them first publishes a package that cannot install.
2. **crates.io**, which is self-contained and is also the fallback every other
   route names when a platform has no prebuilt archive.
3. **npm and PyPI**, in either order.
4. **Homebrew and Packagist**, which are one commit and one form.

---

## The one thing that cannot be automated

Every registry above requires an interactive login: a browser, an OAuth
redirect, and in PyPI's case a mandatory second factor. That is deliberate on
their part — it is what stops somebody else publishing under your name — and it
means the last step of each route belongs to a person at a keyboard, not to a
script. Everything up to it is a command in this directory.
