# Mutation baselines, phase 4

Platform: `windows-x86_64`

These are baselines, not results. Nothing here is a comparison against SQLite, and
no number here should be read as one. They exist so that a later phase which changes
a write path has something to change it against.

Every database is in memory, so the numbers are the engine's own rather than the file
system's. `Images` counts page copies - the undo image and the edited copy - which is
what a write costs before it reaches the file. `Amp` is bytes written to the file for
each byte of payload stored; one is a floor a page-based engine cannot reach, because
a hundred-byte row lands on a four-kilobyte page and the page is what gets written.

## What was changed, and what the numbers said

Two hot spots were found by measuring, and only one of them was worth what it cost.

**Validating a page was being paid four or five times for one row.** An interior
table page holds hundreds of cells and every one is decoded before the first is
read, and a descent, an insert and a balance each parsed the same page again. The
result now hangs off the cache frame, which gets its invalidation for free: a writer
publishes a new frame rather than mutating the old one, so a layout cannot outlive
the bytes it describes. A rowid seek at the large scale went from 6495 ns to 756,
and a hundred-row range scan from 15256 to 6517.

**Finding a page in a shard was a walk down a list.** The frames are in a map now,
and the clock hand keeps its own ring. This one is recorded honestly: at the
four-thousand-page cache the write workloads did not move outside run-to-run noise,
which is five to twenty-five per cent here. What justifies it is `cache-lookup`,
which stays flat as the cache grows forty-fold - the step at a hundred and sixty
thousand pages is the working set leaving the processor cache, not the algorithm.

Hoisting the local-payload window out of the per-cell loop was tried too. It is kept
because it is the same arithmetic in a place it is not repeated, but it moved no
measurement outside noise and is not claimed as a speedup.

| Workload | Scale | Page | Ops | ns/op | Alloc | Freed | Images | Writes | Amp | Cache bytes |
|---|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| `insert-sequential` | small | 4096 | 1000 | 7282.3 | 41 | 0 | 1663 | 0 | 0.00 | 172032 |
| `commit-per-dirty-page` | small | 4096 | 42 | 3002.4 | 0 | 0 | 2 | 43 | 1.47 | 0 |
| `seek-by-rowid` | small | 4096 | 1000 | 363.8 | 0 | 0 | 0 | 0 | 0.00 | 176128 |
| `range-scan-100-rows` | small | 4096 | 1000 | 5533.3 | 0 | 0 | 0 | 0 | 0.00 | 176128 |
| `insert-random` | small | 4096 | 1000 | 7440.3 | 36 | 0 | 1631 | 0 | 0.00 | 323584 |
| `replace` | small | 4096 | 1000 | 10544.4 | 29 | 0 | 2920 | 0 | 0.00 | 442368 |
| `delete-sequential` | small | 4096 | 1000 | 4914.0 | 0 | 70 | 2413 | 0 | 0.00 | 442368 |
| `insert-sequential` | medium | 4096 | 20000 | 13444.1 | 835 | 0 | 34125 | 0 | 0.00 | 3424256 |
| `commit-per-dirty-page` | medium | 4096 | 836 | 2398.6 | 0 | 0 | 2 | 837 | 1.43 | 0 |
| `seek-by-rowid` | medium | 4096 | 20000 | 456.3 | 0 | 0 | 0 | 0 | 0.00 | 3428352 |
| `range-scan-100-rows` | medium | 4096 | 2000 | 8503.5 | 0 | 0 | 0 | 0 | 0.00 | 3428352 |
| `insert-random` | medium | 4096 | 20000 | 15322.0 | 723 | 0 | 32716 | 0 | 0.00 | 6389760 |
| `replace` | medium | 4096 | 20000 | 19137.7 | 585 | 0 | 58614 | 0 | 0.00 | 8785920 |
| `delete-sequential` | medium | 4096 | 20000 | 7704.9 | 0 | 1419 | 48191 | 0 | 0.00 | 8785920 |
| `insert-sequential` | large | 4096 | 100000 | 15031.7 | 4177 | 0 | 171082 | 0 | 0.00 | 17113088 |
| `commit-per-dirty-page` | large | 4096 | 4178 | 3132.1 | 0 | 0 | 2 | 4179 | 1.43 | 0 |
| `seek-by-rowid` | large | 4096 | 20000 | 814.1 | 0 | 0 | 0 | 0 | 0.00 | 17117184 |
| `range-scan-100-rows` | large | 4096 | 2000 | 6856.9 | 0 | 0 | 0 | 0 | 0.00 | 17117184 |
| `insert-random` | large | 4096 | 20000 | 14555.7 | 718 | 0 | 32399 | 0 | 0.00 | 20058112 |
| `replace` | large | 4096 | 20000 | 18528.1 | 585 | 0 | 58579 | 0 | 0.00 | 22454272 |
| `delete-sequential` | large | 4096 | 20000 | 7337.6 | 0 | 1420 | 48193 | 0 | 0.00 | 22454272 |
| `insert-split-heavy` | structure | 512 | 20000 | 5484.9 | 6809 | 0 | 55678 | 0 | 0.00 | 3486720 |
| `delete-merge-heavy` | structure | 512 | 20000 | 5408.4 | 0 | 6809 | 77443 | 0 | 0.00 | 3487232 |
| `insert-split-heavy` | structure | 65536 | 20000 | 346483.6 | 6667 | 0 | 53331 | 0 | 0.00 | 436994048 |
| `delete-merge-heavy` | structure | 65536 | 20000 | 736968.4 | 0 | 6667 | 76665 | 0 | 0.00 | 437059584 |
| `insert-overflow` | 8192-byte-payload | 4096 | 4000 | 16330.5 | 8573 | 0 | 24568 | 0 | 0.00 | 35119104 |
| `read-overflow` | 8192-byte-payload | 4096 | 4000 | 918.6 | 0 | 0 | 0 | 0 | 0.00 | 35119104 |
| `delete-overflow` | 8192-byte-payload | 4096 | 4000 | 10446.1 | 0 | 8573 | 26862 | 0 | 0.00 | 35123200 |
| `insert-overflow` | 262144-byte-payload | 4096 | 200 | 157886.0 | 12829 | 0 | 26024 | 0 | 0.00 | 52551680 |
| `read-overflow` | 262144-byte-payload | 4096 | 200 | 25088.0 | 0 | 0 | 0 | 0 | 0.00 | 52551680 |
| `delete-overflow` | 262144-byte-payload | 4096 | 200 | 54405.5 | 0 | 12829 | 26142 | 0 | 0.00 | 52555776 |
| `incremental-vacuum` | 40000-rows | 4096 | 834 | 11101.1 | 834 | 0 | 7003 | 0 | 0.00 | 3440640 |
| `vacuum-copy-tree` | 40000-rows | 4096 | 20000 | 11826.3 | 836 | 0 | 34141 | 0 | 0.00 | 3424256 |
| `cache-lookup` | 4000-pages-resident | 512 | 200000 | 33.9 | 0 | 0 | 0 | 0 | 0.00 | 2048000 |
| `cache-lookup` | 32000-pages-resident | 512 | 200000 | 37.1 | 0 | 0 | 0 | 0 | 0.00 | 16384000 |
| `cache-lookup` | 160000-pages-resident | 512 | 200000 | 70.0 | 0 | 0 | 0 | 0 | 0.00 | 81920000 |
