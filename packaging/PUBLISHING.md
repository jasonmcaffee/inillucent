# Publishing: each destination, what it needs, and how to repair it

`pwsh packaging/ship.ps1` publishes a release to every destination in one run.
[`README.md`](README.md) describes that run: its phases, the order of its routes, the credentials
it reads and the files that carry the version. This page describes each destination on its own:
what it publishes, what it needs, how `ship.ps1` checks it, and how to run or repair that one step
by hand.

Words used here and not explained, such as route, mirror, tap, notarise and DPAPI, are in the terms
table at the top of [`README.md`](README.md#terms-used-on-this-page).

## Where each destination stands

Checked on 2026-09-24 with requests that carried no credential. Every destination serves 1.0.29.

| Destination | Name a user installs | Serves 1.0.29 |
|---|---|---|
| inillucent.com | `irm https://inillucent.com/downloads/install.ps1 \| iex`, or `curl -fsSL https://inillucent.com/downloads/install.sh \| sh` | yes, nine downloads, `SHA256SUMS`, `SHA256SUMS.minisig` and `provenance.json` |
| GitHub release | `Black-Rainbow-Labs/Inillucent`, release `v1.0.29` | yes |
| crates.io | `inillucent-cli` and the other publishable crates | yes |
| npm | `inillucent`, with five `@blackrainbowlabs/cli-*` platform packages | yes |
| PyPI | `inillucent` | yes |
| Go | `github.com/Black-Rainbow-Labs/Inillucent/packages/go` | yes, through `proxy.golang.org` |
| Packagist | `black-rainbow-labs/inillucent` | yes |
| Homebrew | `brew install black-rainbow-labs/inillucent/inillucent` | yes |

Both GitHub repositories are public: the development repository (the `origin` remote) and the
mirror `Black-Rainbow-Labs/Inillucent` (the `brl` remote). Every published package links to the
mirror.

## Repairing a release

A route that failed can run again on its own:

```powershell
pwsh packaging/ship.ps1 -Only npm,pypi
```

`-Only` selects routes. The tests and the version phase still run, and the version phase writes the
current version again, which changes nothing.

`ship.ps1` refuses a `-Version` lower than the version in `Cargo.toml`. The version phase would
rewrite every version file backwards, and the `tag` route would commit that. To add a file to a
release that is already out, run the step itself: `packaging/publish-site.ps1` for the site, or
`gh release upload` for GitHub.

A published version cannot be replaced on npm, crates.io or PyPI. `proxy.golang.org` and Packagist
keep the commit they first saw for a version. So a broken release is fixed by a new version:

| Version | Where | What happened |
|---|---|---|
| 0.1.0 | inillucent.com, GitHub | Withdrawn. Its archives told readers to run a Go command that did not exist. The downloads answer 404. The GitHub release keeps its assets and is titled as withdrawn. |
| 0.1.3, 0.1.4 | npm | Marked deprecated. They shipped broken and npm cannot replace a version. |
| v0.1.5 | Go module | Serves 0.1.3's source. The Go tag was pushed before the mirror held the release. |
| 0.1.6 | Packagist | Serves 0.1.3's source. Packagist read the tag before the mirror held the release. |

## Build: every target, signed

`ship.ps1` routes: `build`, `linux-packages`, `signature`.

```powershell
pwsh tools/cross/fetch-toolchain.ps1         # once: zig, cargo-zigbuild, rcodesign, nfpm, minisign
pwsh packaging/release-all.ps1 -Targets all
pwsh packaging/linux/package-linux.ps1 -Version 1.0.30
pwsh packaging/sign-sums.ps1
```

- `release-all.ps1` builds Windows natively and cross compiles Linux and macOS with zig and
  `cargo-zigbuild`. The five Rust targets are listed in `rust-toolchain.toml`, so `rustup` installs
  them. The macOS half is `macos/release-macos.ps1`, which signs, builds the `.pkg` and notarises.
  [`macos/README.md`](macos/README.md) has the Apple credentials it needs.
- `tools/cross/bin` is not in git. A worktree uses the main checkout's copy.
  `INILLUCENT_CROSS_BIN` points every script at a different folder.
- `linux/package-linux.ps1` builds a `.deb` and an `.rpm` for x86-64 and aarch64 with `nfpm`, and
  signs them with the OpenPGP key named by `INILLUCENT_GPG_KEY`. `apt` and `dnf` refuse an unsigned
  package from outside a distribution's own repository. `-Unsigned` builds without signing.
- `sign-sums.ps1` signs `dist/SHA256SUMS` with minisign and writes `SHA256SUMS.minisig`. It checks
  the new signature against `packaging/inillucent.pub` and refuses when the two keys do not match.
  `-AllowUnverifiedKey` overrides that check.

`dist/` and `packages/npm/staged/` are build output. Both are in `.gitignore`, and each release run
writes them again.

## Tag

`ship.ps1` route: `tag`.

The `tag` route commits the files the version phase changed with the message `inillucent <version>`,
creates the annotated tag `v<version>`, and pushes the commit to the default branch of `origin` and
then the tag. A release cut from a worktree has no upstream branch, so the route pushes `HEAD` to
the default branch by name.

## Mirror

`ship.ps1` route: `mirror`.

```powershell
pwsh packaging/mirror-github.ps1 -Verify                  # check every release tag on the mirror
pwsh packaging/mirror-github.ps1 -Version 1.0.30          # build the commit, push nothing
pwsh packaging/mirror-github.ps1 -Version 1.0.30 -Push    # push the commit and the tag
```

The mirror holds one commit per release and no development history. `mirror-github.ps1` builds that
commit with `git commit-tree` from the tree of the tag `v<version>`, and refuses when the commit's
tree differs from the tag's tree. The same inputs always give the same commit hash. The tag it
pushes is annotated.

`-Verify` reads the mirror and prints `same tree` for each release tag whose tree matches the tag of
the same name in the development repository. Do not combine `-Verify` with `-Push`: `-Verify` checks
and exits, so nothing is pushed.

## GitHub release

`ship.ps1` route: `github`.

The release is created on the mirror, named by the `brl` remote. `gh` with no `--repo` would use
`origin`. Every file in `dist/` whose name contains the version is attached, with `SHA256SUMS` and
its signature. The macOS zip that `rcodesign` sends to Apple's notary is left out.

`gh` needs a token. `ship.ps1` uses `GH_TOKEN` when it is set, then a `gh auth login`, then the
credential `git push` already uses, read through `git credential fill`. Preflight prints the account
it will publish as.

A release uploaded into a draft stays invisible. `ship.ps1` publishes a draft after the upload. The
release notes say how the release was tested: "Verified by the nightly run of `<date>` at `<commit>`",
with the prerequisites that run declared absent. When `-SkipTests` was given, they say the release
was published without running the test suite.

The nightly also publishes a rolling pre release called `nightly` to the same repository. Its tag is
`nightly`, which no registry reads as a version, and nothing links to it.

`ship.ps1` checks that the release exists and has assets.

## Interop fixture

`ship.ps1` route: `interop`.

`tools/build-interop-fixture.ps1` downloads this release's Windows archive from the GitHub release,
checks it against `SHA256SUMS` and the minisign signature, runs `tests/interop/build.sql` with it, and
writes `tests/interop/<version>/`. The route commits that folder and pushes it.
`crates/inillucent-compat/tests/e2e/release_format.rs` opens every folder under `tests/interop/` with the
current build, which checks that the engine still reads files written by each earlier release.

`ship.ps1` checks that `app.rdb`, `expected.tsv` and at least one log segment exist.

## inillucent.com

`ship.ps1` route: `site`.

```powershell
pwsh packaging/publish-site.ps1 -Version 1.0.30 -Stage   # copy the files, link nothing
pwsh packaging/publish-site.ps1 -Version 1.0.30 -Link    # add the download links
pwsh packaging/deploy-site.ps1                           # build the site and restart it
```

inillucent.com is where every installer downloads from. `install.ps1`, `install.sh`, the Homebrew
formula, the Go `cmd/inillucent-install` and the PHP `bin/inillucent-install` all read
`https://inillucent.com/downloads/` and check the SHA-256 against the published `SHA256SUMS`.

- `-Stage` copies the archives, `SHA256SUMS`, its signature, the public key and `VERSION` into the
  site checkout's `public/downloads/`. The files can then be downloaded, and no page links to them.
- `-Link` rewrites the download section of the site so the links appear.
- `deploy-site.ps1` builds the site's static export and restarts the service that serves it.
  `public/downloads/` is not in git, so the files reach the live site only through that build.

Staging before linking lets `verify-macos.sh` run against the published bytes before any reader can
find them.

Before the route runs, preflight checks that `install.sh` and `macos/verify-macos.sh` parse with
`bash -n` and contain no carriage return. `install.sh` runs under `sh`, which is dash on Debian and
Ubuntu, and dash fails on a carriage return. `.gitattributes` keeps every `*.sh` file at LF.

After the route runs, `ship.ps1` checks four things:

1. `downloads/VERSION` names this release.
2. The home page links a file for each of the nine platforms.
3. Every name in the published `SHA256SUMS` is served, at the size of the file in `dist/`.
4. The smallest file, downloaded in full, has the SHA-256 that `SHA256SUMS` gives.

## crates.io

`ship.ps1` route: `crates`.

```powershell
pwsh packaging/cargo-publish.ps1            # dry run
pwsh packaging/cargo-publish.ps1 -Execute   # asks you to type PUBLISH, then publishes
```

`cargo-publish.ps1` runs `cargo publish --workspace --locked`, which publishes the crates in
dependency order. Four crates set `publish = false` and are never published: `inillucent-bench`,
`inillucent-compat`, `inillucent-model` and `inillucent-sim`.

- The route needs `CARGO_REGISTRY_TOKEN`. `-Token` passes one on the command line, or `cargo login`
  stores one.
- Each workspace crate requires the others at the release's own version. The version phase writes
  that version into `[workspace.dependencies]` in `Cargo.toml`.
- `inillucent-base` reads two manifests from `compat/`, which is outside the crate folder and so not
  in the published crate. A copy lives in `crates/inillucent-base/manifests/`, and the build uses the
  workspace copy when it exists.
- A published crate version cannot be deleted. `cargo yank` stops new projects from resolving it,
  and it stays downloadable.

`ship.ps1` checks that `https://crates.io/api/v1/crates/inillucent-cli` names the version.

## npm

`ship.ps1` route: `npm`.

```sh
node packages/npm/build.mjs --publish --dry-run   # say what would be published
node packages/npm/build.mjs --publish             # publish
node packages/npm/build.mjs --publish --otp 123456
```

`inillucent` is a wrapper. It lists five platform packages as `optionalDependencies` at an exact
version: `@blackrainbowlabs/cli-win32-x64`, `cli-darwin-arm64`, `cli-darwin-x64`, `cli-linux-x64` and
`cli-linux-arm64`. npm installs only the one for the machine. Nothing is downloaded after install, so
`npm ci` works offline. `build.mjs` builds each platform package from the archives in `dist/`,
publishes the platform packages first and the wrapper last.

- Preflight runs `npm whoami` and prints the account. An npm token can sign in and still be refused
  at publish time when the account requires a second factor. A granular access token created with
  "bypass two factor authentication" publishes without one. With any other token, pass `-Otp` to
  `ship.ps1`.
- `ship.ps1` writes the sealed token into a temporary `.npmrc` on the RAM disk and points
  `npm_config_userconfig` at it.

`ship.ps1` checks that `https://registry.npmjs.org/inillucent` names the version.

## PyPI

`ship.ps1` route: `pypi`.

```sh
python -m pip install build twine
python packages/python/build.py --all --publish   # add --test for TestPyPI
```

`--all` builds four wheels: `win_amd64`, `macosx_13_0_universal2`, `manylinux_2_28_x86_64` and
`manylinux_2_28_aarch64`. Without `--all`, `build.py` builds one wheel, for the machine it runs on.
Each wheel holds the four programs, the C ABI library, the header and the Python binding, so
`pip install inillucent` gives both the commands and an in process driver with no compiler.

The route needs a PyPI API token. `ship.ps1` passes it to `twine` as `TWINE_USERNAME=__token__` and
`TWINE_PASSWORD`. PyPI issues an API token only to an account with two factor authentication.

`ship.ps1` checks that `https://pypi.org/pypi/inillucent/json` names the version.

## Go

`ship.ps1` route: `go`.

Go has no registry and no account. A tag is the release. The module is in a subfolder, so the tag is
`packages/go/v<version>`. The `go` route pushes that tag to the mirror, on the mirror's own
`v<version>` commit, and refuses when the mirror has no `v<version>` tag yet.

`go install` takes the version without the folder prefix:

```sh
go install github.com/Black-Rainbow-Labs/Inillucent/packages/go/cmd/inillucent-install@v1.0.29
inillucent-install
```

`@packages/go/v1.0.29` is rejected by Go with `disallowed version string`. `inillucent-install`
downloads the release named by `nativeVersion` in `main.go` from inillucent.com, checks it and
installs the four programs.

`proxy.golang.org` serves the module. It lowercases the module path and marks each capital with `!`,
so `ship.ps1` asks
`https://proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@latest`.

## Packagist

`ship.ps1` route: `packagist`.

`composer.json` is at the repository root, because Packagist reads a repository and not a subfolder.
It loads `Inillucent\` from `packages/php/src/` and declares the two `bin` scripts.

The `packagist` route calls Packagist's `update-package` API with the sealed token, so Packagist
reads the mirror's tags straight after the `mirror` and `go` routes. Packagist accounts sign in with
GitHub, so `PACKAGIST_USER` names a person's GitHub account. `ship.ps1` has a default for it.

`ship.ps1` checks that `https://repo.packagist.org/p2/black-rainbow-labs/inillucent.json` names the
version.

## Homebrew

`ship.ps1` route: `homebrew`.

```sh
./packaging/homebrew/update.sh --tap ../homebrew-inillucent
```

`update.sh` fills the formula's version, its three download URLs and their checksums from
`dist/SHA256SUMS`, and writes `Formula/inillucent.rb` into the tap. When an archive is missing it
leaves the placeholder, prints a warning and exits 1. The `homebrew` route then commits the formula
and pushes the tap. On Windows it runs `update.sh` with the `bash` that ships with Git for Windows,
because the `bash` on the `PATH` can be the WSL one, which cannot open a Windows path.

The formula downloads from inillucent.com. Its `head` spec clones the mirror, for
`brew install --HEAD`. Its `test do` block creates a database, reads a row back and asks
`inillucent-mcp` for its tool list. Homebrew does not run on Windows, so that block runs only on a
machine that installs the formula.

`ship.ps1` checks the formula on GitHub at
`https://raw.githubusercontent.com/Black-Rainbow-Labs/homebrew-inillucent/main/Formula/inillucent.rb`,
because that copy is the one `brew` reads.

## Checking a release after it is out

| Command | What it checks |
|---|---|
| `node tools/check-public-urls.mjs` | Every GitHub URL a shipped package names, and the Go module on `proxy.golang.org`, fetched with no credential. `tools/validate.ps1` and `tools/validate.sh` run it. |
| `bash packaging/verify-sites.sh` | Downloads every file inillucent.com offers and checks it against `SHA256SUMS`. |
| `bash packaging/verify-installs.sh <version>` | Installs from each published route and runs the result. It writes its scratch files to the folder in `WORK` at the top of the script. |
| `curl -fsSL https://inillucent.com/downloads/verify-macos.sh \| sh -s -- --version <version>` | On a Mac: the published macOS build's checksum, Gatekeeper, signatures, a database round trip, the MCP server, the x86-64 half under Rosetta, and the C ABI library. |

## Accounts

`ship.ps1` publishes with tokens and needs no browser. Creating an account and its token does need a
person at a browser:

| Destination | Account and token |
|---|---|
| npm | An account, and a granular access token from the account's Access Tokens page. npm's signup page refuses automated clients. |
| PyPI | An account with two factor authentication, and an API token. The registration form has a CAPTCHA. |
| crates.io | Sign in with GitHub, then create an API token. |
| Packagist | Sign in with GitHub, then copy the API token from the profile page. |
| GitHub | The account `git push` already uses. |
| Apple | An Apple Developer Program membership. [`macos/README.md`](macos/README.md) covers the certificates and the notary key. |

Each token is sealed into `%LOCALAPPDATA%\inillucent\signing\` with `Protect-AppleSecret` from
`packaging/macos/apple-credentials.ps1`. [`README.md`](README.md#credentials) lists the file name for
each one.
