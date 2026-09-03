# Durable-write baselines, phase 7

Platform: `windows-x86_64`

Each workload writes 2000 rows, either one transaction per row (`autocommit`) or 200 rows per transaction (`batched`). Every run is on the real operating-system VFS: a memory VFS would make the sync free, and a benchmark of a durability mechanism whose syncs are free measures nothing about it.

These are baselines, not comparisons. Nothing here is measured against SQLite and no number here should be read as one. The wall-clock columns move with the machine and the filesystem; the journal and page counters do not, and they are what a later change to the commit path should be read against.

`Amp` counts the journal as well as the database, because the journal is written for the same rows and a figure that left it out would make the crash guarantee look free. `memory`/`off` is included as the floor a run without a crash guarantee reaches, and is never a durability result: the two modes are recorded as not crash-safe and cannot be quoted for one.

## What was changed, and what the numbers said

**The commit was rewriting the journal's header sector when it only had to rewrite its fields.** The header is written twice: once with the magic zeroed, so the file is not yet hot, and again at commit with the magic and the real record count. The second write was a full sector, and every byte of it past the twenty-eighth was already exactly what it was putting there.

Shrinking it to the fields changes nothing about the crash argument, and the argument is worth restating because it is what licenses the change: a torn write leaves a prefix of the new bytes and the rest of the old, the old bytes here are the *first* header, and the two versions differ only in the magic and the record count. Every mixture is therefore either not hot, or hot with a record count that is the real one or smaller - and a smaller one is safe because no database page has been written at the moment that count was the truth. Whether the padding is rewritten does not enter into it.

Measured on the autocommit insert, where the header dominates because each commit journals only two or three pages: journal bytes fell from 34,957 KB to 27,012 KB for the same 2,000 rows, and write amplification from 436 to 372. The batched workloads moved much less, which is the expected shape - one header per 200 rows instead of one per row. No wall-clock column moved outside run-to-run noise, and none is claimed.

The transaction boundaries and the durability level are identical on both sides of that change, and the whole crash matrix was re-run after it.

| Workload | Mode | Sync | Rows | Commits | ns/row | p50 us | p95 us | p99 us | Journal recs | Journal KB | Syncs | Page writes | Amp |
|---|---|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| `insert-autocommit` | delete | full | 2000 | 2000 | 1281092 | 1215.1 | 1532.8 | 1771.1 | 4730 | 27011.6 | 4000 | 4763 | 371.73 |
| `insert-batched` | delete | full | 2000 | 10 | 27975 | 1012.1 | 1292.5 | 1292.5 | 51 | 244.7 | 20 | 84 | 4.69 |
| `update-autocommit` | delete | full | 2000 | 2000 | 1624546 | 1600.4 | 2113.4 | 2393.1 | 4000 | 24085.9 | 4000 | 4000 | 323.49 |
| `update-batched` | delete | full | 2000 | 10 | 196533 | 1148.4 | 3885.5 | 3885.5 | 58 | 272.7 | 20 | 56 | 4.01 |
| `delete-autocommit` | delete | full | 2000 | 2000 | 1443217 | 1308.7 | 1727.7 | 2157.5 | 4163 | 24739.2 | 4000 | 4163 | 334.03 |
| `delete-batched` | delete | full | 2000 | 10 | 107359 | 1209.0 | 4155.5 | 4155.5 | 72 | 328.8 | 20 | 68 | 4.85 |
| `savepoint-rollback` | delete | full | 2000 | 10 | 20849 | 900.4 | 1176.1 | 1176.1 | 21 | 124.4 | 20 | 10 | 0.00 |
| `recovery` | delete | full | 1 | 0 | 11893300 | 11893.3 | 11893.3 | 11893.3 | 0 | 0.0 | 0 | 0 | 0.00 |
| `insert-autocommit` | delete | normal | 2000 | 2000 | 1373927 | 1244.7 | 1534.4 | 1902.1 | 4730 | 27011.6 | 4000 | 4763 | 371.73 |
| `insert-batched` | delete | normal | 2000 | 10 | 27843 | 1110.6 | 1248.7 | 1248.7 | 51 | 244.7 | 20 | 84 | 4.69 |
| `update-autocommit` | delete | normal | 2000 | 2000 | 1768437 | 1747.1 | 2180.9 | 2440.8 | 4000 | 24085.9 | 4000 | 4000 | 323.49 |
| `update-batched` | delete | normal | 2000 | 10 | 200164 | 1183.6 | 3436.6 | 3436.6 | 58 | 272.7 | 20 | 56 | 4.01 |
| `delete-autocommit` | delete | normal | 2000 | 2000 | 1427851 | 1311.9 | 1727.6 | 2007.2 | 4163 | 24739.2 | 4000 | 4163 | 334.03 |
| `delete-batched` | delete | normal | 2000 | 10 | 107338 | 1157.9 | 3980.8 | 3980.8 | 72 | 328.8 | 20 | 68 | 4.85 |
| `savepoint-rollback` | delete | normal | 2000 | 10 | 24319 | 964.9 | 1363.5 | 1363.5 | 21 | 124.4 | 20 | 10 | 0.00 |
| `recovery` | delete | normal | 1 | 0 | 9825500 | 9825.5 | 9825.5 | 9825.5 | 0 | 0.0 | 0 | 0 | 0.00 |
| `insert-autocommit` | truncate | full | 2000 | 2000 | 1026408 | 894.5 | 1545.7 | 2492.9 | 4730 | 27011.6 | 6000 | 4763 | 371.73 |
| `insert-batched` | truncate | full | 2000 | 10 | 28973 | 1054.1 | 1166.6 | 1166.6 | 51 | 244.7 | 30 | 84 | 4.69 |
| `update-autocommit` | truncate | full | 2000 | 2000 | 1508990 | 1399.0 | 2265.6 | 3330.0 | 4000 | 24085.9 | 6000 | 4000 | 323.49 |
| `update-batched` | truncate | full | 2000 | 10 | 194393 | 831.0 | 1089.1 | 1089.1 | 58 | 272.7 | 30 | 56 | 4.01 |
| `delete-autocommit` | truncate | full | 2000 | 2000 | 1152698 | 1061.1 | 1780.0 | 2503.7 | 4163 | 24739.2 | 6000 | 4163 | 334.03 |
| `delete-batched` | truncate | full | 2000 | 10 | 108404 | 1228.1 | 1433.4 | 1433.4 | 72 | 328.8 | 30 | 68 | 4.85 |
| `savepoint-rollback` | truncate | full | 2000 | 10 | 21485 | 825.3 | 921.9 | 921.9 | 21 | 124.4 | 30 | 10 | 0.00 |
| `recovery` | truncate | full | 1 | 0 | 10334900 | 10334.9 | 10334.9 | 10334.9 | 0 | 0.0 | 0 | 0 | 0.00 |
| `insert-autocommit` | persist | full | 2000 | 2000 | 4768582 | 4618.7 | 6213.7 | 7617.5 | 4730 | 27066.3 | 6000 | 4763 | 372.17 |
| `insert-batched` | persist | full | 2000 | 10 | 51850 | 1123.1 | 2051.9 | 2051.9 | 51 | 244.9 | 30 | 84 | 4.69 |
| `update-autocommit` | persist | full | 2000 | 2000 | 5325544 | 5189.2 | 6821.6 | 7903.1 | 4000 | 24140.6 | 6000 | 4000 | 323.93 |
| `update-batched` | persist | full | 2000 | 10 | 219890 | 1271.2 | 3617.4 | 3617.4 | 58 | 273.0 | 30 | 56 | 4.01 |
| `delete-autocommit` | persist | full | 2000 | 2000 | 4975170 | 4870.2 | 6428.6 | 7381.6 | 4163 | 24793.9 | 6000 | 4163 | 334.47 |
| `delete-batched` | persist | full | 2000 | 10 | 130984 | 1112.0 | 4032.4 | 4032.4 | 72 | 329.1 | 30 | 68 | 4.85 |
| `savepoint-rollback` | persist | full | 2000 | 10 | 46491 | 1098.3 | 1359.1 | 1359.1 | 21 | 124.7 | 30 | 10 | 0.00 |
| `recovery` | persist | full | 1 | 0 | 5746800 | 5746.8 | 5746.8 | 5746.8 | 0 | 0.0 | 0 | 0 | 0.00 |
| `insert-autocommit` | memory | off | 2000 | 2000 | 56923 | 44.5 | 115.9 | 179.5 | 4730 | 27011.6 | 0 | 4763 | 371.73 |
| `insert-batched` | memory | off | 2000 | 10 | 25912 | 103.4 | 155.9 | 155.9 | 51 | 244.7 | 0 | 84 | 4.69 |
| `update-autocommit` | memory | off | 2000 | 2000 | 540793 | 524.1 | 834.0 | 952.2 | 4000 | 24085.9 | 0 | 4000 | 323.49 |
| `update-batched` | memory | off | 2000 | 10 | 184037 | 44.4 | 73.2 | 73.2 | 58 | 272.7 | 0 | 56 | 4.01 |
| `delete-autocommit` | memory | off | 2000 | 2000 | 235616 | 258.4 | 304.8 | 421.6 | 4163 | 24739.2 | 0 | 4163 | 334.03 |
| `delete-batched` | memory | off | 2000 | 10 | 96782 | 62.6 | 137.0 | 137.0 | 72 | 328.8 | 0 | 68 | 4.85 |
| `savepoint-rollback` | memory | off | 2000 | 10 | 15718 | 26.6 | 65.2 | 65.2 | 21 | 124.4 | 0 | 10 | 0.00 |
| `recovery` | memory | off | 0 | 0 | 0 | 0.0 | 0.0 | 0.0 | 0 | 0.0 | 0 | 0 | 0.00 |
