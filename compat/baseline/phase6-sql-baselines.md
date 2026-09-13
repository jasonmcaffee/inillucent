# SQL front end and read-only VM baselines, phases 5-6

Platform: `windows-x86_64`

These are baselines, not results. Nothing here is a comparison against SQLite and no
number here should be read as one; they exist so that a later phase which changes the
parser, the planner or the machine has something to change it against. They were taken
only after the semantic gates were green, because a fast wrong answer is not a
measurement.

The allocation and row counts matter more than the clock. Wall time on a shared
machine moves with whatever else is running; the number of heap allocations a parse makes
and the number of rows a query produces do not, and they are what a
later change will actually have moved.

| Workload | Scale | Ops | ns/op | Allocs/op | Bytes/op | rows/op |
|---|---|--:|--:|--:|--:|--:|
| `lex` | representative | 20000 | 8337.4 | 0.0 | 0 | 71 |
| `parse` | representative | 20000 | 51248.4 | 81.0 | 7267 | 25 |
| `prepare` | representative | 20000 | 45112.5 | 69.0 | 403 | 1 |
| `prepare-cached` | representative | 20000 | 155.2 | 1.0 | 9 | 1 |
| `schema-load` | 104-objects | 200 | 19.0 | 0.0 | 0 | 1 |
| `point-select-rowid` | 20000-rows | 5000 | 8239.3 | 7.0 | 228 | 1 |
| `point-select-index` | 20000-rows | 5000 | 16712.4 | 17.0 | 567 | 1 |
| `range-select-rowid` | 20000-rows | 5000 | 34543.1 | 115.0 | 10202 | 100 |
| `scan-count` | 20000-rows | 20 | 209940.0 | 19.0 | 2440 | 1 |
| `scan-project` | 20000-rows | 20 | 13029460.0 | 40037.0 | 3679794 | 20000 |
| `sort` | 20000-rows | 20 | 35120.0 | 116.0 | 10490 | 100 |
| `aggregate-grouped` | 20000-rows | 20 | 15865495.0 | 40152.0 | 1020275 | 17 |
| `distinct` | 20000-rows | 20 | 13585010.0 | 40067.0 | 1011499 | 17 |
