# inillucent release candidate

**This candidate does not pass.** The gates it fails are marked below, with the numbers they were judged against. Nothing here argues that a number is acceptable: the bars were written down before the runs.

## Gates

| gate | verdict | detail |
|---|---|---|
| `compatibility` | pass | 263 capabilities pass, 13 not implemented, 0 unsupported claims |
| `checkpoint` | **FAIL** | worst commit over median: scheduled 19.3x, all_at_once 20.0x (bar 10x) |
| `performance` | **FAIL** | no scorecard has been run |
| `artifacts` | **FAIL** | missing: target/release/inillucent-shell.exe, target/release/inillucent-migrate.exe, target/release/inillucent_driver_capi.dll, _agent_output/measurements/migrate/release/corpus.db.migration-report.md, _agent_output/measurements/migrate/release/corpus.db.migration-manifest, _agent_output/measurements/migrate/full/corpus.db.migration-report.md, _agent_output/measurements/scorecard/scorecard.md, _agent_output/measurements/scorecard/arm-no-covering-index.md, _agent_output/measurements/scorecard/arm-no-indexed-write.md, _agent_output/measurements/scorecard/arm-no-ordered-walk.md, _agent_output/measurements/scorecard/arm-no-streaming-group.md, _agent_output/measurements/scorecard/arm-no-fused-bytecode.md, _agent_output/measurements/scorecard/scorecard.json, _agent_output/measurements/scorecard/history.jsonl, _agent_output/measurements/scorecard/dashboard.md |

## Supported platforms

| platform | evidence |
|---|---|
| `windows-x86_64` (this machine) | compatibility evidence only |
| `linux-x86_64` | compatibility evidence only |

## Artifacts

| file | bytes | sha256 | what it is |
|---|---:|---|---|
| `compat/compat-report.md` | 22981 | `b7a6648d5eaa83f94c149ebe7c2a4266ad5dcdb06cd5af134911feedbb7e0b6e` | the compatibility report |
| `compat/compat-report.json` | 125458 | `6ec3ea6e0160c0dd877f3996c4ac5a6eb9d167d5bfc663b4ce2906ae44a3388c` | the same, machine readable |
| `compat/sqlite-3.53.4.toml` | 136078 | `1985448972032b6eef5a6eb74765d22559220bfa740b09d91ebd7228ca11f6a4` | the parity manifest the report is generated from |
| `compat/perf/contract.toml` | 4745 | `8e55151807cfa80b187743882773d760ef042eda45ad6a912b576afeaf048d59` | the performance contract: weights, floors and the headline bound |
| `docs/reference-register.toml` | 3783 | `ee2deae860ab3c6f73651e2b30c90deb7eeb79d3cbc14e05e209583f59db4fef` | every external project consulted, and in what capacity |
| `docs/invariants/layering.toml` | 42879 | `6952834525586310f939c2b3abf88950f68478d661698cbcafd06717bd1ab54a` | the dependency-direction contract |
| `compat/baseline/inillucent-core-baseline.json` | 6068 | `3e13be695a40b4eb200153a15effbbebcdf7f25005487c37428471de605d8437` | the retrieval engine's frozen baseline |
| `compat/baseline/inillucent-core-amendments.toml` | 27604 | `6204e097f5eb783ec088161b5902bf73d138398badd79f5ce0b653d1084f14f4` | every declared change to it, with its reason |
| `compat/release/checkpoint-checkpoint.md` | 1225 | `1e2dff4f9d16bea866936e718b22cb424dde6cf25e8cda8cc995ebe1ce93c286` | the checkpoint-scheduling lever, measured against its own arm and left off |

Not present in this candidate:

- `target/release/inillucent-shell.exe`
- `target/release/inillucent-migrate.exe`
- `target/release/inillucent_driver_capi.dll`
- `_agent_output/measurements/migrate/release/corpus.db.migration-report.md`
- `_agent_output/measurements/migrate/release/corpus.db.migration-manifest`
- `_agent_output/measurements/migrate/full/corpus.db.migration-report.md`
- `_agent_output/measurements/scorecard/scorecard.md`
- `_agent_output/measurements/scorecard/arm-no-covering-index.md`
- `_agent_output/measurements/scorecard/arm-no-indexed-write.md`
- `_agent_output/measurements/scorecard/arm-no-ordered-walk.md`
- `_agent_output/measurements/scorecard/arm-no-streaming-group.md`
- `_agent_output/measurements/scorecard/arm-no-fused-bytecode.md`
- `_agent_output/measurements/scorecard/scorecard.json`
- `_agent_output/measurements/scorecard/history.jsonl`
- `_agent_output/measurements/scorecard/dashboard.md`

## Reproducing this

A clean machine with a Rust toolchain and a C compiler reproduces every artifact above with these commands, in this order. Nothing needs another database engine installed: the reference is downloaded, checksum-verified against the sums SQLite publishes, and built from source into `.sqlite-ref/`, which no inillucent crate links against.

| artifact | command |
|---|---|
| the pinned reference | `tools/sqlite-reference.ps1   # or tools/sqlite-reference.sh on POSIX` |
| the engine and its tools | `cargo build --release --workspace` |
| the test evidence | `cargo run --release -p inillucent-compat --bin inillucent-evidence` |
| the compatibility report | `cargo run --release -p inillucent-compat --bin inillucent-manifest` |
| the performance scorecard | `cargo run --release -p inillucent-compat --bin inillucent-scorecard -- --scale all --rounds 30 --label <name>` |
| each optimization's A/B arm | `cargo run --release -p inillucent-compat --bin inillucent-scorecard -- --scale small --rounds 30 --label <name> --disable covering-index   # then --disable indexed-write` |
| the storage and write profiles | `cargo run --release -p inillucent-compat --bin inillucent-storageprofile && cargo run --release -p inillucent-compat --bin inillucent-writeprofile` |
| a legacy index migration | `cargo run --release -p inillucent-migrate -- <index-dir> <destination.db> --sqlite .sqlite-ref/3.53.4/shell/sqlite3` |
| the checkpoint distribution | `cargo run --release -p inillucent-compat --bin inillucent-checkpointperf` |
| this release candidate | `cargo run --release -p inillucent-compat --bin inillucent-release` |

## Upgrade and downgrade

The default writer produces the SQLite file format and nothing else, so an upgrade is a binary swap: the file a previous build wrote is the file this one opens, and the file this one writes is one the pinned SQLite opens. That is what the interoperability suites check in both directions, and what the migration tool's own probe checks on the database it just built.

A downgrade is the same swap in reverse, with one condition: a database holding a `inillucent_search` table is read by any build - the index lives in ordinary tables - but it is *queried* only by a build that has the module. An older build opens the file, reads every relational table, and reports `no such module: inillucent_search` for the search table alone.

Legacy retrieval indexes migrate with `inillucent-migrate`, which never writes to the source. Going back is not an undo; it is pointing the application at the directory that never changed.

## Known limitations

- **Performance.** The engine is slower than the pinned reference on every family except point reads by rowid, which it wins. The measured cause is the virtual machine rather than the storage layer: one step of a table scan costs 3.7 ns, reading the row 21.7, finding its fields 35.7 and decoding an integer 38.2 - while the machine on top of that costs about two hundred nanoseconds per column read and comparison. Closing it is opcode-level work: borrowed values through the whole register file, fused superinstructions, and specialised scan loops.
- **Seven optional SQLite surfaces are not implemented**, and the manifest carries a row for each so the denominator says so: the session extension, the pre-update hook, the snapshot API, `unlock_notify`, RBU, geopoly, and the R-Tree geometry callbacks. None of them is reachable from SQL or from the file format, so a database written by this engine is not affected by their absence - an application that calls them is.
- **Two of the TDD's optimisation levers were implemented, measured, and left off.** Checkpoint scheduling - bounding how many frames one automatic checkpoint copies, so the cost is spread over the commits that caused it - makes no difference, and its own counters say why: the checkpoint already runs after nearly every commit once the log passes its threshold, about 5,700 times in 6,000, so there is no accumulated batch to spread. The bound is a tunable rather than a default and the shipped behaviour is unchanged. Group commit has nothing to group: writers are serialised, so transactions do not overlap, and a commit already takes exactly the barriers the reference takes - one sync in a write-ahead log at `synchronous=full`, none at `normal`, two in a rollback journal. The evidence is that the two families where the barrier dominates are at parity: `write.insert.autocommit` at 0.99x and `txn.autocommit` at 0.92x. Sharing one barrier between two writers would need overlapping write transactions, which is a change to the locking rather than a tuning lever.
- **Vectorisation and SIMD are not applicable to this execution model.** The lever's name pairs them with bytecode fusion, which is implemented; the other two need a columnar or batched interpreter, where one instruction works on many rows. This is a row-at-a-time virtual machine, so there is no vector for an instruction to act on, and saying so is more use than a benchmark of nothing.
- **FTS5's segment format inside `%_data` is first-party.** SQLite's is described only in comments in `fts5_index.c` and is explicitly not a published format, unlike the R-Tree's. What is matched is everything a reader outside the module sees: the five table names, the layouts of `%_content`, `%_docsize` and `%_config`, the rows `MATCH` finds, their order, and `bm25()` to the last digit.
- **A database opened through a caller-supplied C VFS journals rather than using a write-ahead log.** `xShmMap` is the easiest part of the VFS contract to get subtly wrong, and a log over a broken one corrupts silently.
- **A `inillucent_search` table has one row per document**, so the per-document cap in the fusion never binds on it. An application that wants documents made of several chunks models them in SQL - a document table and a join - which is what the migration tool writes and what its own grouped check verifies.
- **The legacy engine's lexical ranking depends on the `k` it was asked for.** Position-aware rescoring reaches `k * lexical_rescore_depth` hits and only ever scales a score down, so a chunk just outside that window keeps its full BM25 score and competes against rescored ones: ask for ten and ask for fifty, and the tail of the ranking moves. Every path a search table sits behind goes through `search_branches`, which retrieves at `candidates` depth, so the two agree when asked at that depth and can differ when they are not. The migration compares them at one depth for exactly that reason. A corpus of real prose is what exposed it: a small one has fewer chunks than the window, so the window never binds.
- **A migrated vector index is not the same graph.** The legacy index's graph grew one insert at a time; a migrated one is built in a single pass over every row, which is better connected - that is what makes compaction worth its cost. Two different graphs searched approximately give slightly different answers, sometimes one better and sometimes the other, so the migration compares them with the approximation switched off and reports separately what each finds of the exact answer at its default width. An application that depends on a particular ranking of near-ties should expect it to move.
- **A tombstone and a delete differ.** The legacy engine keeps a tombstoned chunk in the inverted index and filters it at query time, so its corpus statistics do not move; a search table deletes the row, so they do. Both make the document unreachable immediately; deep orderings can differ until the legacy index is rebuilt.
- **A migration is refused when the source was built with a non-zero heading boost.** A migrated search table indexes a chunk's text without its heading structure, which contributes nothing at the measured default of zero and would contribute at anything else - and the migrated index would then rank differently in a way nothing about it looked wrong.
