#!/usr/bin/env bash
#
# Builds the release archive every installer and every package is made from.
# The POSIX half of packaging/release.ps1: same layout, same names, same
# SHA256SUMS, so a macOS or Linux archive is interchangeable with a Windows one
# everywhere downstream.
#
#   ./packaging/release.sh                       # host target
#   ./packaging/release.sh --target aarch64-apple-darwin
#   ./packaging/release.sh --skip-build          # stage what is already built
#
# On macOS, packaging/macos/build-pkg.sh calls this twice - once per
# architecture - and lipo's the results into a universal binary. That is why
# --target is a parameter rather than being read from the host every time.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version=""
target=""
skip_build=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --target) target="$2"; shift 2 ;;
    --skip-build) skip_build=1; shift ;;
    -h|--help)
      sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

# The version is the workspace's, so an archive can never claim a number the
# binaries inside it do not.
if [ -z "$version" ]; then
  version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
    | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
fi
[ -n "$version" ] || { echo "Cargo.toml declares no [workspace.package] version" >&2; exit 1; }

if [ -z "$target" ]; then
  target="$(rustc -vV | sed -n 's/^host: //p')"
fi

echo "inillucent $version for $target"

built="$root/target/release"
if [ "$skip_build" -eq 0 ]; then
  echo "building (release, locked)..."
  cargo build --manifest-path "$root/Cargo.toml" --release --locked \
    --target "$target" \
    -p inillucent-cli -p inillucent-migrate -p inillucent-driver-capi
  # `--target` moves the output under target/<triple>/release, and omitting it
  # does not. Both are handled rather than one being assumed, because the macOS
  # packaging always passes a triple and a developer running this by hand never
  # does.
  built="$root/target/$target/release"
fi

name="inillucent-$version-$target"
dist="$root/dist"
stage="$dist/$name"
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/lib" "$stage/include"

for program in inillucent inillucent-shell inillucent-mcp inillucent-migrate; do
  [ -f "$built/$program" ] || { echo "the build did not produce $built/$program" >&2; exit 1; }
  cp "$built/$program" "$stage/bin/"
done

# The C ABI, which is how every language that is not Rust reaches the engine.
copied_library=0
for library in libinillucent_driver_capi.dylib libinillucent_driver_capi.so inillucent_driver_capi.dll; do
  if [ -f "$built/$library" ]; then
    cp "$built/$library" "$stage/lib/"
    copied_library=1
  fi
done
[ "$copied_library" -eq 1 ] || { echo "the build produced no C ABI shared library" >&2; exit 1; }

cp "$root/drivers/inillucent-driver-capi/include/inillucent_driver.h" "$stage/include/"
cp "$root/README.md" "$stage/"
cp "$root/drivers/README.md" "$stage/DRIVER.md"

# README.md links into docs/ for every subject it does not cover itself, so the
# archive carries that directory or the front page it ships is full of dead
# links. AGENTS.md and agent-skills/ travel with it for the same reason: the
# readme sends an AI agent to both.
cp -R "$root/docs" "$stage/docs"
cp "$root/AGENTS.md" "$stage/"
cp -R "$root/agent-skills" "$stage/agent-skills"

# Two documents live under tests/ rather than under docs/, because they describe
# assets that sit beside them. README.md and three of the docs/ pages link to
# both, so they travel as well. Only the prose: the fixtures and the schedules
# are not part of a binary archive.
mkdir -p "$stage/tests"
cp "$root/tests/synthetic-corpus.md" "$stage/tests/"
cp "$root/tests/inillucent-testing-tdd.md" "$stage/tests/"

printf '%s' "$version" > "$stage/VERSION"
if [ -f "$root/LICENSE" ]; then
  cp "$root/LICENSE" "$stage/"
else
  echo "warning: LICENSE is missing; the archive will not carry one" >&2
fi

archive="$dist/$name.tar.gz"
rm -f "$archive"
tar --directory "$dist" -czf "$archive" "$name"

# One SHA256SUMS over everything in dist, rewritten each time, so an installer
# verifies what it downloaded against one file.
sums="$dist/SHA256SUMS"
: > "$sums"
for file in "$dist"/*.tar.gz "$dist"/*.zip; do
  [ -f "$file" ] || continue
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$dist" && sha256sum "$(basename "$file")") >> "$sums"
  else
    # macOS ships shasum rather than sha256sum, and its output has the two
    # fields the other way round.
    (cd "$dist" && shasum -a 256 "$(basename "$file")") >> "$sums"
  fi
done

echo
echo "staged  $stage"
echo "archive $archive"
echo "sums    $sums"
cat "$sums"
