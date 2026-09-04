# The automatic checkpoint, spread and not spread

Platform `windows-x86_64`, 6000 single-row transactions per run, 3 runs per arm, write-ahead log at `synchronous=normal`. Each number is the median across runs of that run's own statistic, in microseconds.

| statistic | spread over commits | all on one commit |
|---|---:|---:|
| median commit | 550.9 | 556.0 |
| 90th percentile | 817.5 | 830.7 |
| 99th percentile | 1457.6 | 1472.8 |
| 99.9th percentile | 2089.0 | 2088.5 |
| **worst commit** | **3200.3** | **22936.0** |
| frames appended | 19808 | 19808 |
| checkpoints run | 5698 | 5698 |
| frames checkpointed | 13198 | 13172 |
| total, all commits | 3668551 | 3658054 |

The total and the frame counters are the controls: both arms copy the same pages into the same file, so a difference there would mean the arm had changed the work rather than its distribution.

**The finding is that it makes no difference, and the counters say why.** The automatic checkpoint runs after nearly every commit once the log passes its threshold - about 5,700 times in 6,000 commits - because a passive checkpoint backfills without restarting the log, so the frame count stays above the threshold and each commit copies only the handful of frames the one before it added. There is no accumulated batch for a budget to spread. Every percentile agrees, and the single worst commit lands in whichever arm the machine was busiest during: across four runs it swapped sides twice, which is why it is reported last and carries no weight.

So the bound stays a tunable an application can reach and not a default. The lever was implemented and measured rather than assumed, and what it measured was zero.
