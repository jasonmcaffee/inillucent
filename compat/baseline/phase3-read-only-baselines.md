# Read-only baselines, phases 2 and 3

Platform: `windows-x86_64`

These are baselines, not results. Nothing has been optimised, and nothing here is a
comparison against SQLite. They exist so that a later phase which changes a read path
has a number to change it against.

Every database was written by the pinned SQLite 3.53.4 shell at a 4096-byte page size,
with a sixteen-column table and one index on a text column.

| Scale | Rows | Operation | Iterations | ns/op | Pages read | Cache hits |
|---|--:|---|--:|--:|--:|--:|
| small | 1000 | `open` | 200 | 21892.5 | 0 | 0 |
| small | 1000 | `schema-load` | 100 | 20839.0 | 1 | 0 |
| small | 1000 | `page-read-cache-hit` | 200000 | 32.4 | 0 | 200001 |
| small | 1000 | `page-read-cache-miss` | 2000 | 23428.4 | 2000 | 0 |
| small | 1000 | `point-read-by-rowid` | 20000 | 1590.5 | 33 | 39967 |
| small | 1000 | `point-read-by-index` | 20000 | 4666.3 | 7 | 39993 |
| small | 1000 | `range-scan-100-rows` | 2000 | 5013.9 | 0 | 10341 |
| small | 1000 | `project-one-of-sixteen-columns` | 200000 | 133.7 | 0 | 0 |
| small | 1000 | `project-all-sixteen-columns` | 200000 | 451.4 | 0 | 0 |
| medium | 50000 | `open` | 200 | 27178.5 | 0 | 0 |
| medium | 50000 | `schema-load` | 100 | 20212.0 | 1 | 0 |
| medium | 50000 | `page-read-cache-hit` | 200000 | 31.9 | 0 | 200001 |
| medium | 50000 | `page-read-cache-miss` | 2000 | 18702.2 | 2000 | 0 |
| medium | 50000 | `point-read-by-rowid` | 20000 | 6977.9 | 733 | 59267 |
| medium | 50000 | `point-read-by-index` | 20000 | 7788.7 | 105 | 59895 |
| medium | 50000 | `range-scan-100-rows` | 2000 | 12366.9 | 3092 | 9556 |
| medium | 50000 | `project-one-of-sixteen-columns` | 200000 | 135.6 | 0 | 0 |
| medium | 50000 | `project-all-sixteen-columns` | 200000 | 476.7 | 0 | 0 |
| large | 500000 | `open` | 200 | 29247.0 | 0 | 0 |
| large | 500000 | `schema-load` | 100 | 19755.0 | 1 | 0 |
| large | 500000 | `page-read-cache-hit` | 200000 | 30.8 | 0 | 200001 |
| large | 500000 | `page-read-cache-miss` | 2000 | 18673.2 | 2000 | 0 |
| large | 500000 | `point-read-by-rowid` | 20000 | 7508.8 | 733 | 59267 |
| large | 500000 | `point-read-by-index` | 20000 | 8310.4 | 105 | 59895 |
| large | 500000 | `range-scan-100-rows` | 2000 | 12807.1 | 2961 | 9687 |
| large | 500000 | `project-one-of-sixteen-columns` | 200000 | 146.5 | 0 | 0 |
| large | 500000 | `project-all-sixteen-columns` | 200000 | 471.0 | 0 | 0 |

