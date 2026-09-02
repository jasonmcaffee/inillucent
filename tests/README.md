# Cross-crate test assets

These directories hold the assets the assurance program uses, kept out of the
crates so that a fixture is not owned by whichever crate happened to need it
first. Each is filled by the phase that needs it; an empty one is a phase that
has not arrived yet.

| directory | what lands here | phase |
|---|---|---|
| `conformance/` | semantic and negative-parity cases, in the harness's own case format | 5 onward |
| `crash/` | generated failure schedules and the state each one must recover to | 7 onward |
| `interop/` | databases written by SQLite and read by rust-db, and the reverse: cross-open, cross-write, checkpoint, backup | 3 onward |
| `workloads/` | deterministic benchmark and application traces, so a performance run is replayable | 14 |

The VFS conformance suite is not here: it is a library in `rustdb-vfs`, because
three implementations run it and the simulator has to be able to run it against
itself.
