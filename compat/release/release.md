# inillucent release candidate

> **These are the `release` gate's own output files, taken on the engine that came
> before the rearchitecture, and they are kept as that run's record. They are not the current
> numbers and they are not edited by hand.** The engine measured here was slower than SQLite on
> every family; the shipping engine is **326% faster** weighted over the same ten families, with
> no family below the contract's 1.00x floor. The current run is
> [docs/performance.md](../../docs/performance.md) and
> [docs/feature-comparison.md](../../docs/feature-comparison.md).

**This candidate does not pass.** The gates it fails are marked below, with the numbers they were judged against. Nothing here argues that a number is acceptable: the bars were written down before the runs.

## Gates

| gate | verdict | detail |
|---|---|---|
| `compatibility` | pass | 264 capabilities pass, 7 not implemented, 0 unsupported claims |
| `performance.headline.small` | **FAIL** | weighted geometric mean 0.316x, lower bound 0.313x against a bound of 1.50x |
| `performance.floors.small` | **FAIL** | below the 0.90x floor: open.prepare at 0.151x, read.point at 0.764x, read.range at 0.249x, read.analytical at 0.108x, read.join at 0.234x, write at 0.236x, transaction at 0.386x, schema at 0.249x, extension at 0.108x, large.values at 0.780x |
| `performance.headline.medium` | **FAIL** | weighted geometric mean 0.232x, lower bound 0.229x against a bound of 1.50x |
| `performance.floors.medium` | **FAIL** | below the 0.90x floor: open.prepare at 0.150x, read.point at 0.558x, read.range at 0.223x, read.analytical at 0.046x, read.join at 0.154x, write at 0.156x, transaction at 0.309x, schema at 0.070x, extension at 0.106x, large.values at 0.697x |
| `performance.headline.large` | **FAIL** | weighted geometric mean 0.260x, lower bound 0.257x against a bound of 1.50x |
| `performance.floors.large` | **FAIL** | below the 0.90x floor: open.prepare at 0.145x, read.point at 0.697x, read.range at 0.217x, read.analytical at 0.044x, read.join at 0.142x, write at 0.274x, transaction at 0.265x, schema at 0.019x, extension at 0.168x |
| `performance.regressions` | pass | no workload has been slower than its best for two consecutive runs |
| `artifacts` | pass | 24 artifacts present and digested |

## Supported platforms

| platform | evidence |
|---|---|
| `windows-x86_64` (this machine) | compatibility evidence and a performance scorecard |
| `linux-x86_64` | compatibility evidence only |

## Artifacts

| file | bytes | sha256 | what it is |
|---|---:|---|---|
| `target/release/inillucent-shell.exe` | 4616192 | `a3131fceec1713e8264e12ea05fa37d0006f686f10a6d87df220c7c29c141f82` | the SQLite-like shell |
| `target/release/inillucent-migrate.exe` | 4819456 | `d299e13f6b8a514a3b7edb4c575a12de000ea246f54ca2277374e28910dea41c` | the resumable copy-and-verify migration tool |
| `target/release/inillucent_capi.dll` | 5191168 | `aced63169eec0b21982611fc62001df1230b8ca3602b0877e4a95586bbcb1a13` | the C library, linked against the official sqlite3.h |
| `compat/compat-report.md` | 22376 | `370ce58ef099525d672f08f9e61bc8f77ac42d5bdb979417afe2858e4a2af1a8` | the compatibility report |
| `compat/compat-report.json` | 123900 | `b3e0bb18fb2fd85bddd823fd6ddb475be55053498d2b544b194848805a9cbade` | the same, machine readable |
| `compat/sqlite-3.53.4.toml` | 118547 | `e29133cdfae4b172740a9902a12a34f0b6e66b0f419cce6e62df505e9ece0f92` | the parity manifest the report is generated from |
| `compat/perf/contract.toml` | 2297 | `69b283d73d8ec7cc397239c44e57af595310bcb2e2ab8af81fc2104b9389300f` | the performance contract: weights, floors and the headline bound |
| `docs/reference-register.toml` | 3596 | `9fb2ff1d779daf90894a12f48a005be1867b8159938afd5fd33221d517f6c832` | every external project consulted, and in what capacity |
| `docs/invariants/layering.toml` | 12285 | `0430e44804bbb77eaeebf0e1f417cfe866909e9045da607c9f0a1d0876930bd6` | the dependency-direction contract |
| `compat/baseline/inillucent-core-baseline.json` | 4982 | `e133fb06cd28d8108a7e91061016d2fa2baecd881b6bb98b5c286756755ddf2c` | the retrieval engine's frozen baseline |
| `compat/baseline/inillucent-core-amendments.toml` | 835 | `92f51827b47a4beb252039cc412b0447ab2aa098e4f18de8bac1eea995e395fa` | every declared change to it, with its reason |
| `compat/release/migrate-release-corpus.db.migration-report.md` | 3095 | `cff76ffb702cebb35e56c0450eff9a9ad7c2f8a7d73089eac509e57c5a4d224a` | the migration report for an index of the repository's own prose, at release size |
| `compat/release/migrate-release-corpus.db.migration-manifest` | 3122 | `11bfa5ed5073ed88998033218ae2d36f00a799c634647646eb0a5b1096f5449f` | that migration's append-only manifest, which is what a resume reads |
| `compat/release/migrate-full-corpus.db.migration-report.md` | 3054 | `678da01cdcbc77b438d7da6144647ebea1b0a6ab067510bfff3a29eafc123a4c` | the migration report for the small corpus that has one of everything |
| `compat/release/scorecard.md` | 12173 | `96297b09fe12a48e5c58148ee7813765bf8ef8b078c53c86082afc0dc13be9fd` | the performance scorecard |
| `compat/release/arm-no-covering-index.md` | 4653 | `9e45285a9950d1304549d6b48dba10377af01ba7bf24549ae888199e9656108a` | the same scorecard with the covering-index lever switched off |
| `compat/release/arm-no-indexed-write.md` | 4653 | `cded2d67f48e8554d78e5bad2480eb5eefb5587ada9fae330eab1f8d3a864ebf` | the same scorecard with the indexed-write lever switched off |
| `compat/release/arm-no-ordered-walk.md` | 4647 | `0996462520d6d1391de5cff10ead4ea7d0f4e5858b7367c5f62e45ba91a3777b` | the same scorecard with the ordered-walk lever switched off |
| `compat/release/arm-no-streaming-group.md` | 4655 | `10b547fc9fcd239a9dbb5a7a78cfd5a6094d3022d8c00902bb52ba195759825a` | the same scorecard with the streaming-group lever switched off |
| `compat/release/arm-no-fused-bytecode.md` | 4652 | `addfe46ff42c89fa1a0a90b1927fc30cae828c73124fffb80adf7f00973511b7` | the same scorecard with the fused-bytecode lever switched off |
| `compat/release/checkpoint-checkpoint.md` | 1688 | `46b7b3eac07f3a1391becedf76a593b37117727732dfe528034ba9913b9f638a` | the checkpoint-scheduling lever, measured against its own arm and left off |
| `compat/release/scorecard.json` | 20292 | `a36d1135df3e5ddf217273b5f380a25d51ae6cf8e5343f15788cffc260be4698` | the same, machine readable |
| `compat/release/history.jsonl` | 26363 | `383f9e59f3b776494f01e3513fd480357c03626bdd9a9c09cae49327c7e14072` | the raw performance history, one line per workload per run |
| `compat/release/dashboard.md` | 4426 | `859aa1d3a8e114830c5aef7f71ff067b32cb14abb0074fbb5eac3a965dd6413d` | every workload's ratio across every recorded run |

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
