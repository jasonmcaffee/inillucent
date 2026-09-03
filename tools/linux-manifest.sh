#!/usr/bin/env bash
# Regenerates the compatibility report on Linux, to prove it is reproducible.
set -u
PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
export PATH
ROOT=/mnt/c/jason/dev/rust-db
export CARGO_TARGET_DIR=/tmp/rustdb-linux-target
cargo run --manifest-path "$ROOT/Cargo.toml" -p rustdb-compat --bin rustdb-manifest 2>&1 | tail -8
