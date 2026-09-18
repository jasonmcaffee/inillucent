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
# It downloads from inillucent.com rather than from GitHub. That was originally
# because the repository was private and a private repository's release assets
# are private too - an unauthenticated request for one answered 404 - and both
# repositories are public as of task-1961. The site stays the download because
# it is the one URL every installer on every platform reads: install.ps1, the
# Homebrew formula, the Go cmd/inillucent-install and the PHP
# bin/inillucent-install all read inillucent.com/downloads and check the SHA-256
# against the published SHA256SUMS. One place to publish is one place to get
# wrong.
#
# On macOS this needs no Apple account and no notarisation on the reader's side,
# because a file fetched with curl carries no quarantine attribute. The signed
# .pkg on the site is the convenience for somebody who would rather
# double-click. See packaging/macos/README.md.

set -eu
# `pipefail` is not POSIX. The documented install is `curl ... | sh`, and on
# Debian and Ubuntu that `sh` is dash, which answers `set: Illegal option -o
# pipefail` and stops before anything is downloaded. So it is enabled only where
# the shell has it, in a subshell that cannot take the script down with it.
# shellcheck disable=SC3040
(set -o pipefail 2>/dev/null) && set -o pipefail || true

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
  # tr -d '
' because a SHA256SUMS written on Windows arrives with CRLF, and
  # awk on Linux keeps the carriage return in $NF - so the name never matches
  # and a correct download is reported as unpublished.
  expected="$(tr -d '
' < "$sums" | awk -v want="$name" '$NF == want { print $1 }' | head -1)"
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
  # `$0` rather than `${BASH_SOURCE[0]}`: the latter is a bash array and dash
  # cannot read it. This branch only runs for a script on disk, which is the
  # case where `$0` is that script.
  root="$(cd "$(dirname "$0")/.." && pwd)"
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
  name="inillucent-$version-$target.tar.gz"

  # SHA256SUMS lists every archive this release published, and it is fetched
  # anyway to verify the download - so it is fetched *first*, and used to answer
  # "is there a build for this machine" before asking for one. Without this a
  # platform that is not published yet gets `curl -fsSL` failing on a 404, which
  # under `set -e` ends the script with nothing printed at all.
  curl -fsSL "$base_url/SHA256SUMS" -o "$work/SHA256SUMS.raw"
  # A SHA256SUMS produced on Windows arrives with CRLF, and awk on Linux keeps
  # the carriage return in the file name - so every lookup below fails while the
  # listing prints names that look exactly right. Strip it once, here.
  tr -d '\r' < "$work/SHA256SUMS.raw" > "$work/SHA256SUMS"
  if ! awk -v want="$name" '$NF == want { found = 1 } END { exit !found }' "$work/SHA256SUMS"; then
    echo "inillucent $version has no build for $target yet." >&2
    echo "  published in this release:" >&2
    awk '{ print "    " $NF }' "$work/SHA256SUMS" >&2
    echo "  Build it from source instead:" >&2
    echo "    git clone https://github.com/Black-Rainbow-Labs/Inillucent" >&2
    echo "    cargo build --release -p inillucent-cli" >&2
    exit 1
  fi

  echo "downloading inillucent $version for $target..."
  curl -fsSL "$base_url/$name" -o "$archive"
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

# The file to write the line into is the one this shell reads, which is not
# always ~/.zshrc: Ubuntu's login shell is bash and Debian's is dash. Naming
# ~/.zshrc on a machine running bash printed an instruction that adds the line
# to a file nothing reads (task-1979, E5).
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    case "$(basename "${SHELL:-sh}")" in
      zsh) profile='~/.zshrc'; reload='exec zsh' ;;
      bash) profile='~/.bashrc'; reload='exec bash' ;;
      fish) profile='~/.config/fish/config.fish'; reload='exec fish' ;;
      *) profile='~/.profile'; reload='. ~/.profile' ;;
    esac
    echo
    echo "$bin_dir is not on your PATH. Add it:"
    if [ "$profile" = '~/.config/fish/config.fish' ]; then
      echo "  echo 'fish_add_path $bin_dir' >> $profile && $reload"
    else
      echo "  echo 'export PATH=\"$bin_dir:\$PATH\"' >> $profile && $reload"
    fi
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
