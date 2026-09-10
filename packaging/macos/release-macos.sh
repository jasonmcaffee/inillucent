#!/usr/bin/env bash
#
# The whole macOS release, on the MacBook, in one command.
#
#   ./packaging/macos/release-macos.sh --version 0.1.0
#
# It builds both architectures, joins them into universal binaries, signs them
# with the Developer ID, produces the three artifacts the site and the package
# managers need, notarises them, runs them, and hands them to the Windows
# machine that publishes inillucent.com.
#
#     dist/inillucent-<version>-universal-apple-darwin.tar.gz   install.sh, Homebrew
#     dist/inillucent-<version>-universal-apple-darwin.zip      the notary submission
#     dist/inillucent-<version>.pkg                             the double-click download
#     dist/SHA256SUMS-macos                                     what the Windows side verifies
#
# Signing happens on the binaries before they are packaged, never after: a
# package assembled from unsigned binaries and then signed passes `spctl` on the
# package and fails on first run.
#
# What it needs, once. packaging/macos/README.md has the detail:
#
#   * a Developer ID Application certificate and a Developer ID Installer
#     certificate in the login keychain;
#   * a notarytool credential profile, `inillucent-notary` by default;
#   * rustup target add aarch64-apple-darwin x86_64-apple-darwin;
#   * gh, signed in, if --upload is used.
#
# Nothing secret is in this repository, and nothing secret is passed on a
# command line: the certificates live in the keychain and the notary credential
# lives in a keychain profile.

set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This is the macOS half of the release and only runs on macOS." >&2
  echo "On the Windows machine: pwsh packaging/release-all.ps1 builds Windows and Linux." >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version=""
identity=""
installer_identity=""
profile="inillucent-notary"
identifier="com.jasonmcaffee.inillucent"
upload=0
skip_notarize=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --identity) identity="$2"; shift 2 ;;
    --installer-identity) installer_identity="$2"; shift 2 ;;
    --profile) profile="$2"; shift 2 ;;
    --upload) upload=1; shift ;;
    --skip-notarize) skip_notarize=1; shift ;;
    -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$version" ]; then
  version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
    | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
fi
[ -n "$version" ] || { echo "pass --version, or declare one in Cargo.toml" >&2; exit 1; }

# The signing identity, found in the keychain rather than typed, because the
# full string includes the team id and getting it wrong fails late.
if [ -z "$identity" ]; then
  identity="$(security find-identity -v -p codesigning \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
fi
if [ -z "$identity" ]; then
  echo "no Developer ID Application certificate in the keychain." >&2
  echo "packaging/macos/README.md step 2 is how to get one. Or pass --identity." >&2
  exit 1
fi
# The installer certificate is the *other* one, and the two are easy to confuse:
# a .pkg signed with the Application identity fails notarisation with a message
# that does not say which certificate was wrong.
if [ -z "$installer_identity" ]; then
  installer_identity="${identity/Developer ID Application/Developer ID Installer}"
fi

dist="$root/dist"
name="inillucent-$version-universal-apple-darwin"
stage="$dist/$name"

echo "inillucent $version"
echo "  signing as: $identity"
echo

# ---------------------------------------------------------------------------
# 1. Both architectures, then one universal file per program.
# ---------------------------------------------------------------------------

echo "== building both architectures"
"$root/packaging/release.sh" --version "$version" --target aarch64-apple-darwin
"$root/packaging/release.sh" --version "$version" --target x86_64-apple-darwin

arm="$dist/inillucent-$version-aarch64-apple-darwin"
intel="$dist/inillucent-$version-x86_64-apple-darwin"

# The universal stage starts as a copy of the arm64 one, so it inherits the
# documentation, the header and the layout, and then each program is replaced by
# the two-architecture version of itself.
rm -rf "$stage"
cp -R "$arm" "$stage"

echo
echo "== lipo"
for program in inillucent inillucent-shell inillucent-mcp inillucent-migrate; do
  lipo -create -output "$stage/bin/$program" "$arm/bin/$program" "$intel/bin/$program"
  echo "  $program: $(lipo -archs "$stage/bin/$program")"
done
lipo -create -output "$stage/lib/libinillucent_driver_capi.dylib" \
  "$arm/lib/libinillucent_driver_capi.dylib" "$intel/lib/libinillucent_driver_capi.dylib"
echo "  libinillucent_driver_capi.dylib: $(lipo -archs "$stage/lib/libinillucent_driver_capi.dylib")"

# ---------------------------------------------------------------------------
# 2. Sign. --options runtime is the hardened runtime, which notarisation
#    requires; --timestamp gets a trusted timestamp so the signature outlives
#    the certificate that made it.
# ---------------------------------------------------------------------------

echo
echo "== signing"
signable="$stage/bin/inillucent $stage/bin/inillucent-shell $stage/bin/inillucent-mcp $stage/bin/inillucent-migrate $stage/lib/libinillucent_driver_capi.dylib"
for file in $signable; do
  codesign --force --sign "$identity" --options runtime --timestamp "$file"
  codesign --verify --strict --verbose=2 "$file"
done

# ---------------------------------------------------------------------------
# 3. The two archives. Both hold the same signed binaries. The .tar.gz is what
#    install.sh and Homebrew fetch; the .zip exists because Apple's notary
#    accepts .zip, .pkg and .dmg and does not accept .tar.gz.
# ---------------------------------------------------------------------------

echo
echo "== archives"
tarball="$dist/$name.tar.gz"
zip_archive="$dist/$name.zip"
rm -f "$tarball" "$zip_archive"
tar --directory "$dist" -czf "$tarball" "$name"
# ditto rather than zip, because it preserves the signatures and the symlinks
# the way Apple's own tooling expects.
ditto -c -k --keepParent "$stage" "$zip_archive"
echo "  $tarball"
echo "  $zip_archive"

# ---------------------------------------------------------------------------
# 4. The .pkg, assembled from the binaries that were just signed.
# ---------------------------------------------------------------------------

echo
echo "== package"
pkgroot="$dist/pkgroot"
rm -rf "$pkgroot"
mkdir -p "$pkgroot/usr/local/bin" "$pkgroot/usr/local/lib" "$pkgroot/usr/local/include"
cp "$stage/bin/"* "$pkgroot/usr/local/bin/"
cp "$stage/lib/"* "$pkgroot/usr/local/lib/"
cp "$stage/include/"* "$pkgroot/usr/local/include/"

component="$dist/inillucent-component-$version.pkg"
pkgbuild --root "$pkgroot" --identifier "$identifier" --version "$version" \
  --install-location / "$component"

resources="$dist/pkgresources"
rm -rf "$resources"
mkdir -p "$resources"
cp "$root/packaging/macos/welcome.txt" "$root/packaging/macos/conclusion.txt" "$resources/"
cp "$root/LICENSE" "$resources/LICENSE"
distribution="$dist/Distribution.xml"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Distribution.xml" > "$distribution"

unsigned="$dist/inillucent-$version-unsigned.pkg"
product="$dist/inillucent-$version.pkg"
rm -f "$unsigned" "$product"
productbuild --distribution "$distribution" --package-path "$dist" \
  --resources "$resources" "$unsigned"
productsign --sign "$installer_identity" "$unsigned" "$product"
rm -f "$unsigned" "$component" "$distribution"
rm -rf "$resources"
echo "  $product"

# ---------------------------------------------------------------------------
# 5. Notarisation. The .pkg gets a stapled ticket, so it installs on a machine
#    that is offline. The .zip cannot be stapled - no container format outside
#    .pkg, .dmg and .app can be - but submitting it registers the cdhash of
#    every binary inside it with Apple, which is what Gatekeeper looks up when
#    somebody runs a program out of the tarball.
# ---------------------------------------------------------------------------

if [ "$skip_notarize" -eq 0 ]; then
  echo
  echo "== notarising; this usually takes two to fifteen minutes per submission"
  xcrun notarytool submit "$product" --keychain-profile "$profile" --wait
  xcrun stapler staple "$product"
  xcrun notarytool submit "$zip_archive" --keychain-profile "$profile" --wait
  echo
  echo "  gatekeeper on the package:"
  spctl -a -vvv -t install "$product" 2>&1 | sed 's/^/    /'
else
  echo
  echo "== notarisation skipped (--skip-notarize). The artifacts are NOT distributable."
fi

# ---------------------------------------------------------------------------
# 6. Run what was just built, from the archive rather than from the build
#    directory, because the archive is what a reader gets.
# ---------------------------------------------------------------------------

echo
echo "== smoke test"
"$root/packaging/macos/verify-macos.sh" --archive "$tarball" --version "$version"

# ---------------------------------------------------------------------------
# 7. Checksums, and the handoff to the machine that publishes the site.
# ---------------------------------------------------------------------------

sums="$dist/SHA256SUMS-macos"
: > "$sums"
for file in "$tarball" "$zip_archive" "$product"; do
  ( cd "$dist" && shasum -a 256 "$(basename "$file")" >> "$sums" )
done
echo
echo "== $sums"
cat "$sums"

if [ "$upload" -eq 1 ]; then
  echo
  echo "== uploading to the v$version release"
  if ! gh release view "v$version" >/dev/null 2>&1; then
    gh release create "v$version" --draft --title "inillucent $version" \
      --notes "macOS artifacts built and signed on the MacBook."
  fi
  gh release upload "v$version" "$tarball" "$zip_archive" "$product" "$sums" --clobber
  echo
  echo "On the Windows machine:"
  echo "  pwsh packaging/fetch-macos-artifacts.ps1 -Version $version"
else
  echo
  echo "Not uploaded. Either re-run with --upload, or copy these four files to the"
  echo "Windows machine's dist/ directory yourself:"
  echo "  $tarball"
  echo "  $zip_archive"
  echo "  $product"
  echo "  $sums"
fi
