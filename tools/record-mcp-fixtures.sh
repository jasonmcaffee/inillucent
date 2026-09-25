#!/bin/sh
# Records the two MCP transcripts `crates/inillucent-compat/tests/e2e/mcp_replay.rs`
# replays, through `tools/record-mcp-transcript.py`.
#
# Usage, from anywhere:
#
#     sh tools/record-mcp-fixtures.sh
#     BUILD=target/release sh tools/record-mcp-fixtures.sh
#
# Run it again when a fixture goes stale - `mcp_replay.rs` says so when it does,
# and names the file. The database both clients talk to is built here by the
# shipped command line, so the fixture is what a user would have rather than
# what a library would write.
set -e

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
BUILD=${BUILD:-target/debug}
case "$BUILD" in
  /*|[A-Za-z]:*) BIN="$BUILD" ;;
  *) BIN="$ROOT/$BUILD" ;;
esac
OUT="$ROOT/crates/inillucent-compat/tests/fixtures/mcp"
SCRATCH="$ROOT/_agent_output/mcp-transcripts"

EXE=""
if [ -x "$BIN/inillucent.exe" ]; then EXE=".exe"; fi
CLI="$BIN/inillucent$EXE"
SERVER="$BIN/inillucent-mcp$EXE"
for program in "$CLI" "$SERVER"; do
  if [ ! -x "$program" ]; then
    echo "$program is not built; run: cargo build -p inillucent-cli" >&2
    exit 1
  fi
done

rm -rf "$SCRATCH"
mkdir -p "$SCRATCH" "$OUT"

# One ordinary table with two rows.
"$CLI" create "$SCRATCH/smoke.rdb" > /dev/null
"$CLI" --db "$SCRATCH/smoke.rdb" exec "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)" > /dev/null
"$CLI" --db "$SCRATCH/smoke.rdb" exec "INSERT INTO t (body) VALUES ('one')" > /dev/null
"$CLI" --db "$SCRATCH/smoke.rdb" exec "INSERT INTO t (body) VALUES ('two')" > /dev/null

# 1. The release smoke test's client, line for line from packaging/release.sh.
printf '%s\n%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"release-smoke","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inillucent_query","arguments":{"sql":"SELECT count(*) FROM t"}}}' \
  | python "$HERE/record-mcp-transcript.py" \
      --out "$OUT/release-smoke.transcript" \
      --note "The four lines packaging/release.sh sends to the installed server. 0.1.2 shipped a handshake this client refused, and running the release was what found it." \
      -- "$SERVER" --db "$SCRATCH/smoke.rdb" > /dev/null

# 2. An agent client: Claude Code's own handshake version and client name, then
#    the calls an agent makes when it meets a database it has not seen.
printf '%s\n%s\n%s\n%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"claude-code","version":"1.0.0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"inillucent_tables","arguments":{}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inillucent_describe","arguments":{"table":"t"}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"inillucent_query","arguments":{"sql":"SELECT id, body FROM t ORDER BY id"}}}' \
  | python "$HERE/record-mcp-transcript.py" \
      --out "$OUT/claude-code.transcript" \
      --note "The handshake Claude Code sends and the first four calls an agent makes: list the tools, look at the tables, describe one, read it." \
      -- "$SERVER" --db "$SCRATCH/smoke.rdb" > /dev/null

wc -l "$OUT/release-smoke.transcript" "$OUT/claude-code.transcript"
