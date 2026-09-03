# SQL front end and read-only VM baselines, phases 5-6

Platform: `windows-x86_64`

These are baselines, not results. Nothing here is a comparison against SQLite and no
number here should be read as one; they exist so that a later phase which changes the
parser, the planner or the machine has something to change it against. They were taken
only after the semantic gates were green, because a fast wrong answer is not a
measurement.

The allocation and instruction counts matter more than the clock. Wall time on a shared
machine moves with whatever else is running; the number of heap allocations a parse makes
and the number of bytecode instructions a query executes do not, and they are what a
later change will actually have moved.

| Workload | Scale | Ops | ns/op | Allocs/op | Bytes/op | VM ops |
|---|---|--:|--:|--:|--:|--:|
| `lex` | representative | 20000 | 390.3 | 0.0 | 0 | 71 |
| `parse` | representative | 20000 | 3867.1 | 81.0 | 7267 | 25 |
| `prepare` | representative | 20000 | 66790.2 | 1858.0 | 63020 | 83 |
| `prepare-cached` | representative | 20000 | 163.5 | 3.0 | 1577 | 1 |
| `schema-load` | 104-objects | 200 | 376704.5 | 6840.0 | 615719 | 1 |
| `point-select-rowid` | 20000-rows | 5000 | 10019.9 | 8.0 | 608 | 12 |
| `point-select-index` | 20000-rows | 5000 | 11523.1 | 56.0 | 3235 | 20 |
| `range-select-rowid` | 20000-rows | 5000 | 488827.6 | 111.0 | 14612 | 1012 |
| `scan-count` | 20000-rows | 20 | 1257940.0 | 570.0 | 760264 | 40012 |
| `scan-project` | 20000-rows | 20 | 93467000.0 | 160558.8 | 11635600 | 200006 |
| `sort` | 20000-rows | 20 | 10501995.0 | 120677.5 | 9212767 | 180510 |
| `aggregate-grouped` | 20000-rows | 20 | 8136935.0 | 80593.8 | 7750002 | 200162 |
| `distinct` | 20000-rows | 20 | 6795220.0 | 100582.0 | 5757100 | 120024 |
