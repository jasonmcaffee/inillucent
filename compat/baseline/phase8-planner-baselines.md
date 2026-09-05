# Planner, join, and schema baselines, phase 8

Platform: `windows-x86_64`

These are baselines, not results. Nothing here is a comparison against SQLite and no
number here should be read as one; they exist so that a later phase which changes the
cost model, the join enumerator or the write path has something to change it against.
They were taken only after the semantic gates were green, because a fast wrong answer
is not a measurement.

The allocation and instruction counts matter more than the clock. Wall time on a shared
machine moves with whatever else is running; the number of heap allocations a plan makes
and the number of bytecode instructions a query executes do not.

The four `plan-join-N` rows are the ones worth watching. Join-order enumeration is
exhaustive up to eight terms and greedy past it, so twelve and thirty-two are measuring
a different algorithm from two and five - and recording all four means a change that
moves the cut-over shows up as a step rather than as a slope.

| Workload | Scale | Ops | ns/op | Allocs/op | Bytes/op | VM ops |
|---|---|--:|--:|--:|--:|--:|
| `plan-join-2` | 2-terms | 2000 | 25363.0 | 669.0 | 31054 | 1 |
| `plan-join-5` | 5-terms | 2000 | 636822.1 | 15361.0 | 360458 | 1 |
| `plan-join-12` | 12-terms | 200 | 75419.5 | 1763.0 | 102200 | 1 |
| `plan-join-32` | 32-terms | 200 | 209963.0 | 4046.0 | 301384 | 1 |
| `run-join-2` | 2-terms | 50 | 35944.0 | 256.0 | 21208 | 518 |
| `run-join-5` | 5-terms | 50 | 70726.0 | 559.0 | 50584 | 821 |
| `run-join-12` | 12-terms | 50 | 153336.0 | 1266.0 | 119128 | 1528 |
| `correlated-exists` | 20000-rows | 20 | 10641795.0 | 80751.2 | 5888040 | 300014 |
| `correlated-scalar` | 20000-rows | 5 | 1111196400.0 | 12987904.0 | 819012309 | 8260764 |
| `in-subquery` | 20000-rows | 50 | 8548024.0 | 80771.8 | 5892326 | 240137 |
| `aggregate-grouped` | 20000-rows | 50 | 18114124.0 | 200786.9 | 18183552 | 360264 |
| `aggregate-having` | 20000-rows | 50 | 8784888.0 | 80810.4 | 7943460 | 200334 |
| `window-row-number` | 20000-rows | 20 | 24276670.0 | 200844.0 | 26159774 | 400011 |
| `window-frame` | 20000-rows | 20 | 29375565.0 | 440792.0 | 33837998 | 400011 |
| `sort-indexed` | 20000-rows | 50 | 11745628.0 | 120952.8 | 9406896 | 180509 |
| `sort-full` | 20000-rows | 20 | 12584220.0 | 120752.9 | 10500718 | 260008 |
| `distinct` | 20000-rows | 50 | 18860896.0 | 174765.0 | 14495244 | 197007 |
| `compound-union` | 20000-rows | 20 | 4081785.0 | 24444.0 | 2641366 | 32981 |
| `recursive-cte` | 20000-rows | 200 | 144949.0 | 528.0 | 50238 | 7513 |
| `analyze` | 35-objects | 3 | 12619866.7 | 164814.0 | 9894436 | 1 |
| `ddl-create-drop` | one-table | 200 | 2566803.0 | 4585.0 | 509710 | 1 |
| `vacuum` | 20000-rows | 2 | 682007250.0 | 10915479.0 | 1271235728 | 1 |
| `insert-plain` | one-row | 2000 | 1247797.5 | 159.7 | 66242 | 17 |
| `insert-triggered` | one-row | 2000 | 1270562.2 | 296.4 | 102264 | 34 |

## What the first run showed

**`DISTINCT` was quadratic, and is not any more.** It measured 447 ms against 18 ms for
the `GROUP BY` of the same shape, on *fewer* bytecode instructions - which is the tell:
time far out of line with the instruction count is time being spent somewhere the VM is
not looking. The distinct set was a linear scan of every row it had already kept, so a
`DISTINCT` over twenty thousand rows with seventeen thousand distinct values cost about
three hundred million comparisons. It now keeps an ordered index of what it has seen and
binary-searches it: 447 ms became 18.5 ms, on identical instruction and allocation
counts, which is how you can tell the change was the search structure and not the plan.
The ordering it searches by is proved in `inillucent-vm` to say `Equal` exactly where the
equality it replaced said `true`.

## What it still shows, and has not been changed

These are recorded rather than fixed. The phase's brief is correctness, and each of them
is a cost that is *explained* by the design rather than a defect in it - but each is also
where a later phase should look first.

- **`plan-join-5` costs more than `plan-join-12` and `plan-join-32` put together.** Six
reorderable terms is 720 permutations, each costed; thirteen and thirty-three are past
the eight-term cut-off and keep their written order for nothing. The step is the
intended behaviour and these four rows exist to show where it falls, but 15,361
allocations to plan one six-way join is a lot of allocation for a plan.
- **`correlated-scalar` allocates about thirteen thousand times per outer row.** A
correlated aggregate is re-evaluated per row by definition, so the *shape* is right;
what the number says is that re-evaluating it rebuilds rather than rewinds.
- **`vacuum` asks for 1.27 GB to rebuild a 1.7 MB database.** The copy back reads every
page of the rebuilt file into memory before writing any of it, which is what makes it a
single journalled transaction; streaming it a page at a time would need the journal to
be held open across the read, and that trade has not been made.
