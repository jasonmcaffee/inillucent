#!/usr/bin/env bash
#
# Signs, notarises and staples the macOS package.
#
#   ./packaging/macos/notarize.sh --version 0.1.0 \
#       --identity "Developer ID Application: Your Name (ABCDE12345)"
#
# It needs the two things packaging/macos/README.md steps 2 and 3 set up:
#
#   * a Developer ID Application certificate, and a Developer ID Installer one,
#     both in the login keychain;
#   * a notarytool credential profile called `inillucent-notary`.
#
# Neither is in this repository and neither ever should be. The profile name is
# the only thing this script knows, and it is not a secret.
#
# Nothing here is a substitute for build-pkg.sh - it runs that first, because
# signing has to happen on the binaries *before* they are packaged, and a
# package assembled from unsigned binaries and then signed passes `spctl` on the
# package and fails on first run.

set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "codesign, notarytool and stapler are macOS-only. Run this on the MacBook." >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version=""
identity=""
installer_identity=""
profile="inillucent-notary"
identifier="com.jasonmcaffee.inillucent"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --identity) identity="$2"; shift 2 ;;
    --installer-identity) installer_identity="$2"; shift 2 ;;
    --profile) profile="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$version" ]; then
  version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
    | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
fi
[ -n "$identity" ] || { echo "pass --identity \"Developer ID Application: ...\"" >&2; exit 2; }

# The installer certificate is the *other* one, and the two are easy to confuse:
# a .pkg signed with the Application identity fails notarisation with a message
# that does not say which certificate was wrong. Derived by default so the
# common case needs one argument rather than two.
if [ -z "$installer_identity" ]; then
  installer_identity="${identity/Developer ID Application/Developer ID Installer}"
fi

echo "building the payload first, so signing happens on the binaries"
"$root/packaging/macos/build-pkg.sh" --version "$version" --identifier "$identifier"

pkgroot="$root/dist/pkgroot"

echo
echo "signing with: $identity"
for file in \
  "$pkgroot/usr/local/bin/inillucent" \
  "$pkgroot/usr/local/bin/inillucent-shell" \
  "$pkgroot/usr/local/bin/inillucent-mcp" \
  "$pkgroot/usr/local/bin/inillucent-migrate" \
  "$pkgroot/usr/local/lib/libinillucent_driver_capi.dylib"
do
  # --options runtime is the hardened runtime, and notarisation refuses a
  # binary without it. --timestamp gets a trusted timestamp so the signature
  # outlives the certificate.
  codesign --force --sign "$identity" --options runtime --timestamp "$file"
  codesign --verify --verbose=2 "$file"
done

component="$root/dist/inillucent-component-$version.pkg"
pkgbuild \
  --root "$pkgroot" \
  --identifier "$identifier" \
  --version "$version" \
  --install-location / \
  "$component"

resources="$root/dist/pkgresources"
rm -rf "$resources"
mkdir -p "$resources"
cp "$root/packaging/macos/welcome.txt" "$root/packaging/macos/conclusion.txt" "$resources/"
cp "$root/LICENSE" "$resources/LICENSE"
distribution="$root/dist/Distribution.xml"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Distribution.xml" > "$distribution"

unsigned="$root/dist/inillucent-$version-unsigned.pkg"
product="$root/dist/inillucent-$version.pkg"
productbuild \
  --distribution "$distribution" \
  --package-path "$root/dist" \
  --resources "$resources" \
  "$unsigned"

echo
echo "signing the package with: $installer_identity"
productsign --sign "$installer_identity" "$unsigned" "$product"
rm -f "$unsigned" "$component"
rm -rf "$resources" "$distribution"

echo
echo "submitting to Apple; this usually takes two to fifteen minutes"
xcrun notarytool submit "$product" --keychain-profile "$profile" --wait

echo
echo "stapling the ticket, so it installs on a machine that is offline"
xcrun stapler staple "$product"

echo
echo "checking what a user's machine will check"
spctl -a -vvv -t install "$product"

echo
echo "notarised: $product"
echo "Upload it to the GitHub release beside the tarballs."
