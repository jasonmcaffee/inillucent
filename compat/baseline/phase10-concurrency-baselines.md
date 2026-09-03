# Concurrency and multi-database baselines, phases 9 and 10

Platform: `windows-x86_64`

Every run is on the real operating-system VFS at a stated durability level, and **the checkpoint is inside the timed region**. That last one is the only thing about this table that needs defending. A write-ahead log is quick at commit time precisely because it defers work; a benchmark that stops the clock before the checkpoint is measuring the deferral rather than the system, and would report a log as free. The `Ckpt%` column says how much of each family's total was the checkpoint, so the deferral is visible rather than hidden.

These are baselines, not comparisons. Nothing here is measured against SQLite. The wall-clock columns move with the machine and the filesystem; the frame, sync and page counters do not, and they are what a later change should be read against.

Each family ran 3 times and the quietest attempt is the one reported, kept whole so that rows meant to be compared with each other come from the same attempt. On a machine that is also running a virus scanner and a compiler, the alternative is to report the interference.

| Family | Workload | Sync | Ops | ns/op | p50 us | p95 us | p99 us | s | Ckpt% | Frames | Syncs | Backfilled | Pages | Bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| wal-commit | insert-autocommit | full | 1000 | 461469 | 421.6 | 868.6 | 1592.0 | 0.461 | 0.3% | 3151 | 1675 | 1487 | 0 | 12982120 |
| wal-commit | insert-autocommit | normal | 1000 | 231242 | 256.1 | 445.5 | 690.5 | 0.231 | 0.5% | 3151 | 675 | 1487 | 0 | 12982120 |
| wal-commit | insert-autocommit-journal | full | 1000 | 1081139 | 1032.4 | 1376.8 | 1542.4 | 1.081 | 0.0% | 0 | 0 | 0 | 2151 | 8810496 |
| checkpoint | passive | full | 6377 | 92 | 583.5 | 583.5 | 583.5 | 0.001 | 100.0% | 0 | 1 | 19 | 0 | 0 |
| checkpoint | full | full | 6377 | 85 | 542.7 | 542.7 | 542.7 | 0.001 | 100.0% | 0 | 1 | 19 | 0 | 0 |
| checkpoint | restart | full | 6377 | 96 | 615.2 | 615.2 | 615.2 | 0.001 | 100.0% | 0 | 1 | 19 | 0 | 0 |
| checkpoint | truncate | full | 6377 | 408 | 2602.7 | 2602.7 | 2602.7 | 0.003 | 100.0% | 0 | 1 | 19 | 0 | 0 |
| recovery | reopen-and-rebuild | full | 2000 | 19357 | 38713.8 | 38713.8 | 38713.8 | 0.039 | 0.0% | 0 | 0 | 0 | 0 | 0 |
| readers-and-writer | 1-readers | full | 1391 | 539513 | 529.9 | 874.2 | 1130.4 | 0.750 | 0.0% | 4344 | 2452 | 2277 | 0 | 17897312 |
| readers-and-writer | 4-readers | full | 1380 | 543612 | 553.4 | 849.6 | 1125.1 | 0.750 | 0.0% | 4311 | 2421 | 2237 | 0 | 17761352 |
| contention | two-writers | full | 676 | 1110158 | 541.0 | 3673.6 | 11748.8 | 0.750 | 0.0% | 2052 | 1150 | 977 | 0 | 8454240 |
| foreign-keys | insert-child-keys-off | full | 1000 | 538411 | 477.6 | 905.1 | 1412.6 | 0.538 | 0.0% | 3020 | 2000 | 2022 | 0 | 12442400 |
| foreign-keys | insert-child-keys-on | full | 1000 | 722325 | 647.5 | 1128.9 | 1824.1 | 0.722 | 0.0% | 3020 | 2000 | 2022 | 0 | 12442400 |
| foreign-keys | delete-parent-cascade | full | 1000 | 987660 | 900.9 | 1601.4 | 2622.2 | 0.988 | 0.0% | 4010 | 2000 | 3010 | 0 | 16521200 |
| attach | 1-database-commit | full | 1000 | 1146947 | 1111.6 | 1463.0 | 1647.8 | 1.147 | 0.0% | 0 | 0 | 0 | 2034 | 8331264 |
| attach | 2-database-commit | full | 1000 | 2617201 | 2551.3 | 3245.1 | 3773.9 | 2.617 | 0.0% | 0 | 0 | 0 | 2034 | 8331264 |
| services | backup | full | 64 | 40038 | 2562.4 | 2562.4 | 2562.4 | 0.003 | 0.0% | 67 | 2 | 0 | 0 | 276072 |
| services | serialize | full | 64 | 3966 | 253.8 | 253.8 | 253.8 | 0.000 | 0.0% | 0 | 0 | 0 | 0 | 0 |
| services | blob-write-one-byte | full | 1000 | 531226 | 504.7 | 959.4 | 1515.1 | 0.531 | 0.0% | 2996 | 1667 | 1339 | 0 | 12343552 |

## What the shape of these numbers says

The absolute figures belong to this machine. The *relations* between them are the findings, and they are what a later change should be checked against.

**A log is worth having, and the checkpoint does not take it back.** The same thousand autocommitted rows cost roughly a third of what they cost through a rollback journal, and that is with the whole log copied back inside the timed region - the `Ckpt%` column puts the checkpoint at well under one percent of the total. The journal writes an undo image of every page it touches before it touches it; the log writes the new page once and sorts it out later, and later turns out to be cheap.

**Later is cheap because a log compacts.** A checkpoint of a log holding several thousand frames writes only as many pages as there are distinct pages in it - two thousand commits to a small table leave two thousand copies of the same handful of pages, and only the newest of each is copied. That is the single most important property of the design and it is why deferring is not merely postponing.

**`TRUNCATE` is the expensive mode and the other three are not.** Passive, full and restart differ from each other by noise here; truncate costs several times any of them, because shortening the file is a metadata operation the file system has to make durable. A caller that wants the log to stop growing wants `RESTART`; only a caller that wants the file *gone* should pay for `TRUNCATE`.

**Readers do not cost the writer.** Going from one reader to four leaves the writer's commit rate within a few percent of where it was, while the readers do several times as many queries. That is the promise WAL mode exists to make, and it is the one number here that would look completely different under a rollback journal, where every reader is a lock the writer has to wait behind.

**Two writers is a tail-latency story, not a throughput one.** The pair together commit about as often as one writer alone; what changes is p99, which is several times p50 because a refused transaction waits and tries again. Contention costs predictability rather than work.

**Enforcement and atomicity both have a price, and it is visible.** Foreign keys on cost around forty percent more per child insert than keys off, which is the lookup. A two-database commit costs close to three times a one-database commit, which is the super-journal: a file written, synced and removed on every transaction, and the whole reason the two databases move together.


## What each family says

- **wal-commit / insert-autocommit** - one transaction per row, then the whole log copied back
- **wal-commit / insert-autocommit** - one transaction per row, then the whole log copied back
- **wal-commit / insert-autocommit-journal** - the same rows through a rollback journal, for scale
- **checkpoint / passive** - 19 pages written for a log of 6377 frames; the checkpoint reported 6377 of 6377 frames copied
- **checkpoint / full** - 19 pages written for a log of 6377 frames; the checkpoint reported 6377 of 6377 frames copied
- **checkpoint / restart** - 19 pages written for a log of 6377 frames; the checkpoint reported 6377 of 6377 frames copied
- **checkpoint / truncate** - 19 pages written for a log of 6377 frames; the checkpoint reported 6377 of 6377 frames copied
- **recovery / reopen-and-rebuild** - 2000 rows read back after 1 index rebuild; 20 frames then served from the log
- **readers-and-writer / 1-readers** - 2674 reads by 1 readers, slowest 5835 us, while the writer committed 1391 times
- **readers-and-writer / 4-readers** - 10227 reads by 4 readers, slowest 17552 us, while the writer committed 1380 times
- **contention / two-writers** - 676 commits here and 630 on the other connection
- **foreign-keys / insert-child-keys-off** - the same inserts with the parent lookup on and off
- **foreign-keys / insert-child-keys-on** - the same inserts with the parent lookup on and off
- **foreign-keys / delete-parent-cascade** - each delete takes one child with it
- **attach / 1-database-commit** - one database, no super-journal
- **attach / 2-database-commit** - a super-journal is written, synced and removed per transaction
- **services / backup** - 64 pages copied in 1 steps of 64
- **services / serialize** - 262144 bytes handed over without a temporary file
- **services / blob-write-one-byte** - one byte of a value, without reading or rewriting the row

