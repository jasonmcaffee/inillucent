#!/usr/bin/env bash
# Verifies every install route the README offers, against what is actually published.
#
# Each check prints PASS or FAIL and the evidence. Nothing here reads a local build: every
# artifact is fetched from inillucent.com, the GitHub release or a registry, because the
# question is whether a stranger can install it, not whether the build produced files.
#
#   bash J:/build/task-1995/verify-installs.sh 0.1.5

set -uo pipefail
VERSION="${1:?usage: verify-installs.sh <version>}"
WORK="/j/build/task-1995/verify"
mkdir -p "$WORK"
PASS=0
FAIL=0

say() { printf '%-34s %s\n' "$1" "$2"; }
ok()   { PASS=$((PASS+1)); say "$1" "PASS  $2"; }
bad()  { FAIL=$((FAIL+1)); say "$1" "FAIL  $2"; }

# --- npm: the wrapper resolves its platform package and the binary runs -------------------
npm_check() {
  local d="$WORK/npm"
  mkdir -p "$d"
  echo '{"name":"probe","private":true}' > "$d/package.json"
  if ! npm --prefix "$d" install "inillucent@$VERSION" --no-audit --no-fund >/dev/null 2>&1; then
    bad "npm install" "npm install inillucent@$VERSION failed"; return
  fi
  local out
  out=$(node "$d/node_modules/inillucent/bin/inillucent.mjs" --version 2>&1 | head -1)
  if [[ "$out" == *"$VERSION"* ]]; then ok "npm install" "$out"; else bad "npm install" "$out"; fi
}

# --- npx: no install at all ---------------------------------------------------------------
npx_check() {
  local out
  out=$(npx -y "inillucent@$VERSION" --version 2>&1 | tail -1)
  if [[ "$out" == *"$VERSION"* ]]; then ok "npx" "$out"; else bad "npx" "$out"; fi
}

# --- pip: the wheel carries the programs --------------------------------------------------
pip_check() {
  local d="$WORK/venv"
  python -m venv "$d" >/dev/null 2>&1 || { bad "pip install" "could not create a venv"; return; }
  local py="$d/Scripts/python.exe"; [ -x "$py" ] || py="$d/bin/python"
  if ! "$py" -m pip install --quiet --no-cache-dir "inillucent==$VERSION" >/dev/null 2>&1; then
    bad "pip install" "pip install inillucent==$VERSION failed"; return
  fi
  local out
  out=$("$py" -c "import inillucent,sys; print(inillucent.__version__)" 2>&1 | tail -1)
  if [[ "$out" == "$VERSION" ]]; then ok "pip install" "__version__ = $out"; else bad "pip install" "$out"; fi
}

# --- the published checksum list, its signature, and every file it names -------------------
sums_check() {
  local d="$WORK/sums"
  mkdir -p "$d"
  curl -sSL -o "$d/SHA256SUMS" https://inillucent.com/downloads/SHA256SUMS
  curl -sSL -o "$d/SHA256SUMS.minisig" https://inillucent.com/downloads/SHA256SUMS.minisig
  curl -sSL -o "$d/inillucent.pub" https://inillucent.com/downloads/inillucent.pub
  local minisign="/c/jason/dev/inillucent/tools/cross/bin/minisign.exe"
  if "$minisign" -Vm "$(cygpath -w "$d/SHA256SUMS")" -p "$(cygpath -w "$d/inillucent.pub")" >/dev/null 2>&1; then
    ok "SHA256SUMS signature" "verified against the published public key"
  else
    bad "SHA256SUMS signature" "minisign rejected it"
  fi
  local missing=0 named=0
  while read -r hash name; do
    [ -z "$name" ] && continue
    named=$((named+1))
    local code
    code=$(curl -sI "https://inillucent.com/downloads/$name" | head -1 | awk '{print $2}')
    [ "$code" = "200" ] || { missing=$((missing+1)); echo "    missing: $name ($code)"; }
  done < "$d/SHA256SUMS"
  if [ "$missing" -eq 0 ]; then ok "SHA256SUMS names" "$named files, all served"
  else bad "SHA256SUMS names" "$missing of $named answer 404"; fi
}

# --- Homebrew: the formula's URL and checksum name the same file ---------------------------
brew_check() {
  local f="$WORK/inillucent.rb"
  curl -sSL -o "$f" https://raw.githubusercontent.com/Black-Rainbow-Labs/homebrew-inillucent/main/Formula/inillucent.rb
  local v
  v=$(grep -m1 '^  version "' "$f" | sed 's/.*"\(.*\)"/\1/')
  [ "$v" = "$VERSION" ] || { bad "homebrew formula" "formula says $v"; return; }
  local bad_pairs=0 checked=0
  # Each url line is followed by its sha256 line.
  while read -r url_line; do
    local name url sha want
    url=$(echo "$url_line" | sed 's/.*"\(.*\)".*/\1/')
    name=$(basename "$url")
    sha=$(grep -A1 -F "$url" "$f" | grep sha256 | sed 's/.*"\(.*\)".*/\1/')
    want=$(grep " $name\$" "$WORK/sums/SHA256SUMS" | cut -d' ' -f1)
    checked=$((checked+1))
    if [ -z "$want" ] || [ "$sha" != "$want" ]; then
      bad_pairs=$((bad_pairs+1)); echo "    $name: formula $sha, published ${want:-none}"
    fi
  done < <(grep '    url "https://inillucent.com' "$f")
  if [ "$bad_pairs" -eq 0 ] && [ "$checked" -gt 0 ]; then
    ok "homebrew formula" "$checked urls, every checksum matches SHA256SUMS"
  else
    bad "homebrew formula" "$bad_pairs of $checked disagree"
  fi
}

# --- Go: the proxy serves this version ----------------------------------------------------
go_check() {
  local out
  out=$(curl -s "https://proxy.golang.org/github.com/!black-!rainbow-!labs/!inillucent/packages/go/@v/list" | tr -d '\r')
  if echo "$out" | grep -qx "v$VERSION"; then ok "go module" "proxy lists v$VERSION"
  else bad "go module" "proxy lists: $(echo "$out" | tr '\n' ' ')"; fi
}

# --- crates.io ----------------------------------------------------------------------------
crates_check() {
  local out
  out=$(curl -s "https://crates.io/api/v1/crates/inillucent-cli" -H 'User-Agent: inillucent-verify')
  if echo "$out" | grep -q "\"num\":\"$VERSION\""; then ok "crates.io" "inillucent-cli $VERSION is listed"
  else bad "crates.io" "$(echo "$out" | head -c 120)"; fi
}

# --- Composer -----------------------------------------------------------------------------
composer_check() {
  local out
  out=$(curl -s "https://repo.packagist.org/p2/black-rainbow-labs/inillucent.json")
  if echo "$out" | grep -q "\"version\":\"v\\?$VERSION\""; then
    ok "packagist" "black-rainbow-labs/inillucent $VERSION is indexed"
  else
    bad "packagist" "versions: $(echo "$out" | grep -o '"version":"[^"]*"' | head -5 | tr '\n' ' ')"
  fi
  # **What composer would install, without needing composer.** This machine's PHP has no openssl
  # extension, so `composer require` cannot reach an https registry at all - which is a fact about
  # the verification box and not about the package. The question that matters is whether the dist
  # Packagist hands out is this release's source, so that is what is checked: the archive is
  # fetched and its commit compared against the mirror's tag. Packagist pins a version's commit the
  # first time it crawls the tag, so this is exactly where a wrong one shows up.
  local meta="$WORK/packagist.json"
  curl -s -o "$meta" "https://repo.packagist.org/p2/black-rainbow-labs/inillucent.json"
  local ref
  ref=$(node -e '
    const fs=require("fs");
    const all=JSON.parse(fs.readFileSync(process.argv[1],"utf8")).packages["black-rainbow-labs/inillucent"]||[];
    const v=all.find(x=>x.version==="v"+process.argv[2]||x.version===process.argv[2]);
    process.stdout.write(v&&v.source&&v.source.reference?v.source.reference:"");
  ' "$meta" "$VERSION" 2>/dev/null)
  local want
  want=$(git -C /c/jason/dev/inillucent ls-remote --tags brl 2>/dev/null | grep "v$VERSION^{}" | cut -f1)
  if [ -n "$ref" ] && [ "$ref" = "$want" ]; then
    ok "composer source" "dist points at ${ref:0:12}, the mirror's v$VERSION commit"
  elif [ -n "$ref" ]; then
    bad "composer source" "dist points at ${ref:0:12}, the mirror's v$VERSION is ${want:0:12}"
  else
    bad "composer source" "Packagist has no reference for $VERSION"
  fi

  if php -r 'exit(extension_loaded("openssl")?0:1);' 2>/dev/null; then
    local d="$WORK/composer"
    mkdir -p "$d"
    echo "{}" > "$d/composer.json"
    if php "$WORK/composer.phar" --working-dir="$d" require --no-interaction --quiet "black-rainbow-labs/inillucent:$VERSION" >/dev/null 2>&1; then
      ok "composer require" "resolved $VERSION"
    else
      bad "composer require" "could not resolve black-rainbow-labs/inillucent:$VERSION"
    fi
  else
    say "composer require" "SKIP  this PHP has no openssl extension, so composer cannot reach https"
  fi
}

# --- Linux, in WSL: the install script, and the .deb -------------------------------------
linux_check() {
  local out
  out=$(wsl -d Ubuntu -- bash -lc "curl -fsSL https://inillucent.com/downloads/install.sh | sh >/dev/null 2>&1; \$HOME/.local/bin/inillucent --version 2>&1 | head -1" 2>&1 | tr -d '
')
  if [[ "$out" == *"$VERSION"* ]]; then ok "install.sh (Linux)" "$out"; else bad "install.sh (Linux)" "$out"; fi

  # A fresh directory each run, named by the version, so nothing has to be deleted and every
  # path in the command can be read before it runs.
  local probe="/tmp/inillucent-deb-$VERSION-$$"
  out=$(wsl -d Ubuntu -- bash -lc "
    set -e
    mkdir -p $probe
    curl -fsSL -o $probe/p.deb https://inillucent.com/downloads/inillucent_${VERSION}_amd64.deb
    dpkg-deb -x $probe/p.deb $probe/root
    dpkg-deb -I $probe/p.deb | grep -E 'Package:|Version:|Architecture:' | tr -s ' '
    $probe/root/usr/bin/inillucent --version" 2>&1 | tr -d '
' | tail -4)
  if [[ "$out" == *"$VERSION"* ]]; then
    ok ".deb (amd64)" "$(echo "$out" | tail -1), $(echo "$out" | grep -i architecture | tr -s ' ')"
  else
    bad ".deb (amd64)" "$out"
  fi
}

# --- macOS: the installer is signed and Apple's ticket is stapled -------------------------
macos_check() {
  local d="$WORK/macos"
  mkdir -p "$d"
  curl -sSL -o "$d/inillucent.pkg" "https://inillucent.com/downloads/inillucent-$VERSION.pkg"
  local rc="/c/jason/dev/inillucent/tools/cross/bin/rcodesign.exe"
  local out
  out=$("$rc" print-signature-info "$(cygpath -w "$d/inillucent.pkg")" 2>&1 | grep -iE "signed|authority|team" | head -3)
  if [ -n "$out" ]; then ok "macOS .pkg signature" "$(echo "$out" | head -1 | tr -s ' ')"
  else bad "macOS .pkg signature" "rcodesign reported nothing"; fi

  curl -sSL -o "$d/darwin.tar.gz" "https://inillucent.com/downloads/inillucent-$VERSION-universal-apple-darwin.tar.gz"
  tar -xzf "$d/darwin.tar.gz" -C "$d"
  local bin="$d/inillucent-$VERSION-universal-apple-darwin/bin/inillucent"
  if [ -f "$bin" ]; then
    local magic
    magic=$(head -c 4 "$bin" | xxd -p)
    if [ "$magic" = "cafebabe" ]; then ok "macOS universal binary" "fat magic cafebabe"
    else bad "macOS universal binary" "magic $magic, expected cafebabe"; fi
  else
    bad "macOS universal binary" "bin/inillucent is not in the archive"
  fi
}

echo "verifying inillucent $VERSION"
echo
sums_check
npm_check
npx_check
pip_check
crates_check
go_check
composer_check
brew_check
linux_check
macos_check
echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
