#!/usr/bin/env bash
# Records the engine evidence on Linux.
#
# The workspace is shared between Windows and WSL, so the build directory has to
# be a Linux one: sharing `target/` between the two would have each toolchain
# overwrite the other's artifacts every run.
set -u
PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export PATH
ROOT=/mnt/c/jason/dev/rust-db
export CARGO_TARGET_DIR=/tmp/rustdb-linux-target
# The Linux oracle is the ELF binary next to the Windows one.
export RUSTDB_SQLITE_ORACLE="$ROOT/.sqlite-ref/3.53.4/sqlite-oracle"
chmod +x "$RUSTDB_SQLITE_ORACLE" 2>/dev/null || true
cargo run --manifest-path "$ROOT/Cargo.toml" -p rustdb-compat --bin rustdb-evidence 2>&1 | tail -25
