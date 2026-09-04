# rust-db release candidate

**This candidate does not pass.** The gates it fails are marked below, with the numbers they were judged against. Nothing here argues that a number is acceptable: the bars were written down before the runs.

## Gates

| gate | verdict | detail |
|---|---|---|
| `compatibility` | pass | 264 capabilities pass, 7 not implemented, 0 unsupported claims |
| `performance.headline.small` | **FAIL** | weighted geometric mean 0.205x, lower bound 0.200x against a bound of 1.50x |
| `performance.floors.small` | **FAIL** | below the 0.90x floor: open.prepare at 0.134x, read.point at 0.727x, read.range at 0.050x, read.analytical at 0.047x, read.join at 0.206x, write at 0.189x, transaction at 0.257x, schema at 0.088x, extension at 0.077x, large.values at 0.723x |
| `performance.headline.medium` | **FAIL** | weighted geometric mean 0.137x, lower bound 0.134x against a bound of 1.50x |
| `performance.floors.medium` | **FAIL** | below the 0.90x floor: open.prepare at 0.124x, read.point at 0.579x, read.range at 0.016x, read.analytical at 0.021x, read.join at 0.122x, write at 0.136x, transaction at 0.258x, schema at 0.009x, extension at 0.082x, large.values at 0.708x |
| `performance.headline.large` | **FAIL** | weighted geometric mean 0.143x, lower bound 0.139x against a bound of 1.50x |
| `performance.floors.large` | **FAIL** | below the 0.90x floor: open.prepare at 0.122x, read.point at 0.722x, read.range at 0.008x, read.analytical at 0.021x, read.join at 0.119x, write at 0.236x, transaction at 0.194x, schema at 0.002x, extension at 0.131x |
| `performance.regressions` | pass | no workload has been slower than its best for two consecutive runs |
| `artifacts` | pass | 20 artifacts present and digested |

## Supported platforms

| platform | evidence |
|---|---|
| `windows-x86_64` (this machine) | compatibility evidence and a performance scorecard |
| `linux-x86_64` | compatibility evidence only |

## Artifacts

| file | bytes | sha256 | what it is |
|---|---:|---|---|
| `target/release/rustdb-shell.exe` | 4839424 | `dbc2bc4a90ce662eb3fd8cdd44e78f8ec7408509b36f5ea5726352fb4dbe9108` | the SQLite-like shell |
| `target/release/rustdb-migrate.exe` | 5042176 | `31479b1e293380d25087b9e2ca5b34530b8811b2d0bf96ac681be42173c7a5dc` | the resumable copy-and-verify migration tool |
| `target/release/rustdb_capi.dll` | 4841472 | `7bac93c7ae04d52e70967527f32e03218632d6d1720d01b005f20526372436c3` | the C library, linked against the official sqlite3.h |
| `compat/compat-report.md` | 22375 | `4ff9144a4ed3eb71a14bec6a4ec26c394bbcf9d6d81c5ceaa6eef47d81d2b922` | the compatibility report |
| `compat/compat-report.json` | 122557 | `6d6b4e2f53ab1144c3c6f8126116cf525e060ca1960f5b9c39f3218f2da338b8` | the same, machine readable |
| `compat/sqlite-3.53.4.toml` | 117130 | `1c8c7fadb5c5c747cb831e3d376830ddc5b96b2a8a1686c824d3355236e35fcc` | the parity manifest the report is generated from |
| `compat/perf/contract.toml` | 2297 | `69b283d73d8ec7cc397239c44e57af595310bcb2e2ab8af81fc2104b9389300f` | the performance contract: weights, floors and the headline bound |
| `docs/reference-register.toml` | 3596 | `9fb2ff1d779daf90894a12f48a005be1867b8159938afd5fd33221d517f6c832` | every external project consulted, and in what capacity |
| `docs/invariants/layering.toml` | 12285 | `0430e44804bbb77eaeebf0e1f417cfe866909e9045da607c9f0a1d0876930bd6` | the dependency-direction contract |
| `compat/baseline/rustdb-core-baseline.json` | 4982 | `dc0d176de829cb9e0e2d28a02a3456445f694059494da7c566185b7a72d3a97a` | the retrieval engine's frozen baseline |
| `compat/baseline/rustdb-core-amendments.toml` | 835 | `92f51827b47a4beb252039cc412b0447ab2aa098e4f18de8bac1eea995e395fa` | every declared change to it, with its reason |
| `compat/release/migrate-release-corpus.db.migration-report.md` | 3089 | `9e3711e3ed4525435a617ecb6be8d36282a052ed5bfb0a3c81cbb62801fa24a1` | the migration report for an index of the repository's own prose, at release size |
| `compat/release/migrate-release-corpus.db.migration-manifest` | 3113 | `dea901e765848b69081f915a226f7e6023c43e2b82d52f9c8b1947666bc2d631` | that migration's append-only manifest, which is what a resume reads |
| `compat/release/migrate-full-corpus.db.migration-report.md` | 3048 | `2735b27b99db59374ed05fa4cf5884addcc961b7cbe022ad45cc86d1b8e5fd4a` | the migration report for the small corpus that has one of everything |
| `compat/release/scorecard.md` | 12166 | `987a8fa622b52da0e6a3025041ed7b314f729b25b6596dc78a40ae26a1ebca15` | the performance scorecard |
| `compat/release/arm-no-covering-index.md` | 4641 | `4c6da97cbc5c204f0df904611c8643834fe44ce3c4ce9196428a11536a6cfff5` | the same scorecard with the covering-index lever switched off |
| `compat/release/arm-no-indexed-write.md` | 4655 | `c12c458c13a166dffa98d26b6c9f2b2c7bbfc404abfe9243debeb2d8ae5cad4d` | the same scorecard with the indexed-write lever switched off |
| `compat/release/scorecard.json` | 20321 | `72b276b6cefc11861b8a03cd9e584122f385ad24534604f774a78d1b32055f7e` | the same, machine readable |
| `compat/release/history.jsonl` | 46699 | `f10ea1547a835f84f9688ecf54a86bbf267efb7b1cb3fb05e3b282a18a82f43e` | the raw performance history, one line per workload per run |
| `compat/release/dashboard.md` | 6087 | `6b4052a56e2662b3050e478c74f8fb82f20715170b95d4f3a4c8259cd4bd385f` | every workload's ratio across every recorded run |

## Reproducing this

A clean machine with a Rust toolchain and a C compiler reproduces every artifact above with these commands, in this order. Nothing needs another database engine installed: the reference is downloaded, checksum-verified against the sums SQLite publishes, and built from source into `.sqlite-ref/`, which no rust-db crate links against.

| artifact | command |
|---|---|
| the pinned reference | `tools/sqlite-reference.ps1   # or tools/sqlite-reference.sh on POSIX` |
| the engine and its tools | `cargo build --release --workspace` |
| the test evidence | `cargo run --release -p rustdb-compat --bin rustdb-evidence` |
| the compatibility report | `cargo run --release -p rustdb-compat --bin rustdb-manifest` |
| the performance scorecard | `cargo run --release -p rustdb-compat --bin rustdb-scorecard -- --scale all --rounds 30 --label <name>` |
| each optimization's A/B arm | `cargo run --release -p rustdb-compat --bin rustdb-scorecard -- --scale small --rounds 30 --label <name> --disable covering-index   # then --disable indexed-write` |
| the storage and write profiles | `cargo run --release -p rustdb-compat --bin rustdb-storageprofile && cargo run --release -p rustdb-compat --bin rustdb-writeprofile` |
| a legacy index migration | `cargo run --release -p rustdb-migrate -- <index-dir> <destination.db> --sqlite .sqlite-ref/3.53.4/shell/sqlite3` |
| this release candidate | `cargo run --release -p rustdb-compat --bin rustdb-release` |

## Upgrade and downgrade

The default writer produces the SQLite file format and nothing else, so an upgrade is a binary swap: the file a previous build wrote is the file this one opens, and the file this one writes is one the pinned SQLite opens. That is what the interoperability suites check in both directions, and what the migration tool's own probe checks on the database it just built.

A downgrade is the same swap in reverse, with one condition: a database holding a `rustdb_search` table is read by any build - the index lives in ordinary tables - but it is *queried* only by a build that has the module. An older build opens the file, reads every relational table, and reports `no such module: rustdb_search` for the search table alone.

Legacy retrieval indexes migrate with `rustdb-migrate`, which never writes to the source. Going back is not an undo; it is pointing the application at the directory that never changed.

## Known limitations

- **Performance.** The engine is slower than the pinned reference on every family except point reads by rowid, which it wins. The measured cause is the virtual machine rather than the storage layer: one step of a table scan costs 3.7 ns, reading the row 21.7, finding its fields 35.7 and decoding an integer 38.2 - while the machine on top of that costs about two hundred nanoseconds per column read and comparison. Closing it is opcode-level work: borrowed values through the whole register file, fused superinstructions, and specialised scan loops.
- **Seven optional SQLite surfaces are not implemented**, and the manifest carries a row for each so the denominator says so: the session extension, the pre-update hook, the snapshot API, `unlock_notify`, RBU, geopoly, and the R-Tree geometry callbacks. None of them is reachable from SQL or from the file format, so a database written by this engine is not affected by their absence - an application that calls them is.
- **What the levers that did land are worth**, measured rather than asserted, at the small scale over thirty paired rounds: with everything on the weighted geometric mean is 0.205x, and without the covering-index lever it is 0.170x, and without the indexed-write lever it is 0.138x. Both arms are in this candidate, and the correctness shard that runs under each of them shows the plans change and the answers do not.
- **FTS5's segment format inside `%_data` is first-party.** SQLite's is described only in comments in `fts5_index.c` and is explicitly not a published format, unlike the R-Tree's. What is matched is everything a reader outside the module sees: the five table names, the layouts of `%_content`, `%_docsize` and `%_config`, the rows `MATCH` finds, their order, and `bm25()` to the last digit.
- **A database opened through a caller-supplied C VFS journals rather than using a write-ahead log.** `xShmMap` is the easiest part of the VFS contract to get subtly wrong, and a log over a broken one corrupts silently.
- **A `rustdb_search` table has one row per document**, so the per-document cap in the fusion never binds on it. An application that wants documents made of several chunks models them in SQL - a document table and a join - which is what the migration tool writes and what its own grouped check verifies.
- **The legacy engine's lexical ranking depends on the `k` it was asked for.** Position-aware rescoring reaches `k * lexical_rescore_depth` hits and only ever scales a score down, so a chunk just outside that window keeps its full BM25 score and competes against rescored ones: ask for ten and ask for fifty, and the tail of the ranking moves. Every path a search table sits behind goes through `search_branches`, which retrieves at `candidates` depth, so the two agree when asked at that depth and can differ when they are not. The migration compares them at one depth for exactly that reason. A corpus of real prose is what exposed it: a small one has fewer chunks than the window, so the window never binds.
- **A migrated vector index is not the same graph.** The legacy index's graph grew one insert at a time; a migrated one is built in a single pass over every row, which is better connected - that is what makes compaction worth its cost. Two different graphs searched approximately give slightly different answers, sometimes one better and sometimes the other, so the migration compares them with the approximation switched off and reports separately what each finds of the exact answer at its default width. An application that depends on a particular ranking of near-ties should expect it to move.
- **A tombstone and a delete differ.** The legacy engine keeps a tombstoned chunk in the inverted index and filters it at query time, so its corpus statistics do not move; a search table deletes the row, so they do. Both make the document unreachable immediately; deep orderings can differ until the legacy index is rebuilt.
- **A migration is refused when the source was built with a non-zero heading boost.** A migrated search table indexes a chunk's text without its heading structure, which contributes nothing at the measured default of zero and would contribute at anything else - and the migrated index would then rank differently in a way nothing about it looked wrong.
