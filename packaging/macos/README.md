# macOS packaging - what runs on the MacBook

The macOS half of a release is built, signed and notarised on the MacBook and
nowhere else. `lipo`, `codesign`, `pkgbuild`, `productbuild`, `notarytool` and
`stapler` are macOS programs, the Developer ID certificate lives in the login
keychain, and only a Mac can run a Mach-O to check that any of it worked.

**One command does all of it:**

```sh
./packaging/macos/release-macos.sh --version 0.1.1 --upload
```

It builds both architectures, `lipo`s them into universal binaries, signs them
with the hardened runtime and a trusted timestamp, writes the `.tar.gz` that
install.sh and Homebrew fetch and the `.zip` that Apple's notary accepts, builds
the `.pkg` from the binaries it just signed, notarises both containers, staples
the ticket to the `.pkg`, runs the result, and uploads the four files to the
GitHub release that carries them to the Windows machine.

Then, on the Windows machine:

```powershell
pwsh packaging/fetch-macos-artifacts.ps1 -Version 0.1.1
```

which verifies the checksums and reads the signature back out of the binaries
before letting them near the site. `packaging/README.md` has the whole sequence,
both machines.

**`verify-macos.sh` is the release gate**, and it is the reason the release ends
on a Mac rather than at the upload:

```sh
curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version 0.1.1
```

It quarantines the published archive the way a browser would, asks `spctl`
whether Gatekeeper accepts it, verifies every signature, runs a database round
trip, asks the MCP server for its tool list, and runs the x86-64 slice under
Rosetta. Nothing on the site links to a macOS download until that passes.

**The install script needs none of this.** `packaging/install.sh` downloads the
tarball, checks its SHA-256 and puts the four programs in `~/.local/bin`, with no
Apple account, no `sudo` and no Gatekeeper prompt, because a file fetched with
curl carries no quarantine attribute.

```sh
curl -fsSL https://inillucent.com/downloads/install.sh | sh
```

The `.pkg` is the convenience for somebody who would rather double-click, and it
is the only artifact that can carry a stapled ticket, so it is also the only one
that installs on a machine with no network.

---

## The older scripts

`build-pkg.sh` and `notarize.sh` predate `release-macos.sh` and still work. They
build and notarise the `.pkg` alone, without the archives and without the smoke
test. Use `release-macos.sh` for a release; reach for these two only when the
`.pkg` is the only thing being rebuilt.

## The one-time setup

Three things, none of which is in this repository and none of which ever should
be. `release-macos.sh` refuses to run until they exist and says which is missing.

### 1. The Rust targets

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

### 2. The two certificates - $99/year, one afternoon

Enrol at <https://developer.apple.com/programs/> ($99/year, needs an Apple ID
with two-factor on). Then, in Xcode → Settings → Accounts → Manage
Certificates, create both of:

- **Developer ID Application** — signs the four executables and the dylib;
- **Developer ID Installer** — signs the `.pkg` itself.

Both are needed. A `.pkg` signed with the wrong one of the two fails
notarisation with a message that does not say which.

### 3. A notarytool credential, stored in the keychain

At <https://appleid.apple.com> → Sign-In and Security → App-Specific Passwords.
Then store it in the keychain once, so it is never on a command line or in this
repository:

```sh
xcrun notarytool store-credentials inillucent-notary \
  --apple-id you@example.com \
  --team-id ABCDE12345 \
  --password xxxx-xxxx-xxxx-xxxx
```

`--team-id` is on the membership page. `inillucent-notary` is the profile name
`notarize.sh` looks for.

### 4. gh, if the artifacts are to travel over a release

```sh
brew install gh && gh auth login
```

`release-macos.sh --upload` puts the four files on the `v<version>` release of
the private repository, and the Windows machine collects them with
`packaging/fetch-macos-artifacts.ps1`. Without `--upload` the script prints the
four paths and they can be copied across by any other means;
`fetch-macos-artifacts.ps1 -FromDirectory` takes them from a folder.

---

## Then, on the Mac

```sh
./packaging/macos/release-macos.sh --version 0.1.1 --upload
```

`inillucent-notary` is the profile name it looks for, and the Developer ID
identity is read out of the keychain rather than typed, because the full string
includes the team id and getting it wrong fails late.

---

## What to check before publishing a macOS release

Nothing by hand: `packaging/macos/verify-macos.sh` is that list, and it runs
against the published bytes rather than against the build directory, so what it
checks is what a reader gets.

```sh
curl -fsSL https://inillucent.com/downloads/verify-macos.sh | sh -s -- --version 0.1.1
```

It asserts, in order: the archive matches the published `SHA256SUMS`; `spctl`
reports `source=Notarized Developer ID` on a quarantined copy; every signature
verifies `--strict`; a database round trip returns its row; the MCP server lists
its tools; the x86-64 slice runs under Rosetta; and the C ABI library loads.

`release-macos.sh` runs the same script against the local archive before it
uploads anything, so a bad build is caught on the Mac that made it.

## A note on where it installs

`/usr/local`, which is the conventional place for a package that is not from
Apple and is what Homebrew uses on Intel. It needs the administrator password
once, at install time, which a `.pkg` asks for anyway. `install.sh` does not
touch `/usr/local` at all — it stays inside `$HOME` — and the two can coexist,
though `PATH` order then decides which `inillucent` a shell finds. `which -a
inillucent` is how somebody finds that out.
