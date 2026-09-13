# Crash and failure schedules

Each file here is what one failure campaign did, written by the campaign that
ran it. The runs are seeded, so a file is reproducible: a diff on one is a
change in what the engine does under failure, which is the thing a review
should be shown rather than told.

| file | campaign | what each line says |
|---|---|---|
| `delete-full-crash.txt` | power loss at every cut point of a DELETE-mode FULL commit | the call the power went at, whether the transaction had reported a commit, and whether recovery produced the old or the new database |
| `truncate-full-crash.txt` | the same in TRUNCATE mode, whose commit point is the truncation | as above |
| `persist-full-crash.txt` | the same in PERSIST mode, whose commit point is the header write | as above |
| `delete-full-io-error.txt` | an I/O error at every cut point | as above |
| `delete-full-disk-full.txt` | a full disk at every cut point | as above |
| `delete-full-short-write.txt` | a short write at every cut point | as above, plus `detected` for a run the engine reported as damaged |
| `delete-full-checkpoint-crash.txt` | power loss at every cut point of the **checkpoint** that follows a DELETE-mode commit | the call the power went at, and that recovery produced the committed database |
| `truncate-full-checkpoint-crash.txt` | the same in TRUNCATE mode | as above |
| `persist-full-checkpoint-crash.txt` | the same in PERSIST mode | as above |
| `delete-full-checkpoint-io-error.txt` | an I/O error at every cut point of a checkpoint | as above |
| `delete-full-checkpoint-disk-full.txt` | a full disk at every cut point of a checkpoint | as above |
| `recovery-crash.txt` | a power loss *during* the recovery of a power loss | the first cut point, the second, and which database the second recovery produced |
| `allocation.txt` | a memory failure at every allocation of a write | the allocation, and whether it was refused. Written by the old engine (`inillucent-session`); `an_injected_allocation_failure_never_reaches_the_write_path` in `faults.rs` now writes `points: 0, refused: 0` here, because the shipping engine's write path makes no allocation the failpoint counts - see `txn.oom-injection` in `compat/sqlite-3.53.4.toml` |

The header line of a campaign report carries the totals: how many cut points
were covered, how many runs had reported a commit before the power went, and
how many ended in damage the engine detected rather than served.

**Two different things used to be called `reported` in the same file** - the
header's count of runs that ended in detected damage, and a per-row column
saying whether the transaction had reported a commit. A reader comparing
`0 reported` in the header against six rows saying `true` had every reason to
think one of them was wrong. The column is `committed` now and the header says
`damaged and detected`.

A campaign that reports fewer cut points than it used to has stopped testing
something. That is the failure mode these files exist to make visible: the
first version of this campaign reported 127 cut points while causing thirteen
crashes, because a crash armed at a read did nothing and the failpoint counter
was already past the low numbers before the run began.

## The checkpoint campaigns, and why the commit campaigns were not enough

The commit campaigns above crash inside the commit. **A rollback journal is not
on that path.** The journal holds page pre-images while a *checkpoint* moves
pages out of the log and into the data file, and nowhere else - so a commit
that reaches the log and stops never writes a journal record, and a campaign
that only crashes inside the commit never tests one.

TRUNCATE and PERSIST were covered by accident. `PRAGMA journal_mode = truncate`
is a real change from the connection's default and runs two checkpoints on its
way in, so those campaigns crashed inside a checkpoint without anybody
intending it - and that is where all three of the journal defects task-1911
fixed were found. `PRAGMA journal_mode = delete` matches the default, returns
without doing anything, and left the **default** journal mode the only one of
the three never tested inside a checkpoint.

The `*-checkpoint-*` files close that. Their assertion is also the stronger one:
the failure is armed *after* the transaction is acknowledged, so the committed
state is the only answer allowed at any cut point, where a crash inside a
commit can only be asked for the old database or the new one.

The same gap was in `crates/inillucent-compat/tests/search_crash.rs`, whose
`TAIL` was two `SELECT count(*)` statements - both served out of the buffer
pool, making no VFS call at all, so `a_rollback_journal_commit_is_atomic_across_both`
was covering the log rather than the journal its name claims. It runs
`PRAGMA wal_checkpoint` now.
