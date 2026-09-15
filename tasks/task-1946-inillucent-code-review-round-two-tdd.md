# task-1946 - inillucent code review, round two: what to fix before the public release

**Reviewed at commit `b179c43e`**, the commit that cut 0.1.2 (task-1934). Every file and line
number below is at that revision. The review was done from a detached worktree of `origin/main`;
the nineteen uncommitted task-1913 files in the main checkout were not read and are not part of
this document.

## Introduction

This is the third review of the workspace. task-1892 found eleven defects and task-1894 fixed them.
task-1920 found forty one and task-1932 implemented every one across nine commits, validating the
result at 165 targets, 2,777 tests, 0 failed. This round starts from that validated commit and asks
a different question: is the repository ready to be public. It read every crate, the driver, the
four language packages, the packaging, the assurance layer, and the documentation, with a mechanical
sweep for dead code on top.

The answer is that the engine is in good shape and the repository around it is not yet. Crash risk
in the storage crates came back clean: no `unwrap`, `expect`, `panic` or slice indexing on a path
that reads a page, a frame, or a network byte, every `unsafe` block with a written argument, and
every round one finding confirmed closed by reading the code rather than the commit message. What
remains is three wrong behaviours in the engine, one wrong answer in the retrieval library API, a
handful of things that must not be in a public repository (a password, a personal email address, a
Google account story, two binary files named `--db`), version pins that disagree with the release,
and about two thousand lines of code that nothing calls.

The result is 10 high findings, 13 medium findings, and 5 decisions that are Jason's to make rather
than an implementer's.

## Goals and Non-Goals

**Goals.** The implementation ticket that follows this document is done when:

1. Every finding in section 4 is fixed and has the test named beside it, and each new test fails on
   `6e84068` and passes after the fix.
2. Every finding in section 5 is fixed, except the ones that name a file for Jason to delete; for
   those, the code that referenced the file is gone and the build no longer needs it.
3. The greps in section 12 return nothing: no personal email address, no credential, no machine
   path in a tracked file.
4. `pwsh tools/validate.ps1 --strict` is green on a clean checkout, and the count of tests and
   targets it prints is the count the documentation states.
5. The performance contract in `compat/perf/contract.toml` still passes. Two of the fixes (H1, H3)
   touch the binder and the planner's inputs; the gate is what says they cost nothing.

**Non-goals.**

- The performance bars in `docs/roadmap.md` items 1 through 6, threads (item 9), segmented
  generations (item 10), the macOS archive (item 11), and recovery reading a page before redo (item
  12, which round one's M3 traced and this round confirms is unchanged at
  `crates/inillucent-engine/src/recovery.rs:181-207`). All are tracked and owned.
- Deleting `inillucent-storage` and `inillucent-transaction`. They are still what
  `inillucent-sqlite-reader` and `inillucent-catalog` read a SQLite file through, and their removal
  is gated on `tasks/task-1816-rearchitecture-tdd.md` Phase 5. This round trims what is dead inside
  them (M1) and stops there.
- Splitting `bind.rs`. task-1913's uncommitted work in the main checkout already holds an untracked
  `crates/inillucent-sql/src/bind/` directory, so the split is in flight there. The implementation
  ticket must not start a second one (M12 says what it may do instead).
- Publishing to crates.io, npm, PyPI, Homebrew or Composer. The README says plainly that none of
  the five is published and each waits on an account; this round only asks that the claim stay
  true and that the manifests would not block it (M6).

## Problem statement

Five things are true of the workspace today that were not visible from the round one list.

**The engine has three behaviours a user will hit and a test does not.** `ANALYZE` writes its
statistics and the running connection never reads them, so every query until the next open plans
against a guessed row count of 1,048,576. `VACUUM` on a connection opened over a caller's `Vfs`
reopens the connection on the operating system's file system, so an application using an
encrypting or in memory VFS ends the statement on a different backend than it started on. A chain
of more than 32 distinct triggers is refused at bind time, while `.limit` and the manifest advertise
1000 and the executor carries an unreachable check against 1000.

**The retrieval library answers a wrong question silently.** `inillucent-core`'s search entry points
take a query slice and never compare its width to the index's. The dot product zips the two and
stops at the shorter, so a 4 wide query against an 8 wide index ranks every vector by its first four
components and returns a plausible list. The SQL boundary in `inillucent-search` checks this; the
library API that `docs/vector-search.md` documents does not.

**Tracked files carry things a public repository must not.** The two scorecard files at the root
record the command line that produced them, unredacted, and that command line holds a PostgreSQL
password and this machine's drive paths. `packaging/PUBLISHING.md` holds an npm account name, two
real email addresses, a live token settings URL and the story of the Google account that was
deleted during task-1898. `crates/inillucent-core/src/tokenize.rs` uses a real email address as a
tokenizer fixture eight times. Two binary files named `--db` and `--db-wal.0000000001` are tracked
under `crates/inillucent-compat`, the output of a smoke run whose flag was read as a path.

**The packages disagree about which release they are.** The workspace is 0.1.2. The Python package
reports `__version__ = "0.1.0"`; the PHP installer downloads 0.1.1. The Go installer and npm say
0.1.2. Nothing checks them against each other.

**About two thousand lines have no caller.** The mechanical sweep found 101 public items that appear
only at their own definition, six functions whose own doc comment says they are unreachable and
flagged for removal, a 724 line module in `inillucent-transaction` that is re-exported and never
named, a 767 line integrity checker with no production caller, and six of seven public functions in
`inillucent-storage/src/vacuum.rs` that duplicate the vacuum the engine actually runs. None of it
warns, because it is all `pub`.

## Architectural overview

```mermaid
flowchart LR
    subgraph surfaces [What an application touches]
        CLI[inillucent-cli\ncommand line, shell, MCP]
        DRV[drivers/inillucent-driver]
        CAPI[C ABI]
        PKG[packages: go, npm, php, python]
    end
    subgraph engine [The engine]
        ENG[inillucent-engine\nlib.rs 7,428 lines]
        EXEC[inillucent-exec]
        SQL[inillucent-sql\nbind.rs 5,315 lines]
        CAT[inillucent-catalog]
    end
    subgraph storage [Storage and durability]
        POOL[inillucent-pool]
        WAL[inillucent-wal]
        TREE[inillucent-tree]
        TXN[inillucent-txn]
        VFS[inillucent-vfs\nOsVfs, MemoryVfs]
        SIM[inillucent-sim\nSimVfs]
    end
    subgraph retired [Kept for reading SQLite files]
        STO[inillucent-storage]
        TRN[inillucent-transaction]
        RDR[inillucent-sqlite-reader]
    end
    subgraph retrieval [Retrieval]
        CORE[inillucent-core]
        SRCH[inillucent-search]
        BENCH[inillucent-bench\nwrites the scorecard]
    end
    PKG --> CAPI --> DRV --> ENG
    CLI --> DRV
    CLI -. 7 files still reach past the driver .-> ENG
    ENG --> EXEC --> SQL
    ENG --> CAT --> STO
    RDR --> STO --> TRN
    ENG --> TXN --> WAL --> POOL --> VFS
    SIM --> VFS
    ENG --> SRCH --> CORE
    BENCH --> CORE
    classDef bad fill:#f8d7da,stroke:#b02a37,color:#000
    class ENG,SQL,CORE,BENCH,STO,TRN bad
```

The shaded boxes are where the section 4 and section 5 findings live. `inillucent-engine` holds H1
and H2, `inillucent-sql` holds H3, `inillucent-core` holds H4 and the skip sites of H10,
`inillucent-bench` writes the scorecard of H6, and the two retired crates hold most of M1.

## 4. High findings

Each is a wrong answer, a credential or personal reference in tracked content, or a claim a user
will act on that is false. Each names the test that proves the fix.

### H1. `ANALYZE` writes statistics the running connection never reads

`crates/inillucent-engine/src/analyze.rs:39-89`. The `analyze` directive ends with
`clear_stat1`, `write_stat1` and `seal`. It never calls `refresh_catalog`, and `refresh_catalog` is
the only path to `republish_statistics` (`analyze.rs:386`). Every other schema writing directive
in `ddl.rs`, `attach.rs`, `vtab.rs` and `marks.rs` ends with `self.refresh_catalog()`. The doc
comment on `refresh_catalog` at `ddl.rs:539-548` describes this exact consequence: a snapshot taken
before the new rows were read describes tables whose row counts are still the planner's guesses.

Result: after `ANALYZE`, `IndexInfo::prefix_rows` is `Some([])` and `TableInfo` rows stay at
`DEFAULT_ROWS`, 1,048,576 (`crates/inillucent-sql/src/cost.rs:21`), until the database is reopened.
The suite never sees it because every case in `crates/inillucent-compat/tests/analyze_reopen.rs`
reopens before it reads anything back. task-1932's closing comment reported this and said it
deserved a ticket; this is that ticket.

**Fix.** Call `self.refresh_catalog()` in `analyze` after `write_stat1`, before `seal`, the way
every other directive does. Confirmed by reading the full path.

**Test.** `analyze_same_session.rs`: create a table with 20 rows and an index, `ANALYZE` on the
same connection, and assert `EXPLAIN QUERY PLAN` (or the binder's `analysed_rows`) reports 20 and
not 1,048,576, with no reopen. Fails on `6e84068`.

### H2. `VACUUM` reopens the connection on `OsVfs`, whatever VFS it was opened on

`crates/inillucent-engine/src/lib.rs:3167-3226`, `vacuum_in_place`. It rebuilds into a scratch
file, then does `*self = ImportedDatabase::open(scratch, ...)` at line 3207 and
`*self = ImportedDatabase::open(path, ...)` at line 3220. `ImportedDatabase::open` (line 1699) and
`create` (line 1634) construct `Arc::new(OsVfs::new())`. The same file uses
`Arc::clone(&self.vfs)` correctly at lines 3488, 3563 and 4266. `crates/inillucent-engine/src/rebuild.rs:256-333`
uses `std::fs::rename`, `read_dir` and `remove_file` directly.

`docs/relational-architecture.md` section 6 documents the rename as a deliberate bypass of the
`Vfs`, because a rename is a single directory update a crash cannot catch halfway and the trait has
no rename. What the document does not say is that the reopen swaps the whole connection: an
application on `MemoryVfs`, `SimVfs` or its own encrypting VFS that runs `VACUUM` or
`PRAGMA incremental_vacuum` either gets `Open: The system cannot find the path specified`, or, if a
real file happens to exist at that path string, silently continues on the operating system's file
system for the rest of the session. task-1932's closing comment reported the first half and called
the fix a design change. It is, and it is small enough to make.

**Fix.**
1. Add `rename` to the `Vfs` trait at `crates/inillucent-vfs/src/contract.rs:330-357`, documented
   as an atomic replace of the target when the target exists.
2. Implement it on the three implementors: `OsVfs` (`crates/inillucent-vfs/src/os/mod.rs`) with
   `std::fs::rename`; `MemoryVfs` (`crates/inillucent-vfs/src/memory.rs`) by moving the inode map
   entry; `SimVfs` (`crates/inillucent-sim/src/sim_vfs.rs`) delegating with a fault site, so the
   crash campaign can cut inside a rename.
3. Route `rebuild.rs`'s `commit_rebuild` and `remove_log_segments` through `self.vfs`, using
   `delete` for the segments and a directory listing derived from the segment naming rule rather
   than `read_dir` (the segment names are deterministic from the base path, so no listing method is
   needed on the trait).
4. Replace both `ImportedDatabase::open(...)` calls in `vacuum_in_place` with
   `open_on(Arc::clone(&self.vfs), ...)`.
5. Correct `docs/relational-architecture.md` section 6.

Confirmed by reading the full path.

**Test.** `vacuum_on_vfs.rs`: open on `MemoryVfs`, insert, delete, `VACUUM`, assert no file exists
on disk at the path string, assert a later `SELECT` still answers, and assert the connection's VFS
is still the `MemoryVfs` instance. Then move `vacuum_crash.rs` off real files onto `SimVfs` and add
one cut inside `rename`. Fails on `6e84068`.

### H3. Trigger depth: 32 is enforced, 1000 is advertised, and the executor's check is unreachable

Two constants share one name. `crates/inillucent-sql/src/bind.rs:1498` declares
`MAX_TRIGGER_DEPTH = 32`, and `crates/inillucent-sql/src/dml.rs:707` refuses at bind time with
`too many levels of trigger recursion`, no number stated. `crates/inillucent-exec/src/trigger.rs:73`
declares `MAX_TRIGGER_DEPTH = 1000`, and `trigger.rs:114` checks it at run time; but the executor
walks a tree the binder already capped at 32, so that check can never fire.
`Limit::TriggerDepth` is read only at `crates/inillucent-cli/src/diagnose.rs:249`, for display, and
`crates/inillucent-base/manifests/limits.toml:112-125` advertises a default of 1000 with a comment
saying the oracle wins.

A self referencing trigger is skipped (`dml.rs:704`, `firing.contains`), so only chains of distinct
triggers are affected. Forty tables `t0` to `t39`, each with `AFTER INSERT ON t{i}` inserting into
`t{i+1}`, then `INSERT INTO t0 VALUES (1)`: the oracle succeeds, inillucent refuses.

**Fix.** The binder's comment says the cap has to live where the inlining happens, and that is
right; what is wrong is the number and the second constant. Make the binder read the configured
`Limit::TriggerDepth` (default 1000, settable through `.limit` and the driver), put the number in
the message, and delete `trigger.rs:73` and the `Depth::deeper` check at `trigger.rs:112-119`
along with the comment at `trigger.rs:373` that cites it. If an inlining cost at depth 1000 turns
out to be a problem (it is a bind time tree, and the cap on distinct triggers is the schema's own
trigger count), the fallback is to set the advertised default to 32 in `limits.toml` and record
the difference from the oracle in `docs/feature-comparison.md`; do not leave two numbers.
Confirmed by reading the full path.

**Test.** A differential case in `dml_differential.rs` or a new `trigger_depth.rs` building the
forty table chain against the oracle; and one asserting that `.limit trigger_depth 10` then an
eleven deep chain is refused with a message naming 10. Fails on `6e84068`.

### H4. `inillucent-core` search silently truncates a query of the wrong width and accepts NaN

`crates/inillucent-core/src/distance.rs:16-52`: `dot` is `a.chunks_exact(4).zip(b.chunks_exact(4))`
under a `debug_assert_eq!` on the lengths. In a release build a length mismatch stops at the
shorter side. None of `exhaustive_search`, `vector_search`, `search_branches` or the hybrid entry
points (`index.rs:865-985`, `flat.rs:42-121`, `hnsw.rs:961-1010`) compares `query.len()` to the
index width or rejects a non finite component. A NaN passes through `rank.rs`'s
`absolute_confidence` `clamp` unchanged. The SQL boundary is guarded (`inillucent-search/src/store.rs:1027-1060`,
`vector_of`, round one's M4), so a user of the virtual table is safe; a user of the library API
that `docs/vector-search.md` documents gets a ranked list computed over the first `k` components.

**Fix.** One validation at the public entry points of `inillucent-core` (a `check_query(query,
dims) -> Result` called by each), refusing a width mismatch and any non finite component with the
same error `vector_of` gives. Leave `dot` as it is; it is the hot loop and the check belongs at the
boundary, once. Confirmed by reading the full path.

**Test.** In `crates/inillucent-core`'s own tests: a 4 wide and a 12 wide query against an 8
wide index each return an error, and a query with one NaN component returns an error. Today all
three return a `Vec<Neighbour>`.

### H5. `ConnectionUrl` derives `Debug` over the raw password

`crates/inillucent-remote/src/url.rs:46`: `#[derive(Clone, Debug, PartialEq, Eq)]` on a struct whose
field at line 57 is `pub password: Option<String>`. The module's invariant at line 8 says the
password is never formatted and `Display` redacts it, which is true and tested
(`display_redacts_the_password`, `url.rs:524`). `Debug` prints it. No `{:?}` on this type was found
today, so this is latent: one `dbg!`, one `#[derive(Debug)]` on a struct that holds a
`ConnectionUrl`, or one error type wrapping it, and a password is in a log. `mysql.rs:134` and
`postgres.rs:138` repeat the claim on the field that holds the parsed URL.

**Fix.** Replace the derive with a hand written `impl Debug` that prints every field and `***` for
the password. Confirmed by reading the full path.

**Test.** `debug_redacts_the_password` beside the `Display` test, asserting `format!("{url:?}")`
does not contain the password. Fails on `6e84068`.

### H6. The scorecard records an unredacted command line with a password and this machine's paths

`crates/inillucent-bench/src/runs.rs:219-221`: `command_line()` joins `std::env::args()` and
returns it. The sibling field, the baseline database URL, goes through `redact` (`runs.rs:206-216`),
which replaces the password with `***`. The command line does not, and `--database-url` is an
argument. So the tracked `inillucent-scorecard.md:220` and `inillucent-scorecard.json` (one hit)
carry `postgres://postgres:inillucent@127.0.0.1:5433/inillucent_synth` in clear, beside the
redacted copy, and the same table (`scorecard.md:220-224`) carries `<machine path>\inillucent\...` and
`J:/inillucent-embeddings/...`.

The password is for a local synthetic corpus database and has been in the history since the
scorecard was first committed, so its exposure is already what it is; the fix is for the next
regeneration, and a person changes that password.

**Fix.** `command_line()` passes every token through `redact` (a token that parses as a URL with
credentials is redacted; every other token is kept), and strips the machine specific paths by
recording the program name without its directory and each path argument as its file name only.
Regenerate both scorecard files from the same run data so they carry the redacted command.
Confirmed by reading the full path.

**Test.** `command_line_redacts_credentials_and_paths` in `inillucent-bench`: a synthetic argv with
a `postgres://u:p@h/d` token and an absolute path produces neither `p` nor the directory.

### H7. Personal addresses, an account name and an incident story in tracked content

Three places, all confirmed by grep at `6e84068`:

- `packaging/PUBLISHING.md:264,341-393,560-582`: the npm username, `<account address>`,
  `the.black.rainbow.labs@gmail.com`, the account deletion story from task-1898, a live
  `npmjs.com/settings/<user>/tokens` URL, and references to `~/.claude/CLAUDE.md`.
- `crates/inillucent-core/src/tokenize.rs`: `<account address>` eight times and
  `gordon@example.invalid` at lines 184, 231, 232, 437, 441, 519, 520 as tokenizer fixtures. The same
  file already uses `jason@example.com` elsewhere.
- `crates/inillucent-remote/src/lib.rs:8` and `url.rs:5`: `postgres://jason@127.0.0.1:5432/corpus`
  as the doc example, where every other example in the crate uses `user`.

**Fix.** Rewrite `PUBLISHING.md` to what a contributor needs (which registries exist, how an
archive is built and verified, what a maintainer with the accounts does), with no account names,
addresses or incident narrative. Replace every fixture address with an `example.com` one, keeping
the byte layout the test depends on (the `example-company` fixture exists because its domain holds a
digit; `gordon@example-company.example` keeps that property). Replace the two doc examples with `user`.

**Test.** The section 12 greps, run by `tools/doc-facts/check.mjs` as a new assertion so they stay
empty.

### H8. Two binary files named `--db` are tracked under `crates/inillucent-compat`

`crates/inillucent-compat/--db` (131,072 bytes, magic `RDB2`) and
`crates/inillucent-compat/--db-wal.0000000001` (64 bytes), committed in `c9f82ea` with task-1932's
phase A. A smoke command read its own `--db` flag as the path. `.gitignore`'s comment says no
`.rdb` has ever been tracked except the documented example, and it is right about `.rdb`; these two
have no extension, which is why the ignore rule missed them.

**Fix, for Jason.** An agent does not delete files it did not create, so this is section 6's
first item. The implementation ticket adds two `.gitignore` rules, `--db` and `--db-wal.*`, at
the repository root, and a policy test that no tracked file has a name beginning with `--`.

### H9. The Python package reports 0.1.0 and the PHP installer pins 0.1.1

`packages/python/src/inillucent/__init__.py:50`: `__version__ = "0.1.0"`, against
`packages/python/pyproject.toml:7` at 0.1.2. `packages/php/bin/inillucent-install:72`:
`const NATIVE_VERSION = '0.1.1'`, so `composer require` followed by the installer fetches the
release that cannot embed. `packages/go/cmd/inillucent-install/main.go:56` and the npm package are
at 0.1.2. task-1934 updated the wrapper versions and missed these two because nothing ties them
together.

**Fix.** Set both to 0.1.2. Then add one check to `tools/doc-facts/check.mjs` (or a policy test)
that reads `[workspace.package] version` and asserts it against every pinned copy: `pyproject.toml`,
`__init__.py`, `package.json` and its five platform packages, `main.go`, `inillucent-install`, and
the Homebrew formula. The release script `packaging/release.ps1` runs that check before it builds.
Confirmed by reading.

### H10. Five retrieval tests report green with no message when no model is installed, and the two policy checks built to catch that do not look in `src/`

`crates/inillucent-core/src/residency.rs:540-570`: `managed_or_skip` returns `None` when
`model_dir` finds nothing, and its five `#[cfg(feature = "onnx")]` tests return silently.
`crates/inillucent-bench/src/models.rs:341` prints its skip as `skipping: ...`, marker first, where
the house marker is the suffix `; skipping` that `--strict` counts. Both slip past the policy suite:
`every_skip_site_carries_the_one_marker` does not treat `return None;` as a skip, and
`every_early_return_in_a_test_says_why` (`crates/inillucent-compat/tests/policy.rs:1118`) scans only
paths with a `tests` component, so inline `#[cfg(test)] mod tests` blocks in `src/` are never read.
This is the class of defect round one's H10 was about, one layer down.

**Fix.** Both concrete sites print the `; skipping` suffix with the reason. Widen
`every_early_return_in_a_test_says_why` to inline test modules under `src/`, and teach the marker
detector that a bare `return None;` or `return;` inside a test is an early return that must say
why. Confirmed by reading.

**Test.** The widened policy test itself, which fails on `6e84068` at the two sites; and
`cargo test -p inillucent-core --features onnx` on a machine with no model prints five skip lines.

## 5. Medium findings

### M1. Dead code in the two retired crates, about 2,300 lines

All confirmed callerless by grep across every tracked file, and the largest read in full.

| file | what | lines |
|---|---|---|
| `crates/inillucent-transaction/src/state.rs` | the whole module: `Transaction`, `TransactionState`, `Savepoint`, `BeginMode`, `ConflictAlgorithm`, `ChangeCounters`, `TransactionStats`, re-exported at `lib.rs:45,54-57` and named nowhere else; `inillucent-engine` defines its own types of the same names over `inillucent-txn` | 724 |
| `crates/inillucent-storage/src/check.rs` | the integrity checker; every caller is a test or a benchmark binary in `inillucent-compat` | 767 |
| `crates/inillucent-storage/src/vacuum.rs` | six of seven public functions (`final_size`, `incremental_step`, `incremental_vacuum`, `auto_vacuum_commit`, `copy_tree`, `copy_database`); only `relocate_page` is called, from `mutate.rs`. The shipping `VACUUM` is `inillucent-engine/src/rebuild.rs` | about 600 of 657 |
| `crates/inillucent-storage/src/overflow.rs:288,312` | `read_range` and `write_range`, an incremental blob API with no caller | 37 |
| `crates/inillucent-storage/src/pager.rs:383-1819` | thirteen introspection functions (`has_wal`, `wal_stats`, `wal_frame_count`, `close_wal`, `detach_wal`, `is_read_only`, `dirty_pages`, `undo_depth` and five more) | about 150 |
| `crates/inillucent-storage/src/databases.rs` | `MAIN_DATABASE` and the `PagerSet` trait; keep `TEMP_DATABASE`, which `inillucent-catalog` uses | about 60 |
| `crates/inillucent-catalog/src/rebuild.rs` and `lib.rs:47` | `rebuild_into`, callerless since `inillucent-session` was deleted; round one recommended this and it was not acted on | 169 |

Only the crates named in `docs/repository.md`'s "kept for reading SQLite files" row and
`inillucent-sqlite-reader` still depend on the two retired crates (`inillucent_transaction::` is
imported by `inillucent-sqlite-reader/src/lib.rs`, `inillucent-storage/src/pager.rs`, and two
`inillucent-compat` binaries; the only paths are `recovery::` and `open_database`).

**What the implementer does.** Remove the functions inside files that stay (`vacuum.rs` down to
`relocate_page`, the `overflow.rs` pair, the `pager.rs` cluster, `databases.rs` down to
`TEMP_DATABASE`), and remove the module declarations and re-exports for the three whole files
(`state.rs`, `check.rs`, `rebuild.rs`), moving the one thing `check.rs` still provides to the
compat binaries into the test crate that uses it. The whole files themselves are section 6's to
delete. The module size ratchet in `policy.rs` is lowered to the new sizes in the same commit.

### M2. Six functions whose own doc comment says they are unreachable and flagged for removal

`crates/inillucent-engine/src/lib.rs:3039-3065` `backup_into`, marked `#[allow(dead_code)]` with
"Unreachable since `VACUUM INTO` took over ... candidate for removal in a later cleanup pass", and
five in `crates/inillucent-tree/src/leaf.rs`: `fixed_size` (2699), `slot_need` (3373),
`slot_need_of` (3401), `costs_at` (3446), `heap_cost_at` (3524), each saying "Unreachable since the
narrow slot pricing moved onto the width arrays ... flagged for removal elsewhere so a person
decides". This document is the person deciding. Delete all six and their `#[allow(dead_code)]`.
Thirteen other `#[allow(dead_code)]` sites in the workspace carry a reason that still holds and
stay.

### M3. Ten more callerless public items, read and confirmed

`crates/inillucent-value/src/record.rs:410` `field_extent` (30 lines) and `record.rs:53`
`SerialType::INTEGER_ONE`, `INTEGER_ZERO`; `crates/inillucent-catalog/src/ddl.rs:284`
`find_schema_row` (27), `ddl.rs:141` `update_schema_row` (17), `analyze.rs:191,204` `clear_stats`,
`write_stat`; `crates/inillucent-catalog/src/paged.rs:49-197` four schema constants;
`crates/inillucent-pool/src/page.rs:104` `MIN_PAGE_BYTES` (22); `crates/inillucent-exec/src/lateral.rs:157`
`batch_over` (20); `crates/inillucent-sql/src/plan.rs:2088` `virtual_offer` (20);
`crates/inillucent-ext/src/vtab/rtree.rs:1601-1606` `out_of_range`, `#[allow(dead_code)]`,
duplicating the inline check at `rtree.rs:895-899` with a different message. Delete each. The
remaining items in the sweep's list of 101 are small (constants, one line accessors) and the
implementer removes any that a grep confirms; the sweep's method and its exclusions are in the
task's closing comment.

**A ratchet so the list does not regrow.** Add `no_public_item_is_callerless` to `policy.rs`: for
every `pub fn`, `pub const` and `pub struct` in `crates/*/src` and `drivers/*/src`, the identifier
occurs in at least one other tracked file or at least one other line of its own file. Exclusions,
listed in the test with a reason: trait methods, `main`, items under `#[cfg(test)]`, and the C ABI's
`extern "C"` exports. It is the check the compiler cannot do for a public item.

### M4. JSON string escaping is implemented three times

`crates/inillucent-cli/src/json.rs:200`, `crates/inillucent-scalar/src/json/node.rs:111` (identical
branch logic), `crates/inillucent-sim/src/trace.rs:55` (two branches fewer). One helper in
`inillucent-base` (the crate all three already depend on), with one test of every escape SQLite's
`json_quote` produces, and the three sites call it. The layering contract in
`docs/invariants/layering.toml` already allows each of the three to depend on `inillucent-base`.

### M5. `compat-report.md` lists fourteen capabilities as passing on Windows only

`compat/compat-report.md:32-45`, the Problems table, flags fourteen rows
`unsupported-release-claim: no passing result recorded on linux-x86_64`, among them
`sql.expr.operators`, `sql.expr.like-glob`, `catalog.sqlite-schema`, `sql.select.window`,
`functions.date-time`, `sql.binder.name-resolution`, `sql.functions.scalar-core` and
`api.rust.bind-step-reset` (lines 174-314). The manifest says `pass`; the evidence column says
`partial`. The code for each was read and looks correct on every platform, so this is most likely
the Linux CI run never having written results back into `compat/results`; but a public
compatibility report should not carry its own unresolved Problems table. Run the oracle graded
suites on Linux in CI with results written back, regenerate, and gate `validate.sh` on an empty
Problems table.

Also in the manifest: `optional.geopoly` is `missing` while `crates/inillucent-ext/src/vtab/rtree.rs`
and `crates/inillucent-scalar/src/geopoly.rs` implement polygon parsing, bounding boxes and an
R-Tree backed shape column. Decide what the row measures (the registered `geopoly_*()` SQL
functions, most likely) and make the note say so.

### M6. Crate metadata: one crate has none, four internal crates are publishable, and the README's Rust name is not a crate

- `crates/inillucent-core/Cargo.toml:1-23` has no `license`, `description`, `repository`, `readme`
  or `[lints] workspace = true`. It is the only crate without them and the one most likely to be
  depended on alone. Add the five lines the sibling `inillucent-search/Cargo.toml:1-7` has.
- `inillucent-model`, `inillucent-sim`, `inillucent-storage` and `inillucent-transaction` lack
  `publish = false`; `inillucent-compat` and `inillucent-bench` have it. Add it to the four: a test
  oracle, a simulator and two retired crates should not reserve names on crates.io.
- No `rust-version` anywhere. `rust-toolchain.toml` pins 1.95.0 for contributors, which says nothing
  to a `cargo install` user about the floor. Add `rust-version` to `[workspace.package]` and
  `rust-version.workspace = true` to every published crate, at the lowest version the workspace
  builds on (verify by building once with it).
- `README.md:149` says the Rust client will be `cargo add inillucent-client`, and the crate that
  exists is `drivers/inillucent-driver`. The paragraph above the table is candid that none of the
  eight client packages exists yet, so this is not a false claim today; it becomes one the day the
  driver is published under its own name. Either rename the table row to `inillucent-driver` now or
  decide that the published name will be `inillucent-client` and say so in `drivers/README.md`.
- `README.md:66-68` says `go install` needs `GOPRIVATE` "because the repository is private". That
  sentence is wrong on the day the repository is public; the implementation ticket removes it and
  `tools/doc-facts/check.mjs` gets an assertion that the word `private` does not describe the
  repository anywhere in `README.md`.

### M7. Community files and a security policy

None of `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `CHANGELOG.md`,
`.github/ISSUE_TEMPLATE/`, `.github/PULL_REQUEST_TEMPLATE.md` or `.github/dependabot.yml` exists.
`SECURITY.md` matters most for a program that parses untrusted SQL and untrusted files: it says
where to report and what the response window is. `CONTRIBUTING.md` can be short, because
`AGENTS.md` section 2 already is the contributor guide; it points there and adds the two things
`AGENTS.md` does not say: how to run the suite on a machine without the oracle, and that a pull
request needs the contract files updated in the same change. `CHANGELOG.md` starts at 0.1.0 with
the three releases that exist, one paragraph each, taken from the release commits. Dependabot for
cargo and npm, weekly.

### M8. CI has no dependency audit and no documentation build

`.github/workflows/ci.yml`, `tools/validate.ps1` and `tools/validate.sh` run fmt, clippy with
`-D warnings`, the oracle, the four contracts, the security suites and the strict test run, on
three operating systems and four language wrappers, with no secret required. Two stages are
missing: `cargo deny check` (licenses, advisories, duplicate versions) with a `deny.toml` that
allows the licenses the dependency policy already allows, and
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`, so a broken intra doc
link fails the build rather than the docs.rs page. Both go into `validate.*` so a developer and CI
agree on what green means.

### M9. Documents that disagree with the code

| where | says | is |
|---|---|---|
| `Cargo.toml:50`, the release profile comment | "eighteen crates" | 29 workspace members |
| `docs/repository.md:98,107` | 149 targets, 2,646 tests | 165 targets, 2,777 tests at `51309f8`; re-measure at the implementation commit with `tools/doc-facts/check.mjs --run-tests` |
| `tests/inillucent-testing-tdd.md:447,464` | 2,642 tests | the same re-measurement |
| `crates/inillucent-wal/src/writer.rs:442`, `crates/inillucent-compat/tests/durability.rs:1192`, `free_map_checkpoint_crash.rs:557`, `tests/inillucent-testing-tdd.md:576` | credit a finding to "Codex Sol" or `codex exec review` | the house style credits a ticket number; write "the task-N review found" |
| `tests/performance-history.tsv`, eight rows | machine `jason-25` | a personal hostname in shipped data; relabel at the next measurement and say in `docs/performance.md` what the label means |
| `crates/inillucent-txn/src/redo.rs:218` | names a test that recovers twice and compares bytes | no test has that name; the closest is `crates/inillucent-txn/tests/durability.rs:410`; name that one |
| `crates/inillucent-core/src/store.rs:735`, `crates/inillucent-search/src/merge.rs:486` | a doc line duplicated onto itself (`.../// One chunk's ...` twice) | delete the duplicate half |
| `crates/inillucent-core/src/embed_onnx.rs:1045` | about eighteen spaces inside a warning string | collapse |
| `tasks/task-1836-cli-mcp-and-installers-tdd.md:448` | names `claude-settings/opencode/opencode.json` and `aiservice-web` | two private repositories; remove the sentence (and see section 6 on `tasks/` as a whole) |

### M10. The rollback journal's ordering invariant has no fault injection test

`crates/inillucent-pool/src/journal.rs:3-8` states the whole correctness argument: a page's old
image is on disk and synced before its new image is written. `crates/inillucent-pool/tests/fault_campaign.rs:14-18`
says crash coverage of the log arrives in a later phase, and no campaign in `inillucent-pool` or
`inillucent-compat` cuts between the old image write and the new image write under the rollback
journal. The free map has exactly this kind of test (`free_map_checkpoint_crash.rs`); the journal
does not. Add a campaign that cuts at every `Site` between those two writes and asserts the file
recovers to the pre statement image.

### M11. The MCP and command line still reach past the driver from seven files

Round one's M1 asked for the driver to be the one surface. task-1932 added `begin()`, named
binding and a bounded cache, and put a ratchet on the count of `inillucent-cli` files that import
`inillucent_engine` directly (`crates/inillucent-compat/tests/policy.rs:1375-1405`), now at seven.
`drivers/README.md` does not claim the migration is finished. This round asks for the ratchet to
go down by at least the files that only need `Database::open` and `query`, and for the remaining
count to be recorded in `docs/roadmap.md` as an item with an owner. It is not a defect; it is the
one architectural claim in the README that is not yet true.

### M12. Function and file sizes

Round one's recommendation is implemented: `policy.rs` holds a function length ratchet with 65
entries and a module ratchet with 10, both only ever lowered, and the largest files are smaller than
they were (`inillucent-engine/src/lib.rs` 7,428 from 7,863; `inillucent-exec/src/physical.rs` 5,732
from 6,663). What this round adds:

- `crates/inillucent-migrate/src/verify.rs:364-592` `retrieval` (229 lines) and
  `crates/inillucent-remote/src/migrate.rs:420-610` `run` (191 lines) are over 150 and not on the
  ratchet, because the ratchet covers only engine, exec, sql and tree. Extend the ratchet to every
  governed crate and record these two at their current length.
- `crates/inillucent-pool/src/pool.rs` (2,728 lines) holds the frame table, eviction, journal sync
  gating and swip logic together. Split along those four seams, the way `inillucent-tree` separates
  `leaf.rs` from `paged.rs`; a mechanical move, no behaviour change, ratchet entries lowered.
- `crates/inillucent-tree/src/paged.rs` `skip_scan` (249), `leaf.rs` `encode_rows_with` (239),
  `write.rs` `write_row` (206), `paged.rs` `bulk_build_rows` (167), `write.rs` `make_room` (166),
  `write.rs` `merge_if_small` (157): none has shrunk since round one. Take `write_row` and
  `make_room` down below 150 by extracting the split and merge decisions they already comment as
  separate steps; leave `skip_scan` and `encode_rows_with`, which are one loop each and would not
  get clearer cut in two.
- `bind.rs`: not in this ticket (see Non-goals). task-1913 owns the split.

### M13. Recovery reads a page before redo outside the catalog root's repair pass

Unchanged from round one's M3 and `docs/roadmap.md` item 12:
`crates/inillucent-engine/src/recovery.rs:181-207` repairs only the catalog root's read. Listed here
so the record shows it was re-checked and is still open; it stays on the roadmap and is not part of
the implementation ticket.

## 6. Decisions for Jason, and files for Jason to delete

None of these is an implementer's call, and an agent does not delete a file it did not create.

**Files to delete**, once the implementation ticket has removed every reference (both orders
build, because an unreferenced `.rs` file is not compiled):

```sh
git -C <machine path>/inillucent rm crates/inillucent-compat/--db crates/inillucent-compat/--db-wal.0000000001
git -C <machine path>/inillucent rm crates/inillucent-transaction/src/state.rs
git -C <machine path>/inillucent rm crates/inillucent-storage/src/check.rs
git -C <machine path>/inillucent rm crates/inillucent-catalog/src/rebuild.rs
```

**Decision 1. The git history.** 89 of the 327 commits on `main` carry a `Co-Authored-By: Claude`
trailer and a `claude.ai/code` session link (`git rev-list --count HEAD -i --grep=Co-Authored-By`).
task-1934 found that `Black-Rainbow-Labs/Inillucent` on GitHub holds ten squashed commits sharing no
history with this repository. If the public repository is refreshed by squashing again, the
trailers never leave this machine. If the intent is to push this history, the trailers and links
need rewriting first, and every collaborator's clone becomes stale. Squashing is the smaller change.

**Decision 2. `tasks/`.** Thirty one design documents named by ticket, plus this one. Three were read
end to end: they read as internal work orders (pinned reviewer commits, ticket cross references,
the names of other agents and, in one, two private repositories), and they are also the most
complete record of why the engine is the way it is. Options: ship them as they are after M9's two
corrections; move a curated set under `docs/design/` with the ticket prefixes kept as history; or
keep `tasks/` out of the public tree. The dependency policy and roadmap already cite several by
path, so the second option needs those links updated.

**Decision 3. The Nikaya case study.** `docs/real-world-use-cases/nikaya-postgres-to-inillucent.md`
and nine other documents name a real production deployment. It reads as a deliberate case study
rather than a leak, and nothing in it is a credential or a path. Confirm it is meant to be public.

**Decision 4. `examples/rag-agent/greek-philosophy.rdb`**, 21 MiB, the largest tracked file, kept so
an agent can search in its first minute. `scripts/build-database.sh` regenerates it from the tracked
corpus. Keep it, or move it to a release asset and have the example's README fetch it; the README
should say which either way.

**Decision 5. The synthetic corpus database password.** It is in the history. Change it on the
local PostgreSQL that holds `inillucent_synth`; nothing else reads it.

## 7. Components and interfaces

| interface | change | who reads it |
|---|---|---|
| `Vfs` trait, `crates/inillucent-vfs/src/contract.rs` | `fn rename(&self, from: &Path, to: &Path) -> VfsResult<()>`, atomic replace (H2) | `OsVfs`, `MemoryVfs`, `SimVfs`, `inillucent-engine::rebuild` |
| `ImportedDatabase::vacuum_in_place` | reopens with `open_on(Arc::clone(&self.vfs), ..)` (H2) | `VACUUM`, `PRAGMA incremental_vacuum` |
| `Binder`, trigger inlining | reads `Limit::TriggerDepth` from the connection's limits instead of a constant; `trigger.rs` constant and check deleted (H3) | `dml.rs`, `diagnose.rs`, the driver's limit setter |
| `inillucent-core` search entry points | `check_query(query, dims)` before any distance (H4) | `inillucent-search`, library users |
| `ConnectionUrl` | hand written `Debug` (H5) | `inillucent-remote`, `inillucent-migrate` |
| `runs::command_line` | redacts credential tokens and directory parts (H6) | `inillucent-bench` scorecard writer |
| `Directive::analyze` | calls `refresh_catalog` (H1) | the planner, through `TableInfo` and `IndexInfo` |
| `policy.rs` | `no_public_item_is_callerless`, `no_tracked_file_is_named_like_a_flag`, the widened early return check, the length ratchet over every governed crate (H8, H10, M3, M12) | every contributor |
| `tools/doc-facts/check.mjs` | version pins agree (H9), the section 12 greps are empty (H7), `README.md` does not call the repository private (M6) | the release scripts |
| `inillucent-base` | `json::escape_into(&mut String, &str)` (M4) | cli, scalar, sim |
| `Cargo.toml` of five crates | metadata, `publish = false`, `rust-version` (M6) | crates.io, `cargo install` |

## 8. Data flows and security

### `VACUUM` on a caller supplied VFS, before and after H2

```mermaid
sequenceDiagram
    participant App as Application on MemoryVfs
    participant Conn as ImportedDatabase
    participant Mem as MemoryVfs
    participant Os as OsVfs (std::fs)
    App->>Conn: VACUUM
    Conn->>Mem: read every page, rebuild into scratch
    Note over Conn,Os: today
    Conn->>Os: ImportedDatabase::open(scratch) constructs OsVfs::new()
    Conn->>Os: std::fs::rename(scratch, path)
    Conn->>Os: ImportedDatabase::open(path)
    Note over App,Os: the connection now reads a real file, or fails to open one
    Note over Conn,Mem: after H2
    Conn->>Mem: open_on(Arc::clone(vfs), scratch)
    Conn->>Mem: vfs.rename(scratch, path)
    Conn->>Mem: vfs.delete(each log segment)
    Conn->>Mem: open_on(Arc::clone(vfs), path)
```

### Where a credential can reach a tracked file, and the two gates that stop it

```mermaid
flowchart LR
    ARGV[argv with --database-url postgres://u:p@h/d] --> CL[runs::command_line]
    CL -->|today: joined as typed| SC[inillucent-scorecard.md and .json, tracked]
    CL -->|after H6: each token through redact| SC2[scorecard with postgres://u:***@h/d]
    URL[ConnectionUrl] -->|Display, tested| RED[redacted]
    URL -->|derived Debug, today| RAW[raw password in any log that formats the struct]
    URL -->|hand written Debug, after H5| RED
    SC2 --> GREP[doc-facts: section 12 greps are empty]
    RED --> GREP
```

**Risks.** H2 changes the one crash sensitive moment in `VACUUM`; the campaign in `vacuum_crash.rs`
moves onto `SimVfs` with a cut inside `rename` precisely so that moment is tested rather than
argued. H3 raises a bind time cap from 32 to 1000; the cost is bounded by the schema's trigger
count, not by the limit, and the differential test on a forty deep chain is also the timing
check. H1 adds one `refresh_catalog` per `ANALYZE`, a directive that already reads every row of
every table, so the refresh is noise beside it. M1 and M2 delete code that nothing calls, and the
compiler is the proof: if a deletion was wrong, the build fails, not a test.

## 9. Alternatives considered

| finding | alternative | why not |
|---|---|---|
| H2 | keep `std::fs` and document that `VACUUM` needs `OsVfs` | the connection still swaps backends after the statement, which is the part no document can make acceptable; and it keeps `vacuum_crash.rs` on real files, outside the simulator that every other durability campaign uses |
| H2 | add `read_dir` to `Vfs` as well | the segment names are a deterministic function of the base path, so a listing is not needed; a smaller trait change is easier for the three implementors and any fourth |
| H3 | keep 32, change the advertised number | the oracle answers 1000 and `limits.toml` says the oracle wins; keeping 32 would need a documented divergence, and the binder's cost at 1000 is a tree the schema bounds anyway |
| H4 | check inside `dot` | it is the hot loop, called per candidate; one check per query at the entry point is the same guarantee at no cost |
| H6 | stop recording the command line | the command is the reproducibility record the scorecard exists for; redacting keeps it useful |
| M1 | delete `inillucent-storage` and `inillucent-transaction` outright | still gated on the catalog rewrite (task-1816 Phase 5); trimming what is dead inside them is safe today and makes the eventual deletion smaller |
| M3 | rely on `cargo udeps` or `warnings` | neither sees a `pub` item with no caller; a grep based policy test is the only check that does, and the sweep shows it finds real code |
| M7 | skip `SECURITY.md` until there is a report | the file is how a reporter finds out where to send one; without it the first report is a public issue |

## 10. Testing strategy

Functional tests, registered in `tests/selection.toml`, each failing on `6e84068`:

| test | proves |
|---|---|
| `analyze_same_session.rs` | `ANALYZE` changes the plan on the same connection (H1) |
| `vacuum_on_vfs.rs` and `vacuum_crash.rs` on `SimVfs` | `VACUUM` stays on the caller's VFS; a crash inside `rename` recovers (H2) |
| `trigger_depth.rs`, differential | a forty deep chain matches the oracle; `.limit trigger_depth 10` refuses an eleven deep one by name (H3) |
| `inillucent-core` query validation tests | width mismatch and NaN are errors, not lists (H4) |
| `debug_redacts_the_password` | `{:?}` on `ConnectionUrl` has no password (H5) |
| `command_line_redacts_credentials_and_paths` | the scorecard's command line carries neither (H6) |
| `doc-facts` assertions | the section 12 greps are empty; every version pin equals the workspace version; `README.md` does not call the repository private (H7, H9, M6) |
| `policy.rs`: `no_tracked_file_is_named_like_a_flag`, the widened `every_early_return_in_a_test_says_why`, `no_public_item_is_callerless`, the ratchet over every governed crate | the repository cannot regrow H8, H10, M3 or M12 |
| journal ordering campaign | a cut between the old image write and the new image write recovers (M10) |
| `cargo deny check`, `cargo doc -D warnings` in `validate.*` | M8 |

Then the whole gate: `pwsh tools/validate.ps1 --strict` on a clean worktree of the implementation
commit, with the printed counts written into `docs/repository.md` and `tests/inillucent-testing-tdd.md`,
and `compat/perf/contract.toml` passing on four consecutive runs.

## 11. Implementation order

| phase | findings | why here |
|---|---|---|
| A. nothing private ships | H7, H8 (the ignore rules and the policy test), H9, H6 | each is a grep and a small edit, and each is the kind of thing that must be true before any other commit is pushed to a public remote |
| B. wrong behaviours | H1, H3, H4, H5 | small, each with a test that fails on the parent commit |
| C. the VFS change | H2 | the one design change; its own commit, with the crash campaign moved onto the simulator |
| D. the assurance layer sees more | H10, M3's ratchet, M12's ratchet extension, M8 | so the deletions in the next phase are checked by the tests they should have had |
| E. dead code | M1, M2, M3, M4 | compiler proven; ratchets lowered in the same commits; the whole files left for section 6 |
| F. documents and metadata | M5, M6, M7, M9, M11's roadmap entry | independent of the engine; can run beside E |
| G. sizes and coverage | M10, M12's splits | last, because they are the changes most likely to introduce a behaviour change and the gate has to judge them alone |

Each phase ends with `inillucent-testrun --changed` green and a commit naming the findings it
closes.

## 12. Acceptance criteria for the implementation ticket

1. Every test in section 10 exists, is in `tests/selection.toml`, and the commit message says which
   fail on `6e84068`.
2. These greps over `git ls-files` return nothing: `jasonlmcaffee`, `black.rainbow.labs@`,
   `example-company`, `postgres:inillucent@`, `C:\jason`, `C:/jason`, `J:/inillucent`, `jason-25`,
   `Codex Sol`, `codex exec`, `npmjs.com/settings`, `~/.claude`, `opencode.json`, `aiservice-web`.
   `tools/doc-facts/check.mjs` runs them.
3. `python -c "import inillucent; print(inillucent.__version__)"` prints 0.1.2, `NATIVE_VERSION` in
   the PHP installer is 0.1.2, and the version pin check fails if any copy differs from
   `[workspace.package] version`.
4. `ANALYZE` on an open connection changes `EXPLAIN QUERY PLAN` without a reopen.
5. `VACUUM` on `MemoryVfs` leaves no file on disk and the connection still answers through
   `MemoryVfs`; `vacuum_crash.rs` runs on `SimVfs`.
6. A forty deep chain of distinct triggers matches the oracle; `MAX_TRIGGER_DEPTH` exists once.
7. `inillucent-core` refuses a query whose width differs from the index or that holds a non finite
   component.
8. `format!("{:?}", url)` on a `ConnectionUrl` with a password contains `***` and not the password.
9. Both scorecard files are regenerated and contain neither a credential nor a directory path.
10. `state.rs`, `check.rs` and `rebuild.rs` are unreferenced and the build does not need them; the
    six self flagged functions and the ten items in M3 are gone; `no_public_item_is_callerless` is
    green with its exclusion list documented.
11. `crates/inillucent-core/Cargo.toml` carries the metadata; the four internal crates are
    `publish = false`; `rust-version` is set and the workspace builds on it.
12. `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, `CHANGELOG.md` and
    `.github/dependabot.yml` exist; `cargo deny check` and `cargo doc` with `-D warnings` run in
    `validate.ps1`, `validate.sh` and `ci.yml`.
13. `compat/compat-report.md` has an empty Problems table, with Linux results in `compat/results`.
14. `docs/repository.md`, `tests/inillucent-testing-tdd.md` and the `Cargo.toml` profile comment
    state the measured counts; `doc-facts --run-tests` passes.
15. `pwsh tools/validate.ps1 --strict` is green on a clean worktree of the final commit, and
    `compat/perf/contract.toml` passes on four consecutive runs.
16. Nothing in section 6 was deleted by the agent; the four `git rm` lines are in the closing
    comment for Jason, and the five decisions are listed there unanswered.
