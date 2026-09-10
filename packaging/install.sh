#!/usr/bin/env bash
#
# Installs inillucent for the current user on macOS or Linux.
#
#   curl -fsSL https://inillucent.com/downloads/install.sh | sh
#
# It downloads the release archive for this machine, checks its SHA-256 against
# the release's SHA256SUMS, unpacks it into ~/.local/share/inillucent and links
# the four programs into ~/.local/bin. Nothing is written outside the home
# directory and nothing needs sudo.
#
#   --version X.Y.Z   install a specific release rather than the latest
#   --base-url URL    download from somewhere else (default inillucent.com)
#   --from-dist       install the archive in dist/ instead of downloading
#   --prefix DIR      install somewhere else (default ~/.local/share/inillucent)
#   --bin-dir DIR     link the programs somewhere else (default ~/.local/bin)
#   --uninstall       remove what this installed
#
# It downloads from inillucent.com rather than from GitHub, because the
# repository is private and a private repository's release assets are private
# too: an unauthenticated request for one answers 404. The site is public and
# serves the same bytes and the same SHA256SUMS.
#
# On macOS this needs no Apple account and no notarisation on the reader's side,
# because a file fetched with curl carries no quarantine attribute. The signed
# .pkg on the site is the convenience for somebody who would rather
# double-click. See packaging/macos/README.md.

set -euo pipefail

base_url="https://inillucent.com/downloads"
version=""
from_dist=0
prefix="${HOME}/.local/share/inillucent"
bin_dir="${HOME}/.local/bin"
uninstall=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --base-url) base_url="${2%/}"; shift 2 ;;
    --from-dist) from_dist=1; shift ;;
    --prefix) prefix="$2"; shift 2 ;;
    --bin-dir) bin_dir="$2"; shift 2 ;;
    --uninstall) uninstall=1; shift ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

programs="inillucent inillucent-shell inillucent-mcp inillucent-migrate"

if [ "$uninstall" -eq 1 ]; then
  for program in $programs; do
    # Only a link this script made: a real program somebody put there by hand
    # is not ours to remove, and `-L` is what tells the two apart.
    if [ -L "$bin_dir/$program" ]; then rm -f "$bin_dir/$program"; fi
  done
  if [ -d "$prefix" ]; then rm -rf "$prefix"; echo "removed $prefix"; fi
  echo "uninstalled."
  exit 0
fi

# The target triple this machine's releases are built for.
kernel="$(uname -s)"
machine="$(uname -m)"
case "$kernel/$machine" in
  # One universal archive covers both Apple architectures, so a machine never
  # gets the half of the release it cannot also run under Rosetta.
  Darwin/arm64)  target="universal-apple-darwin" ;;
  Darwin/x86_64) target="universal-apple-darwin" ;;
  Linux/x86_64)  target="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64) target="aarch64-unknown-linux-gnu" ;;
  *)
    echo "there is no inillucent release for $kernel/$machine yet." >&2
    echo "Build from source instead:  cargo install inillucent-cli" >&2
    exit 1 ;;
esac

# Verifies a downloaded archive against the release's own SHA256SUMS.
#
# The whole reason to publish the file. A downloader that skips this has turned
# a truncated or tampered transfer into an installed program.
verify() {
  archive="$1"
  sums="$2"
  name="$(basename "$archive")"
  expected="$(awk -v want="$name" '$NF == want { print $1 }' "$sums" | head -1)"
  if [ -z "$expected" ]; then
    echo "SHA256SUMS does not list $name" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
  fi
  if [ "$actual" != "$expected" ]; then
    echo "$name does not match its published checksum." >&2
    echo "  expected $expected" >&2
    echo "  got      $actual" >&2
    exit 1
  fi
  echo "checksum ok ($expected)"
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [ "$from_dist" -eq 1 ]; then
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  dist="$root/dist"
  if [ -z "$version" ]; then
    found="$(ls "$dist"/inillucent-*-"$target".tar.gz 2>/dev/null | head -1 || true)"
    [ -n "$found" ] || { echo "no archive for $target in $dist. Run packaging/release.sh first." >&2; exit 1; }
    version="$(basename "$found" | sed "s/^inillucent-//; s/-$target\.tar\.gz$//")"
  fi
  archive="$dist/inillucent-$version-$target.tar.gz"
  [ -f "$archive" ] || { echo "$archive does not exist. Run packaging/release.sh first." >&2; exit 1; }
  verify "$archive" "$dist/SHA256SUMS"
else
  # VERSION, beside the archives, is what says which release is current. It is
  # one file on the same host as everything else here, so there is no second
  # service to be reachable and no API that can answer differently.
  if [ -z "$version" ]; then
    version="$(curl -fsSL "$base_url/VERSION" | tr -d '\r\n ')"
    [ -n "$version" ] || { echo "could not read $base_url/VERSION; pass --version" >&2; exit 1; }
  fi
  archive="$work/inillucent-$version-$target.tar.gz"
  echo "downloading inillucent $version for $target..."
  curl -fsSL "$base_url/inillucent-$version-$target.tar.gz" -o "$archive"
  curl -fsSL "$base_url/SHA256SUMS" -o "$work/SHA256SUMS"
  verify "$archive" "$work/SHA256SUMS"
fi

mkdir -p "$work/unpacked"
tar --directory "$work/unpacked" -xzf "$archive"
inner="$(find "$work/unpacked" -mindepth 1 -maxdepth 1 -type d | head -1)"
[ -n "$inner" ] || { echo "the archive is not shaped as expected: no directory inside it" >&2; exit 1; }

rm -rf "$prefix"
mkdir -p "$prefix"
cp -R "$inner"/. "$prefix"/

mkdir -p "$bin_dir"
for program in $programs; do
  ln -sf "$prefix/bin/$program" "$bin_dir/$program"
  chmod +x "$prefix/bin/$program"
done

echo
echo "inillucent $version is installed in $prefix"
"$prefix/bin/inillucent" --version

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    echo
    echo "$bin_dir is not on your PATH. Add it:"
    echo "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc && exec zsh"
    ;;
esac

cat <<INSTRUCTIONS

Try:
  inillucent create app.rdb
  inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
  inillucent --db app.rdb query "SELECT * FROM notes"

To give an agent the same commands, add this to its MCP configuration:

  "inillucent": {
    "type": "local",
    "command": ["$bin_dir/inillucent-mcp", "--db", "app.rdb"],
    "enabled": true
  }
INSTRUCTIONS
