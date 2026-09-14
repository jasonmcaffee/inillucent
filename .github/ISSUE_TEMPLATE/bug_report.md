---
name: A wrong answer or a crash
about: The engine answered something it should not have, or stopped
title: ''
labels: ''
assignees: ''
---

<!--
**Not for a security problem.** `SECURITY.md` says where those go, and a working
exploit in a public issue puts users at risk to make a point.

**Not for a missing feature.** Exit code 3, and the status `unsupported` over the
driver and MCP, both mean "this engine has not built that" - deliberately a
different code from 1 so a script can branch on it. `inillucent capabilities`
enumerates what is built, and every row of it is checked against the running
engine in both directions. If the answer is "not yet", that is a feature request
rather than a bug.
-->

## The statement, and what it answered

<!--
The smallest schema and statement that shows it. A schema and a statement, not a
file from production - a database is somebody's data.
-->

```sql

```

**What it answered:**

**What it should have answered, and how you know:**

<!--
If the reference answers differently, say so and paste both. That is the
strongest form this report takes, because the whole repository is built around
comparing against a pinned SQLite 3.53.4.
-->

## Version and platform

<!-- `inillucent --version` prints the first three. -->

- inillucent:
- operating system:
- installed from: <!-- the site installer, a release archive, a source build -->

## Anything else

<!--
`inillucent diagnose` prints the limits, the page size and the VFS, and is often
the thing that explains it.
-->
