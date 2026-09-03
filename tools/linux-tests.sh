#!/usr/bin/env bash
# Runs the engine test suite on Linux.
#
# The workspace is shared between Windows and WSL, so the build directory has to
# be a Linux one: sharing `target/` between the two would have each toolchain
# overwrite the other's artifacts every run. The oracle is the ELF binary next
# to the Windows one, and it has to be executable, which a checkout onto a
# Windows filesystem does not preserve.
set -u
PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export PATH
ROOT=/mnt/c/jason/dev/rust-db
export CARGO_TARGET_DIR=/tmp/rustdb-linux-target
export RUSTDB_SQLITE_ORACLE="$ROOT/.sqlite-ref/3.53.4/sqlite-oracle"
chmod +x "$RUSTDB_SQLITE_ORACLE" 2>/dev/null || true
cargo test --manifest-path "$ROOT/Cargo.toml" \
  -p rustdb-base -p rustdb-vfs -p rustdb-sim -p rustdb-value -p rustdb-storage \
  -p rustdb-transaction -p rustdb-sql -p rustdb-catalog -p rustdb-vm -p rustdb-session -p rustdb \
  -p rustdb-compat 2>&1 | grep -E 'test result|^error|FAILED'
