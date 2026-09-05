#!/usr/bin/env bash
# Download every piece of public material the graded corpus is built from.
#
# Nothing downloaded here is committed to this repository. The corpus is rebuilt
# from these sources on demand, which keeps the repository small and satisfies the
# share alike licences of the Wikipedia text by attribution rather than by
# redistribution.
#
# Sources and licences:
#   English and Simple English Wikipedia, CirrusSearch dumps      CC BY-SA 4.0
#   tokio, vuejs/core, react, symfony                             MIT
#   pandas                                                        BSD 3 Clause
#   hugo, moby, airflow                                           Apache 2.0
#   GitHub issue threads from those repositories                  factual metadata
#
# The CirrusSearch dumps are used rather than the page dumps because they already
# contain plain text and an array of section headings, so no wikitext has to be
# parsed.
set -euo pipefail

# Resolved before the cd below. `dirname "$0"` afterwards would be relative to
# $RAW, so a relative invocation could not find the extract scripts.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

RAW="${RAW:-$HOME/.cache/inillucent-corpus/raw}"
DERIVED="${DERIVED:-$HOME/.cache/inillucent-corpus/derived}"
DUMP_DATE="${DUMP_DATE:-20251222}"
# Long articles wanted from the English dump. It is 43 GB, far too large to
# download, so it is streamed and the reader stops once it has this many.
ENWIKI_ARTICLES="${ENWIKI_ARTICLES:-60000}"
# The extract scripts run under whichever interpreter this box calls Python. A
# Windows install provides "python" and no "python3", so the name is resolved
# rather than hard coded.
PYTHON="${PYTHON:-}"
if [ -z "$PYTHON" ]; then
  if command -v python3 > /dev/null; then PYTHON=python3; else PYTHON=python; fi
fi

# Some networks reject a request without a browser user agent.
UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
BASE="https://dumps.wikimedia.org/other/cirrussearch/$DUMP_DATE"

REPOS=(
  "tokio-rs/tokio"      # Rust
  "pandas-dev/pandas"   # Python
  "vuejs/core"          # TypeScript
  "gohugoio/hugo"       # Go
  "apache/airflow"      # Python
  "facebook/react"      # JavaScript
  "moby/moby"           # Go
  "symfony/symfony"     # PHP
)

mkdir -p "$RAW/repos" "$DERIVED"
cd "$RAW"

echo "== Wikipedia dumps =="
# The Simple English dumps are small enough to keep whole. The content dump gives
# the articles; the general dump gives the Talk pages, which are the only real
# threaded conversation in any of these sources.
for name in content general; do
  file="simplewiki-$DUMP_DATE-cirrussearch-$name.json.gz"
  if [ ! -s "$file" ]; then
    echo "downloading $file"
    curl --fail --location --silent --show-error --max-time 3000 \
      -A "$UA" -o "$file" "$BASE/$file"
  fi
  # A stable name the extract script reads. Symlinks are not universally
  # available (Git Bash on Windows makes one the reader cannot open), so fall
  # back to a copy when the link does not resolve.
  ln -sf "$file" "simplewiki-$name.json.gz" 2>/dev/null || true
  if [ ! -s "simplewiki-$name.json.gz" ]; then
    ln -f "$file" "simplewiki-$name.json.gz" 2>/dev/null || cp -f "$file" "simplewiki-$name.json.gz"
  fi
done

echo "== source repositories =="
for repo in "${REPOS[@]}"; do
  name="$(basename "$repo")"
  if [ ! -d "repos/$name" ]; then
    echo "cloning $repo"
    git clone --depth 1 --single-branch --quiet "https://github.com/$repo.git" "repos/$name"
  fi
done

echo "== issue threads =="
# Bounded rather than paginated to the end. These repositories hold tens of
# thousands of issues and the corpus needs about two thousand, so fetching every
# page would spend hundreds of API requests for nothing.
# gh is used when it is installed and authenticated, because its rate limit is
# 5000 requests an hour. Otherwise the same endpoint is called unauthenticated,
# whose limit is 60 requests an hour: this needs 48, so it fits.
use_gh=no
if command -v gh > /dev/null && gh auth status > /dev/null 2>&1; then use_gh=yes; fi
for repo in "${REPOS[@]}"; do
  name="$(basename "$repo")"
  out="issues-$name.json"
  if [ ! -s "$out" ]; then
    echo "fetching issues for $repo"
    : > "$out"
    for page in 1 2 3 4 5 6; do
      if [ "$use_gh" = yes ]; then
        gh api "repos/$repo/issues?state=all&per_page=100&sort=created&direction=desc&page=$page" >> "$out" 2>/dev/null || break
      else
        curl --fail --location --silent --show-error -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28" "https://api.github.com/repos/$repo/issues?state=all&per_page=100&sort=created&direction=desc&page=$page" >> "$out" || break
      fi
    done
  fi
done

echo "== extracting =="
"$PYTHON" "$SCRIPT_DIR/extract-wikipedia.py" "$RAW" "$DERIVED"

# The English dump is streamed and the reader stops early, so only the first part
# of the 43 GB file is ever transferred.
if [ ! -s "$DERIVED/enwiki-articles.jsonl" ]; then
  echo "streaming $ENWIKI_ARTICLES articles from the English dump"
  curl --fail --location --silent --show-error --max-time 3000 \
    -A "$UA" "$BASE/enwiki-$DUMP_DATE-cirrussearch-content.json.gz" \
    | "$PYTHON" "$SCRIPT_DIR/extract-wikipedia.py" - "$DERIVED" \
        --name enwiki-articles.jsonl --min-chars 3000 --limit "$ENWIKI_ARTICLES" || true
fi

"$PYTHON" "$SCRIPT_DIR/extract-github.py" "$RAW" "$DERIVED"

echo
echo "the public material is in $DERIVED"
echo "next: inillucent-bench synth-build, then synth-check, then synth-embed"
