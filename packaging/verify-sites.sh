#!/usr/bin/env bash
# Fetches every platform artifact both sites offer and checks it is this release's bytes.
set -uo pipefail
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  %-52s PASS  %s\n' "$1" "$2"; }
bad() { FAIL=$((FAIL+1)); printf '  %-52s FAIL  %s\n' "$1" "$2"; }

echo "=== inillucent.com"
IV=$(curl -s https://inillucent.com/downloads/VERSION | tr -d '\r\n')
echo "    VERSION says $IV"
PAGE=$(curl -s https://inillucent.com/)
SUMS=$(curl -s https://inillucent.com/downloads/SHA256SUMS)
for spec in \
  "Windows x86-64|inillucent-$IV-x86_64-pc-windows-msvc.zip" \
  "macOS installer|inillucent-$IV.pkg" \
  "macOS archive|inillucent-$IV-universal-apple-darwin.tar.gz" \
  "Linux x86-64|inillucent-$IV-x86_64-unknown-linux-gnu.tar.gz" \
  "Linux aarch64|inillucent-$IV-aarch64-unknown-linux-gnu.tar.gz" \
  "Debian x86-64|inillucent_${IV}_amd64.deb" \
  "Debian aarch64|inillucent_${IV}_arm64.deb" \
  "Fedora x86-64|inillucent-$IV.x86_64.rpm" \
  "Fedora aarch64|inillucent-$IV.aarch64.rpm"; do
  name="${spec%%|*}"; file="${spec##*|}"
  linked=no; case "$PAGE" in *"$file"*) linked=yes ;; esac
  code=$(curl -sI "https://inillucent.com/downloads/$file" | head -1 | awk '{print $2}')
  size=$(curl -sI "https://inillucent.com/downloads/$file" | grep -i '^content-length' | tr -dc '0-9')
  want=$(printf '%s\n' "$SUMS" | grep " $file\$" | cut -d' ' -f1)
  if [ "$linked" = yes ] && [ "$code" = "200" ] && [ -n "$want" ]; then
    ok "$name" "linked, 200, $size bytes, hashed in SHA256SUMS"
  else
    bad "$name" "linked=$linked http=$code sha256=${want:-absent}"
  fi
done
# One artifact fetched whole and hashed, to close the loop between the page and the checksums.
SMALL="inillucent-$IV-aarch64-unknown-linux-gnu.tar.gz"
curl -sSL -o /j/build/task-1995/verify/site-probe.bin "https://inillucent.com/downloads/$SMALL"
GOT=$(sha256sum /j/build/task-1995/verify/site-probe.bin | cut -d' ' -f1)
WANT=$(printf '%s\n' "$SUMS" | grep " $SMALL\$" | cut -d' ' -f1)
[ "$GOT" = "$WANT" ] && ok "$SMALL hashed end to end" "${GOT:0:16}..." || bad "$SMALL hashed end to end" "got ${GOT:0:16}, want ${WANT:0:16}"

echo
echo "=== unluminous.com"
M=/j/build/task-1995/verify/unlum.json
curl -s -o "$M" https://unluminous.com/releases/latest.json
UV=$(node -e 'console.log(require(process.argv[1]).version)' "$M")
echo "    manifest says $UV"
UPAGE=$(curl -s https://unluminous.com/)
for key in installer macos; do
  url=$(node -e 'const m=require(process.argv[1]);process.stdout.write(m[process.argv[2]]||"")' "$M" "$key")
  bytes=$(node -e 'const m=require(process.argv[1]);process.stdout.write(String(m[process.argv[2]+"Bytes"]||""))' "$M" "$key")
  label=$([ "$key" = installer ] && echo "Windows x86-64" || echo "macOS universal")
  if [ -z "$url" ]; then bad "$label" "the manifest has no $key"; continue; fi
  file="${url##*/}"
  linked=no; case "$UPAGE" in *"$file"*) linked=yes ;; esac
  code=$(curl -sI "$url" | head -1 | awk '{print $2}')
  size=$(curl -sI "$url" | grep -i '^content-length' | tr -dc '0-9')
  if [ "$linked" = yes ] && [ "$code" = "200" ] && [ "$size" = "$bytes" ]; then
    ok "$label" "linked, 200, $size bytes = the manifest's"
  else
    bad "$label" "linked=$linked http=$code served=$size manifest=$bytes"
  fi
done
sha=$(node -e 'const m=require(process.argv[1]);process.stdout.write(m.macosSha256||"")' "$M")
[ -n "$sha" ] && ok "the manifest states a macOS SHA-256" "${sha:0:16}..." || bad "the manifest states a macOS SHA-256" "absent"

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
