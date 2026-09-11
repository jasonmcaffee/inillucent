#!/usr/bin/env bash
#
# Fills the version and the three sha256 values into the Homebrew formula from
# a built dist/, and writes the result where the tap expects it.
#
#   ./packaging/homebrew/update.sh --tap ../homebrew-inillucent
#
# A formula's checksums are the one thing in packaging that must never be typed
# by hand: a wrong one fails at install time on somebody else's machine, with a
# message about a corrupted download, and the person who sees it has no way to
# know it was a transcription error. So they are read out of the same
# SHA256SUMS the installers verify against.
#
# A platform whose archive is not in dist/ keeps its placeholder and is reported,
# rather than being silently dropped: a formula that quietly stopped offering
# Linux would look like a formula that never offered it.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tap=""
version=""

while [ $# -gt 0 ]; do
  case "$1" in
    --tap) tap="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$version" ]; then
  version="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" \
    | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
fi
sums="$root/dist/SHA256SUMS"
[ -f "$sums" ] || { echo "$sums does not exist. Run packaging/release.sh first." >&2; exit 1; }

formula="$(cat "$root/packaging/homebrew/inillucent.rb")"

# The version to replace is the one the template declares, read out of the
# template. Substituting a version literal written into this script instead
# meant that the moment the template was bumped, --version stopped having any
# effect and the formula kept pointing at the previous release's archives.
template="$(printf '%s\n' "$formula" | sed -n 's/^  version "\(.*\)"/\1/p' | head -1)"
[ -n "$template" ] || { echo "the formula declares no version" >&2; exit 1; }
formula="${formula//$template/$version}"

missing=0
fill() {
  target="$1"
  placeholder="$2"
  archive="inillucent-$version-$target.tar.gz"
  digest="$(awk -v want="$archive" '$NF == want { print $1 }' "$sums" | head -1)"
  if [ -z "$digest" ]; then
    echo "warning: $archive is not in dist/; $placeholder is left as it is" >&2
    missing=1
    return
  fi
  formula="${formula//$placeholder/$digest}"
  echo "  $target  $digest"
}

echo "filling in checksums for $version:"
fill universal-apple-darwin REPLACE_WITH_THE_UNIVERSAL_DARWIN_SHA256
fill x86_64-unknown-linux-gnu REPLACE_WITH_THE_X86_64_LINUX_SHA256
fill aarch64-unknown-linux-gnu REPLACE_WITH_THE_AARCH64_LINUX_SHA256

if [ -n "$tap" ]; then
  mkdir -p "$tap/Formula"
  printf '%s\n' "$formula" > "$tap/Formula/inillucent.rb"
  echo
  echo "wrote $tap/Formula/inillucent.rb"
  echo "Then, in the tap:  git add Formula/inillucent.rb && git commit -m \"inillucent $version\" && git push"
else
  printf '%s\n' "$formula"
fi

if [ "$missing" -eq 1 ]; then
  echo
  echo "One or more archives were missing, so the formula still has placeholders in it." >&2
  echo "Build them on their own platforms and run this again before pushing the tap." >&2
  exit 1
fi
