# Crash and failure schedules

Each file here is the report of one failure campaign. A failure campaign runs a write, injects a
failure (a power loss, an I/O error, a full disk or a short write) at every point the write touches
the file system, and reopens the database after each one. The campaign writes its own report file.

The runs are seeded, so each report is reproducible. A diff on a report file means the engine now
behaves differently under failure. Show that diff in the review.

## Terms used on this page

| Term | Meaning |
|---|---|
| cut point | one call to the file system during a write. The campaign injects its failure at that call |
| journal mode | how the engine protects a write that is in progress: DELETE, TRUNCATE and PERSIST keep a rollback journal, WAL keeps a write ahead log. See [the glossary](../../docs/glossary.md) |
| checkpoint | copying committed pages from the write ahead log into the database file |
| old, new | the database as it was before the transaction, and as it is after the transaction committed. After a failure, recovery must produce one of the two |
| short write | a write call that stores fewer bytes than it was given |

## How to run the campaigns

The campaigns are ordinary test targets in `inillucent-compat`, in the `durability` tier of
`tests/selection.toml`:

```sh
target/debug/inillucent-testrun --tier durability     # every campaign, through the parallel runner

cargo test -p inillucent-compat --test durability durability::     # the rollback journal campaigns and recovery-crash.txt
cargo test -p inillucent-compat --test durability wal_crash::      # the write ahead log campaigns
cargo test -p inillucent-compat --test durability faults::         # allocation.txt
cargo test -p inillucent-compat --test tooling crash_reports::  # checks every report against its floor
```

## The reports

### Rollback journal commits

These campaigns cut the power, or inject another failure, inside a commit with `synchronous = FULL`.
Each row gives the cut point, whether the transaction reported a commit (`committed` or `failed`),
and whether recovery produced the `old` or the `new` database.

| File | Failure | Journal mode |
|---|---|---|
| `delete-full-crash.txt` | power loss | DELETE |
| `truncate-full-crash.txt` | power loss | TRUNCATE. The commit point is the truncation |
| `persist-full-crash.txt` | power loss | PERSIST. The commit point is the header write |
| `delete-full-io-error.txt` | an I/O error | DELETE |
| `delete-full-disk-full.txt` | a full disk | DELETE |
| `delete-full-short-write.txt` | a short write | DELETE. The header also counts runs the engine reported as damaged |

The header line of each report gives the totals: the cut points covered, the commits acknowledged
before the failure, the runs that ended in damage the engine detected, and the runs lost to a half
written call.

### Rollback journal checkpoints

A rollback journal is written only while a checkpoint moves pages out of the log and into the data
file. A commit that reaches the log and then stops never writes a journal record. So the commit
campaigns above never test the journal. These campaigns cut inside the checkpoint that follows a
commit.

The failure is injected after the transaction is acknowledged. So the committed database is the only
correct result at every cut point.

| File | Failure | Journal mode |
|---|---|---|
| `delete-full-checkpoint-crash.txt` | power loss | DELETE |
| `truncate-full-checkpoint-crash.txt` | power loss | TRUNCATE |
| `persist-full-checkpoint-crash.txt` | power loss | PERSIST |
| `delete-full-checkpoint-io-error.txt` | an I/O error | DELETE |
| `delete-full-checkpoint-disk-full.txt` | a full disk | DELETE |

### Write ahead log

Written by `crates/inillucent-compat/tests/durability/wal_crash.rs`. Each row gives the cut point, the database
recovery produced (`old` or `new`), and whether the transaction reported a commit (`committed`). The
first line counts the cuts, how many ended `old`, how many ended `new`, and how many ended damaged
and detected.

| File | What it cuts |
|---|---|
| `wal-commit.tsv` | a commit |
| `wal-io-error.tsv` | a commit, with an I/O error |
| `wal-short-write.tsv` | a commit, with a short write |
| `wal-checkpoint.tsv` | a checkpoint |
| `wal-recovery-idempotent.tsv` | a second power loss during recovery. Each row gives the first cut, the second cut and the result |

### Other reports

| File | What it records |
|---|---|
| `recovery-crash.txt` | a power loss during the recovery from a power loss. Each row gives the first cut point, the second, and the database the second recovery produced |
| `multi-database-commit.tsv`, `multi-database-short-write.tsv` | a commit that spans two attached databases. These were written by the campaign for the earlier engine. `multi_database_crash.rs` cannot run that campaign on the current engine yet, and its opening comment says why |
| `allocation.txt` | a memory failure at every allocation of a write. It reads `points: 0, refused: 0`, because the current write path makes no allocation that the failpoint counts. `an_injected_allocation_failure_never_reaches_the_write_path` in `faults.rs` writes it. See `txn.oom-injection` in `compat/sqlite-3.53.4.toml` |

## A report that shrinks is a failure

A campaign that reports fewer cut points than before has stopped testing something.
`crates/inillucent-compat/tests/tooling/crash_reports.rs` holds a floor for each report except
`allocation.txt`, and fails when a report falls below its floor. The floor lives in the test source
because a campaign rewrites its own report file. Raising a floor is a deliberate edit.

The first version of the DELETE campaign reported 127 cut points while causing only thirteen
crashes. A crash armed at a read did nothing, and the failpoint counter had already passed the low
numbers before the run began.

`crash_reports.rs` also checks that no report contains a carriage return. `.gitattributes` gives
`tests/crash/` LF line endings, so a checkout does not show every report as modified.
