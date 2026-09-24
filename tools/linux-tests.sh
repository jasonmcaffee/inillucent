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
ROOT=/mnt/c/jason/dev/inillucent
export CARGO_TARGET_DIR=/tmp/inillucent-linux-target
export INILLUCENT_SQLITE_ORACLE="$ROOT/.sqlite-ref/3.53.4/sqlite-oracle"
chmod +x "$INILLUCENT_SQLITE_ORACLE" 2>/dev/null || true
# Every crate that carries engine behaviour, which is the list the Windows
# evidence run covers. inillucent-ext, inillucent-search, inillucent-migrate
# and inillucent-cli used to be left out; they build and pass here, so leaving them
# out only meant the platform matrix said less than it could. The two retrieval
# crates - inillucent-core and inillucent-bench - stay out: they need the ONNX runtime
# and a corpus, and the frozen baseline is what covers them.
#
# inillucent-vm, inillucent-session and inillucent-capi came off this list when
# the old engine was deleted; inillucent-pool, inillucent-wal, inillucent-tree,
# inillucent-txn, inillucent-scalar, inillucent-exec, inillucent-engine,
# inillucent-driver and inillucent-driver-capi are what carry that behaviour now,
# matching `crates/inillucent-compat/src/bin/evidence.rs`'s own `PACKAGES` list.
cargo test --manifest-path "$ROOT/Cargo.toml" \
  -p inillucent-base -p inillucent-vfs -p inillucent-sim -p inillucent-value -p inillucent-storage \
  -p inillucent-transaction -p inillucent-sql -p inillucent-catalog -p inillucent-pool -p inillucent-wal \
  -p inillucent-tree -p inillucent-txn -p inillucent-scalar -p inillucent-exec -p inillucent-engine -p inillucent \
  -p inillucent-ext -p inillucent-search -p inillucent-migrate -p inillucent-driver -p inillucent-driver-capi -p inillucent-cli \
  -p inillucent-compat 2>&1 | grep -E 'test result|^error|FAILED'
