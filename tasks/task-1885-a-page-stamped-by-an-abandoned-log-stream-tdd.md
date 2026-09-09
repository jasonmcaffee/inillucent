# task-1885: a page stamped by an abandoned log stream

A committed write is discarded at the next open, with no error, and
`PRAGMA integrity_check` answers `ok` about the result.

---

## 1. The measurement, taken again before writing any code

The recipe in the ticket, run on a copy of the parked file:

```
copy J:/nikaya-data/wal-parked-task1876/nikaya.rdb.after-checkpoint-recovery  J:/nikaya-data/task-1885-repro/an.rdb
inillucent --db J:/nikaya-data/task-1885-repro/an.rdb analyze
    ok. sqlite_stat1 is up to date.
inillucent --db J:/nikaya-data/task-1885-repro/an.rdb query "SELECT count(*) FROM document"
    Error [io]: could not open "J:/nikaya-data/task-1885-repro/an.rdb": bad parameter or other API misuse:
                the log names tree 2147483710, which this recovery was not told the shape of
```

`analyze` took 5.6 seconds and printed `ok`. The reopen failed. Read out of the file's own bytes,
and out of the segment `analyze` wrote:

| | |
|---|---|
| `meta.page_size` | 32,768 |
| `meta.checkpoint_lsn` | 21,074,969,552 |
| `meta.wal_sequence` | 315 |
| segment 315 `first_lsn` | 21,074,969,552 |
| segment 315 last byte | 21,075,008,440 |
| page 3, the catalog leaf | stamped **21,939,058,496** |
| pages among the first 3,000 stamped at or above the log's end | **9** (pages 3, 4, 5, 6, 9, 16, 23, 45, 46) |

Page 3 carries a stamp 864,049,488 positions beyond the last byte of the log beside it.

## 2. What the stamp means, and where the rule breaks

`recover::replay` applies a record to a page only when the page's stamp is below the record's LSN:

```rust
let below = match redo.page_lsn(*page)? {
    Some(lsn) => lsn < record.lsn,
    None => true,
};
```

That is the idempotence rule, and it is correct **while a page's stamp is a position in the log
currently beside the file**. It is that in every healthy file, and the write-ahead rule is what makes
it so: `Pool::refuse_if_ahead_of_the_log` refuses to write a page whose stamp is above the log's
durable end, so every stamp in the file is below the durable end, which is at or below the end of the
chain a later recovery reads.

The parked file breaks the antecedent. Twenty-four log segments were moved aside to recover it, so
recovery resumed the stream at the end of the chain that was left, and the new stream re-used
positions that pages in the file already carried. The default journal mode is `delete`, which is a
steal policy, so pages reach the file ahead of their commit and carry stamps well above the meta's
checkpoint.

The consequence is not a failed replay. `ANALYZE` wrote `sqlite_stat1`'s catalog row into page 3, the
row went to the log, and the reopen read page 3's stamp as 21,939,058,496, decided the page already
held the record, and skipped it. The row was gone. The message about tree 2147483710 is the *second*
thing that goes wrong: the shape derivation then has no catalog row to register the new tree under.

It is not about `ANALYZE` and not about the catalog. **Any write to a page stamped above the log's
resumed position is discarded at the next open**, and the file stays structurally intact, so the
checker says `ok`.

## 3. The fix, in two parts

The broken invariant is that a page's LSN is a position in the current stream. One part keeps a file
from entering the state; the other refuses a file that is already in it rather than opening it into
silent loss.

### 3.1 Prevention: the log resumes above every stamp the file carries

**`meta.high_water_lsn`, eight bytes at offset 108.** `meta.rs` documents the reserved region for
exactly this: the checksum runs over `0..88` and `92..`, so the region was always covered, and a file
written before the field existed carries zeros there. Zero means unset.

**The pool records it.** `Pool::writeback` is the one place a page reaches the data file - the
checkpointer's flush, an eviction and a manual flush all funnel through it, which is what makes the
write-ahead rule enforceable there. It already reads the page's LSN out of the image for that check.
It records the highest one it has written in a `Cell<u64>`, and `Pool::high_water_lsn` reports it.

**The checkpoint writes it, after the flush.** `Pool::checkpoint` takes the meta record mutably and
sets `meta.high_water_lsn = max(meta.high_water_lsn, self.high_water_lsn())` between the flush and
the encode. Before the flush would be wrong: the pages the checkpoint is writing are part of the file
the record describes, so the number would be one checkpoint behind the stamps it is meant to bound.
`Database::open` seeds the pool from the meta page, so it never goes backwards across runs and
survives a run that wrote nothing.

**The open resumes above it.** After recovery, the log opens at
`max(recovered.next_lsn, high_water_lsn + 1)`, reading the high water off the pool rather than the
meta record - the pool was seeded from the meta and has since been raised for every page this
recovery evicted, so it is the higher of the two and never the lower. In every healthy file the first term already wins, so
this changes nothing: the write-ahead rule puts every stamp below the durable end, and the durable
end is at or below `recovered.next_lsn`. It only fires on a file whose log is short of what its pages
reflect.

**A jump takes a new segment, and the meta says so before a record is written.** An LSN is a byte
position in a segment: the write offset is `header + (lsn - segment.first_lsn)`, so resuming
864 million positions above the current segment's `first_lsn` would demand an 864 MB segment. The
resume therefore opens the *next* sequence, whose header records the new `first_lsn`.

That leaves a gap between segment N's last byte and segment N+1's first, and `read_chain` stops the
chain at a gap - correctly, since a gap is otherwise a lost segment. So the jump is followed by a
checkpoint that writes `checkpoint_lsn = high_water + 1` and `wal_sequence = N+1` durably, before the
session is allowed to write anything. Recovery of the next open then starts inside the new segment
and never looks at the gap. The claim `checkpoint_lsn` makes - "every page write at or below this is
durable in the file" - is true at that moment: the replay's pages have just been flushed, and there
are no records between the chain's end and the new position.

### 3.2 Detection: a stamp from a stream nobody has is refused by name

Prevention cannot repair a file already in the state, because a file written before `high_water_lsn`
existed carries zero there and the true high water is not recoverable from it. Such a file must be
refused rather than opened into silent loss.

`replay` already reads every stamp it needs. A stamp at or above the chain's `valid_end` cannot have
come from this log: a record at LSN *L* occupies `[L, L+len)` and `valid_end` is the position past
the last valid record, so every stamp a healthy file carries is strictly below it. The check is
therefore exact - it has no threshold to tune and no false positive that a healthy file can produce:

```
page 3 carries lsn 21939058496, which is at or above the log's end 21075008440:
the file was stamped by a log stream this database no longer has, and applying
records under the page-LSN rule would discard them silently
```

It is raised the moment the stamp is read, before the record that names that page is applied, and it
fails the open. `truncate_after` is not reached, so the log is left as it was found.

`DryRun::page_lsn` answers `None` for every page, so `inspect`, the `.walcheck` command and the
corrupt-WAL fuzz target are unaffected.

### 3.3 Why the two compose rather than overlap

- A file written by an older build, in the state: `high_water_lsn` is zero, no jump, and the replay
  refuses by name. Correct - the loss is not silent any more.
- A file written by this build whose segments were moved aside, where a record in the surviving chain
  names one of the stamped pages: refused, for the same reason.
- A file written by this build whose segments were moved aside, where no replayed record names a
  stamped page: the replay is complete and correct, then the log jumps above the high water and the
  file is healthy from the next write onwards.
- Every healthy file: `high_water_lsn` is below `recovered.next_lsn`, so no jump; every stamp is
  below `valid_end`, so no refusal. Nothing changes.

## 4. What changes, file by file

| file | change |
|---|---|
| `inillucent-pool/src/meta.rs` | `at::HIGH_WATER_LSN = 108`; `Meta::high_water_lsn`; encode, decode, `u64v_or_zero` for a field a older file does not carry |
| `inillucent-pool/src/pool.rs` | `high_water_lsn: Cell<u64>`, raised in `writeback`; `Pool::high_water_lsn()` |
| `inillucent-pool/src/file.rs` | `Database::checkpoint` folds the pool's high water into the meta; `Database::set_high_water_lsn` for the resume's own checkpoint |
| `inillucent-wal/src/recover.rs` | `replay` refuses a stamp at or above `valid_end`, naming the page, the stamp and the log's end |
| `inillucent-engine/src/lib.rs` | `open_file` resumes at `max(next_lsn, high_water + 1)`, on the next sequence, with the meta checkpointed first |
| `inillucent-txn/src/engine.rs` | the same resume in the second engine's `open`, so both open paths obey one rule |

## 5. Tests

In `inillucent-pool/src/meta.rs` (unit, beside the record it is about):

- `high_water_lsn` round-trips, and the corrupt-every-byte sweep covers it - it is inside the
  checksum's range, so the existing sweep extends over it once the field is written.
- A meta page written with zeros from 92 onwards decodes with `high_water_lsn == 0`, which is the
  compatibility claim stated as a test rather than as a comment.

In `inillucent-wal/tests/recovery.rs`:

- A page stamped at or above the chain's `valid_end` fails recovery with an error naming the page and
  both numbers, and **no record is applied** - the applier's count is zero, which is what says the
  refusal came before the damage rather than after it.
- A page stamped below `valid_end` and above the record's LSN is still skipped and still recovers,
  which is the idempotence rule the refusal must not have broken.

In `inillucent-compat/tests/analyze_reopen.rs`, beside the search that is already written down there:

- **The synthetic reduction the ticket says does not exist.** Build a database, checkpoint it, stamp
  a page above the log's end by hand - the file format is open and the stamp is eight bytes at offset
  zero of the page - then write to that page and reopen. Without the fix the write is gone and the
  reopen answers the old value; with it the open is refused by name. This is what the ticket's
  "distance evidently matters" was: the earlier attempt truncated the log and let the page reach the
  file again before the next open, so the log was never asked. Stamping the page directly removes the
  variable instead of trying to reproduce the conditions that set it.
- A file whose `high_water_lsn` is above its log's end opens, resumes above the stamp, and a write
  made after that reopen survives the next open. That is the prevention half, and it fails on the
  current build.

And the real one, which is a measurement rather than a test target: the parked file, through the
recipe in §1.

## 6. What it did, measured

Everything in §5 is written and green, and every test was shown to fail with the change reverted
rather than assumed to.

| | |
|---|---|
| `inillucent-pool` lib | 91 passed |
| `inillucent-txn` (lib, durability, transactions) | 47 passed |
| `inillucent-wal` (lib, log, recovery) | 56 passed |
| the changed selection, `inillucent-testrun --changed` | 127 targets, 1,594 tests |

Two failures in that run, neither from this change: `inillucent-compat::policy` reports
`crates/inillucent-exec/src/dml.rs` unformatted, and `inillucent-cli::lib` failed once in a build
race and passes on its own (52 tests). Both belong to task-1890, which was editing
`inillucent-exec` and `inillucent-engine` in the same checkout while this ran.

### The reduction, which now exists

`a_page_stamped_above_the_logs_end_refuses_the_open` and
`a_log_below_the_files_high_water_resumes_above_it`, in `analyze_reopen.rs`. With the refusal
removed, the first prints the silent loss in as many words:

```
the open accepted a file stamped by a stream it does not have, and the committed
CREATE TABLE read back as Err(... "no such table: later") - which is the silent loss
```

The earlier search failed because it went after the *conditions* that set the distance. The state is
eight bytes and they are outside the page checksum, so the test stamps the page and the variable is
gone.

### The parked file

Recipe run end to end against a fresh copy of
`J:/nikaya-data/wal-parked-task1876/nikaya.rdb.after-checkpoint-recovery`:

```
query "SELECT count(*) FROM document"   ->  66793          (a read still works)
analyze                                 ->  ok. sqlite_stat1 is up to date.
query "SELECT count(*) FROM document"   ->  Error [io]: database disk image is malformed:
    page 3 carries lsn 21939058496, which is at or above the log's end 21075008440: the file
    was stamped by a log stream this database no longer has, so replaying under the page-LSN
    rule would discard the records for that page silently
```

The file is refused where it used to blame tree 2147483710, and the numbers in the message are the
two the diagnosis rests on. A plain read of such a file still works, which is deliberate: refusing
every read would take away the ability to get data out of one.

## 7. What this does not do

It does not repair the parked file. A file whose pages carry stamps from a stream nobody has cannot
have those writes recovered - the records are in segments that were moved aside - and pretending
otherwise would mean guessing which of the two numbers on the page is the real one. The parked file
will be refused by name, and the message will say why.

It does not change any healthy file's behaviour. `high_water_lsn` is written from the next checkpoint
onwards; every existing file reads zero there and opens exactly as it does today unless a stamp above
the log's end is actually found, which is the state this ticket is about.
