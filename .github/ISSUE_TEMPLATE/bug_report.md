---
name: A wrong answer or a crash
about: The engine returned a wrong answer, or stopped
title: ''
labels: ''
assignees: ''
---

<!--
**Do not report a security problem here.** `SECURITY.md` says where to send it. A working exploit
in a public issue puts users at risk.

**Do not report a missing feature here.** Exit code 3, and the status `unsupported` from the driver
and MCP, mean the engine has not built that feature yet. Exit code 1 means a real failure. The codes
are separate so a script can tell them apart. `inillucent capabilities` lists what is built. If the
feature is not built yet, open a feature request instead.
-->

## The statement, and what it returned

<!--
The smallest schema and statement that shows the problem. Do not attach a production database,
because it holds someone's data.
-->

```sql

```

**What it returned:**

**What it should have returned, and how you know:**

<!--
If SQLite 3.53.4 returns something different for the same statement, paste both answers. The
project compares every answer against that version of SQLite, so this is the most useful kind of
report.
-->

## Version and platform

<!-- `inillucent version` prints the engine, dialect and driver versions. -->

- inillucent:
- operating system:
- installed from: <!-- the site installer, a release archive, a package manager, a source build -->

## Anything else

<!--
`inillucent stats` prints the page cache, the pool size and the page counts of the file, which
often explains a slow query.
-->
