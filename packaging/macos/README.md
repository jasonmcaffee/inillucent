# macOS packaging — what is here, and what to finish on the MacBook

Everything in this directory runs. None of it can run on Windows, which is why
it was written here and is finished there: `pkgbuild`, `productbuild`, `lipo`,
`codesign` and `notarytool` are macOS-only programs, and notarisation needs an
Apple Developer account that belongs to a person rather than to a repository.

**There is already a working macOS install and it needs none of this.**
`packaging/install.sh` downloads the release tarball, verifies its SHA-256 and
puts the four programs in `~/.local/bin`. It needs no Apple account, no
certificate, no notarisation and no `sudo`:

```sh
curl -fsSL https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.sh | sh
```

The `.pkg` below is the convenience for somebody who would rather double-click,
and `brew install jasonmcaffee/tap/inillucent` is the third road. Do not treat
the `.pkg` as blocking anything.

---

## The four steps, in order

### 1. Build a universal binary — no account needed

```sh
./packaging/macos/build-pkg.sh --version 0.1.0
```

It runs `packaging/release.sh` twice, once for `aarch64-apple-darwin` and once
for `x86_64-apple-darwin`, `lipo`s each program and the C ABI dylib into one
universal file, and stages `dist/pkgroot/usr/local/{bin,lib,include}`. Then it
calls `pkgbuild` and `productbuild` and leaves
`dist/inillucent-<version>.pkg`.

It needs `rustup target add aarch64-apple-darwin x86_64-apple-darwin` first.
The `.pkg` it produces installs and works. It is *unsigned*, so Gatekeeper
refuses to open it by double-click and a person has to right-click → Open, or
run `xattr -d com.apple.quarantine`. That is the whole difference steps 2 to 4
buy.

### 2. Get the two certificates — $99/year, one afternoon

Enrol at <https://developer.apple.com/programs/> ($99/year, needs an Apple ID
with two-factor on). Then, in Xcode → Settings → Accounts → Manage
Certificates, create both of:

- **Developer ID Application** — signs the four executables and the dylib;
- **Developer ID Installer** — signs the `.pkg` itself.

Both are needed. A `.pkg` signed with the wrong one of the two fails
notarisation with a message that does not say which.

### 3. Make an app-specific password for `notarytool`

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

### 4. Sign, notarise and staple

```sh
./packaging/macos/notarize.sh --version 0.1.0 --identity "Developer ID Application: Your Name (ABCDE12345)"
```

It signs every binary with the hardened runtime, rebuilds and signs the `.pkg`
with the installer identity, submits it with `notarytool submit --wait`, and
staples the ticket so the result installs on a machine that is offline. The
whole round trip is usually two to fifteen minutes; `--wait` blocks for it.

Then upload `dist/inillucent-<version>.pkg` to the GitHub release beside the
tarballs.

---

## What to check before publishing a macOS release

```sh
# The binary is genuinely both architectures.
lipo -archs dist/pkgroot/usr/local/bin/inillucent
# arm64 x86_64

# The signature is valid and the runtime is hardened.
codesign -dv --verbose=4 dist/pkgroot/usr/local/bin/inillucent 2>&1 | grep -E 'Authority|flags'

# Gatekeeper agrees, which is the thing a user's machine will ask.
spctl -a -vvv -t install dist/inillucent-0.1.0.pkg
# should say: accepted, source=Notarized Developer ID

# And it actually runs.
/usr/local/bin/inillucent --version
/usr/local/bin/inillucent create /tmp/probe.rdb
/usr/local/bin/inillucent --db /tmp/probe.rdb exec "CREATE TABLE t (a)"
```

## A note on where it installs

`/usr/local`, which is the conventional place for a package that is not from
Apple and is what Homebrew uses on Intel. It needs the administrator password
once, at install time, which a `.pkg` asks for anyway. `install.sh` does not
touch `/usr/local` at all — it stays inside `$HOME` — and the two can coexist,
though `PATH` order then decides which `inillucent` a shell finds. `which -a
inillucent` is how somebody finds that out.
