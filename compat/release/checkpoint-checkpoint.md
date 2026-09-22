# An explicit checkpoint, spread over commits and not

Platform `windows-x86_64`, 6000 single-row transactions per run, 3 runs per arm, write-ahead log at `synchronous=normal`. Each number is the median across runs of that run's own statistic, in microseconds.

| statistic | checkpoint every 100 commits | one checkpoint at the end |
|---|---:|---:|
| median commit | 1183.0 | 1189.1 |
| 90th percentile | 1311.9 | 1308.0 |
| 99th percentile | 4606.4 | 1598.5 |
| 99.9th percentile | 18142.3 | 2404.8 |
| **worst commit** | **22805.1** | **23733.9** |
| checkpoints run | 59 | 1 |
| log records | 18946 | 18377 |
| log bytes | 18396464 | 10906480 |
| total, all commits | 8179569 | 7463402 |

The log records and bytes are the controls: both arms write the same rows to the same file, so a difference there would mean an arm had changed the work rather than its distribution.

The finding this file reports is whichever arm's worst commit and tail percentiles are higher: an explicit checkpoint folds every dirty page still in the log into the file, so a checkpoint run mid-batch pays for everything since the last one, and running it more often trades a larger number of smaller pauses for a smaller number of larger ones.
