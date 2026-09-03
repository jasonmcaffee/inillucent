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
| `recovery-crash.txt` | a power loss *during* the recovery of a power loss | the first cut point, the second, and which database the second recovery produced |
| `allocation.txt` | a memory failure at every allocation of a write | the allocation, and whether it was refused |

The header line of a campaign report carries the totals: how many cut points
were covered, how many runs had reported a commit before the power went, and
how many ended in damage the engine detected rather than served.

A campaign that reports fewer cut points than it used to has stopped testing
something. That is the failure mode these files exist to make visible: the
first version of this campaign reported 127 cut points while causing thirteen
crashes, because a crash armed at a read did nothing and the failpoint counter
was already past the low numbers before the run began.
