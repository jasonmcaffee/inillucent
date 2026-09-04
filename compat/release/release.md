# rust-db release candidate

**This candidate does not pass.** The gates it fails are marked below, with the numbers they were judged against. Nothing here argues that a number is acceptable: the bars were written down before the runs.

## Gates

| gate | verdict | detail |
|---|---|---|
| `compatibility` | pass | 264 capabilities pass, 7 not implemented, 0 unsupported claims |
| `performance.headline.small` | **FAIL** | weighted geometric mean 0.256x, lower bound 0.247x against a bound of 1.50x |
| `performance.floors.small` | **FAIL** | below the 0.90x floor: open.prepare at 0.133x, read.point at 0.784x, read.range at 0.194x, read.analytical at 0.060x, read.join at 0.216x, write at 0.180x, transaction at 0.217x, schema at 0.193x, extension at 0.082x, large.values at 0.758x |
| `performance.headline.medium` | **FAIL** | weighted geometric mean 0.198x, lower bound 0.194x against a bound of 1.50x |
| `performance.floors.medium` | **FAIL** | below the 0.90x floor: open.prepare at 0.129x, read.point at 0.578x, read.range at 0.188x, read.analytical at 0.026x, read.join at 0.126x, write at 0.142x, transaction at 0.235x, schema at 0.068x, extension at 0.086x, large.values at 0.731x |
| `performance.headline.large` | **FAIL** | weighted geometric mean 0.219x, lower bound 0.203x against a bound of 1.50x |
| `performance.floors.large` | **FAIL** | below the 0.90x floor: open.prepare at 0.132x, read.point at 0.729x, read.range at 0.194x, read.analytical at 0.027x, read.join at 0.124x, write at 0.210x, transaction at 0.163x, schema at 0.017x, extension at 0.131x |
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
| `target/release/rustdb-shell.exe` | 4888064 | `8ae7f8f08045c4562ac5e94261067c1b1a49a1aec9a27383a09560927ce6f8c1` | the SQLite-like shell |
| `target/release/rustdb-migrate.exe` | 5091840 | `262d36152c46bf2e92a4be6eed4dfb32a6e11b64912101d43971d1c9cc234e0d` | the resumable copy-and-verify migration tool |
| `target/release/rustdb_capi.dll` | 5410304 | `479c4aad55bf3d1a3876dec5fc1b7f528996c65ad6804d40817158c704eafa03` | the C library, linked against the official sqlite3.h |
| `compat/compat-report.md` | 22376 | `370ce58ef099525d672f08f9e61bc8f77ac42d5bdb979417afe2858e4a2af1a8` | the compatibility report |
| `compat/compat-report.json` | 123900 | `b3e0bb18fb2fd85bddd823fd6ddb475be55053498d2b544b194848805a9cbade` | the same, machine readable |
| `compat/sqlite-3.53.4.toml` | 118547 | `e29133cdfae4b172740a9902a12a34f0b6e66b0f419cce6e62df505e9ece0f92` | the parity manifest the report is generated from |
| `compat/perf/contract.toml` | 2297 | `69b283d73d8ec7cc397239c44e57af595310bcb2e2ab8af81fc2104b9389300f` | the performance contract: weights, floors and the headline bound |
| `docs/reference-register.toml` | 3596 | `9fb2ff1d779daf90894a12f48a005be1867b8159938afd5fd33221d517f6c832` | every external project consulted, and in what capacity |
| `docs/invariants/layering.toml` | 12285 | `0430e44804bbb77eaeebf0e1f417cfe866909e9045da607c9f0a1d0876930bd6` | the dependency-direction contract |
| `compat/baseline/rustdb-core-baseline.json` | 4982 | `dc0d176de829cb9e0e2d28a02a3456445f694059494da7c566185b7a72d3a97a` | the retrieval engine's frozen baseline |
| `compat/baseline/rustdb-core-amendments.toml` | 835 | `92f51827b47a4beb252039cc412b0447ab2aa098e4f18de8bac1eea995e395fa` | every declared change to it, with its reason |
| `compat/release/migrate-release-corpus.db.migration-report.md` | 3103 | `1d0e48091514bbe9ff4e1b64fe9c4db043f737fbf98bd7a16e75b78220f56764` | the migration report for an index of the repository's own prose, at release size |
| `compat/release/migrate-release-corpus.db.migration-manifest` | 3155 | `7e1a78d794acc18404f1ba640e84f846943571e250e325ef9738b071c1eb868f` | that migration's append-only manifest, which is what a resume reads |
| `compat/release/migrate-full-corpus.db.migration-report.md` | 3060 | `447146d88a9d1242115097af327a5a83cd280bf0b87547d67099ffd84e258cb1` | the migration report for the small corpus that has one of everything |
| `compat/release/scorecard.md` | 12195 | `1f4cef9d70b697fa279c9f288feced043ec4d29c492b77b063b791cd371bcfec` | the performance scorecard |
| `compat/release/arm-no-covering-index.md` | 4646 | `64e9a863141d9d12168ba49a2abae66ab3731b3ec4c2a5213b4fd35ab0cd5111` | the same scorecard with the covering-index lever switched off |
| `compat/release/arm-no-indexed-write.md` | 4648 | `01eea8decff16fde9333fc4cb9679136d4a4da243bf0709139260707c010fb51` | the same scorecard with the indexed-write lever switched off |
| `compat/release/arm-no-ordered-walk.md` | 4652 | `bc65d617bf9bc38e4e61c0178cf4e216a3102c150dc821a5d0f34c91f8a8fada` | the same scorecard with the ordered-walk lever switched off |
| `compat/release/arm-no-streaming-group.md` | 4657 | `5f9fe0ae45c228fb3a7a8fc41e91e09874afb60bf76b5e1898a1df092c885215` | the same scorecard with the streaming-group lever switched off |
| `compat/release/arm-no-fused-bytecode.md` | 4653 | `c063872a1cdf3f13b48b987216c693b0188f7ccc063a0e2947617c30e91c27b8` | the same scorecard with the fused-bytecode lever switched off |
| `compat/release/checkpoint-checkpoint.md` | 1688 | `0cbb73648de86c6d6bc9fe2eb001b2daf023cfc88e18ca968652ab06f5ae164f` | the checkpoint-scheduling lever, measured against its own arm and left off |
| `compat/release/scorecard.json` | 20317 | `ac9cc992c877749e6a4433c870dd65f186501a5ef84dc2c0d660b29397514b1f` | the same, machine readable |
| `compat/release/history.jsonl` | 77039 | `ea4c12adfe901490fecc980e60d00ad04142410f41f89ab77d2d7325fa90d7b7` | the raw performance history, one line per workload per run |
| `compat/release/dashboard.md` | 8580 | `76787588b940ff08630808154078c95fb71886bd0a57fe6ea29f558d97daf007` | every workload's ratio across every recorded run |

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
- **What the levers that did land are worth**, measured rather than asserted, at the small scale over thirty paired rounds: with everything on the weighted geometric mean is 0.256x; without the covering-index lever 0.208x; without the indexed-write lever 0.166x; without the ordered-walk lever 0.213x; without the streaming-group lever 0.239x; without the fused-bytecode lever 0.244x. Every arm is in this candidate, and the correctness shard that runs under each of them shows the plans change and the answers do not.
- **Two of the TDD's optimisation levers were implemented, measured, and left off.** Checkpoint scheduling - bounding how many frames one automatic checkpoint copies, so the cost is spread over the commits that caused it - makes no difference, and its own counters say why: the checkpoint already runs after nearly every commit once the log passes its threshold, about 5,700 times in 6,000, so there is no accumulated batch to spread. The bound is a tunable rather than a default and the shipped behaviour is unchanged. Group commit has nothing to group: writers are serialised, so transactions do not overlap, and a commit already takes exactly the barriers the reference takes - one sync in a write-ahead log at `synchronous=full`, none at `normal`, two in a rollback journal. The evidence is that the two families where the barrier dominates are at parity: `write.insert.autocommit` at 0.99x and `txn.autocommit` at 0.92x. Sharing one barrier between two writers would need overlapping write transactions, which is a change to the locking rather than a tuning lever.
- **Vectorisation and SIMD are not applicable to this execution model.** The lever's name pairs them with bytecode fusion, which is implemented; the other two need a columnar or batched interpreter, where one instruction works on many rows. This is a row-at-a-time virtual machine, so there is no vector for an instruction to act on, and saying so is more use than a benchmark of nothing.
- **FTS5's segment format inside `%_data` is first-party.** SQLite's is described only in comments in `fts5_index.c` and is explicitly not a published format, unlike the R-Tree's. What is matched is everything a reader outside the module sees: the five table names, the layouts of `%_content`, `%_docsize` and `%_config`, the rows `MATCH` finds, their order, and `bm25()` to the last digit.
- **A database opened through a caller-supplied C VFS journals rather than using a write-ahead log.** `xShmMap` is the easiest part of the VFS contract to get subtly wrong, and a log over a broken one corrupts silently.
- **A `rustdb_search` table has one row per document**, so the per-document cap in the fusion never binds on it. An application that wants documents made of several chunks models them in SQL - a document table and a join - which is what the migration tool writes and what its own grouped check verifies.
- **The legacy engine's lexical ranking depends on the `k` it was asked for.** Position-aware rescoring reaches `k * lexical_rescore_depth` hits and only ever scales a score down, so a chunk just outside that window keeps its full BM25 score and competes against rescored ones: ask for ten and ask for fifty, and the tail of the ranking moves. Every path a search table sits behind goes through `search_branches`, which retrieves at `candidates` depth, so the two agree when asked at that depth and can differ when they are not. The migration compares them at one depth for exactly that reason. A corpus of real prose is what exposed it: a small one has fewer chunks than the window, so the window never binds.
- **A migrated vector index is not the same graph.** The legacy index's graph grew one insert at a time; a migrated one is built in a single pass over every row, which is better connected - that is what makes compaction worth its cost. Two different graphs searched approximately give slightly different answers, sometimes one better and sometimes the other, so the migration compares them with the approximation switched off and reports separately what each finds of the exact answer at its default width. An application that depends on a particular ranking of near-ties should expect it to move.
- **A tombstone and a delete differ.** The legacy engine keeps a tombstoned chunk in the inverted index and filters it at query time, so its corpus statistics do not move; a search table deletes the row, so they do. Both make the document unreachable immediately; deep orderings can differ until the legacy index is rebuilt.
- **A migration is refused when the source was built with a non-zero heading boost.** A migrated search table indexes a chunk's text without its heading structure, which contributes nothing at the measured default of zero and would contribute at anything else - and the migrated index would then rank differently in a way nothing about it looked wrong.
