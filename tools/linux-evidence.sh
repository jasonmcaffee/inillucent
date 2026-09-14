#!/usr/bin/env bash
# Records the engine evidence on Linux.
#
# The workspace is shared between Windows and WSL, so the build directory has to
# be a Linux one: sharing `target/` between the two would have each toolchain
# overwrite the other's artifacts every run.
set -u
PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export PATH
# **The checkout to record, which is not always the same one (task-1946, M5).**
# This named one absolute path, so a run from a worktree - which is how every
# recent ticket in this repository has worked - recorded the wrong checkout's
# evidence into the wrong checkout's `compat/results`. It defaults to the
# checkout this script is in, resolved through `/mnt/` when it is a Windows one,
# and `INILLUCENT_ROOT` overrides it.
ROOT="${INILLUCENT_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
export CARGO_TARGET_DIR=/tmp/inillucent-linux-target
# The Linux oracle is the ELF binary next to the Windows one.
export INILLUCENT_SQLITE_ORACLE="$ROOT/.sqlite-ref/3.53.4/sqlite-oracle"
chmod +x "$INILLUCENT_SQLITE_ORACLE" 2>/dev/null || true
cargo run --manifest-path "$ROOT/Cargo.toml" -p inillucent-compat --bin inillucent-evidence 2>&1 | tail -25
