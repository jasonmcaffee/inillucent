#!/usr/bin/env bash
#
# Builds a universal macOS installer package.
#
#   ./packaging/macos/build-pkg.sh --version 0.1.0
#
# Two `cargo build`s, one per architecture, `lipo`'d into one universal file per
# program, staged under a package root rooted at /usr/local, then `pkgbuild` and
# `productbuild`. The result is dist/inillucent-<version>.pkg and it installs
# and runs - it is just unsigned, so Gatekeeper wants a right-click → Open.
# packaging/macos/notarize.sh is what turns it into one that does not.
#
# Needs, once:
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# macOS only: pkgbuild, productbuild and lipo do not exist elsewhere. See
# packaging/macos/README.md for the four steps and what each one costs.

set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "this builds a macOS package and has to run on macOS." >&2
  echo "On another machine, packaging/release.sh builds the tarball install.sh uses." >&2
  exit 1
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version=""
identifier="com.jasonmcaffee.inillucent"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --identifier) identifier="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$version" ]; then
  version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
    | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
fi
[ -n "$version" ] || { echo "pass --version, or declare one in Cargo.toml" >&2; exit 1; }

echo "building inillucent $version for both architectures"
"$root/packaging/release.sh" --version "$version" --target aarch64-apple-darwin
"$root/packaging/release.sh" --version "$version" --target x86_64-apple-darwin

arm="$root/dist/inillucent-$version-aarch64-apple-darwin"
intel="$root/dist/inillucent-$version-x86_64-apple-darwin"
pkgroot="$root/dist/pkgroot"

rm -rf "$pkgroot"
mkdir -p "$pkgroot/usr/local/bin" "$pkgroot/usr/local/lib" "$pkgroot/usr/local/include"

# One universal file per program. `lipo -create` is what makes a single binary
# run natively on both an M-series machine and an Intel one, which matters more
# for a database than for most tools: Rosetta would work, and it would make
# every measurement on this engine a measurement of Rosetta.
for program in inillucent inillucent-shell inillucent-mcp inillucent-migrate; do
  lipo -create "$arm/bin/$program" "$intel/bin/$program" \
    -output "$pkgroot/usr/local/bin/$program"
  chmod 755 "$pkgroot/usr/local/bin/$program"
done
lipo -create "$arm/lib/libinillucent_driver_capi.dylib" "$intel/lib/libinillucent_driver_capi.dylib" \
  -output "$pkgroot/usr/local/lib/libinillucent_driver_capi.dylib"
cp "$arm/include/inillucent_driver.h" "$pkgroot/usr/local/include/"

echo
echo "architectures in the installed binary:"
lipo -archs "$pkgroot/usr/local/bin/inillucent"

component="$root/dist/inillucent-component-$version.pkg"
pkgbuild \
  --root "$pkgroot" \
  --identifier "$identifier" \
  --version "$version" \
  --install-location / \
  "$component"

# The distribution file names the component package, which carries the version
# in its file name - so the version is substituted rather than the template
# being edited every release. A template with a hard-coded version is a template
# that silently builds the previous release's payload.
resources="$root/dist/pkgresources"
rm -rf "$resources"
mkdir -p "$resources"
cp "$root/packaging/macos/welcome.txt" "$root/packaging/macos/conclusion.txt" "$resources/"
cp "$root/LICENSE" "$resources/LICENSE"

distribution="$root/dist/Distribution.xml"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Distribution.xml" > "$distribution"

product="$root/dist/inillucent-$version.pkg"
productbuild \
  --distribution "$distribution" \
  --package-path "$root/dist" \
  --resources "$resources" \
  "$product"

# The component package is an intermediate: productbuild has already folded it
# into the distribution package, and leaving it beside the real one in dist/ is
# how somebody uploads the wrong file to a release.
rm -f "$component"
rm -rf "$resources" "$distribution"

echo
echo "built $product"
echo
echo "It is UNSIGNED. To install it here for a test:"
echo "  sudo installer -pkg \"$product\" -target /"
echo "To make one that opens by double-click on somebody else's machine, see"
echo "packaging/macos/README.md and then run packaging/macos/notarize.sh."
