# agent-skills

Task-shaped instructions for an AI agent working with inillucent. One directory per job, each with a
`SKILL.md` in the usual convention — YAML frontmatter carrying a `name` and a `description`, then the
body.

Start at [`../AGENTS.md`](../AGENTS.md) if you do not know which of these you want. It is the front
door and it splits the two audiences: using inillucent, and changing it.

## Using inillucent

| skill | when to open it |
|---|---|
| [`inillucent-quickstart`](inillucent-quickstart/SKILL.md) | install it, make a database, run SQL, read the JSON, understand the exit codes |
| [`inillucent-query`](inillucent-query/SKILL.md) | explore and query a database somebody else built |
| [`inillucent-migrate`](inillucent-migrate/SKILL.md) | bring a SQLite file, a PostgreSQL or a MySQL database in, verified |
| [`inillucent-search`](inillucent-search/SKILL.md) | full-text and vector search: `VECTOR(N)`, HNSW, FTS5, hybrid |
| [`inillucent-embed`](inillucent-embed/SKILL.md) | put it in an application, from Rust, Python, Node, Go, PHP or C |
| [`inillucent-mcp`](inillucent-mcp/SKILL.md) | give an agent a database over MCP, safely |
| [`inillucent-troubleshoot`](inillucent-troubleshoot/SKILL.md) | it did something you did not expect |

## Changing inillucent

| skill | when to open it |
|---|---|
| [`inillucent-develop`](inillucent-develop/SKILL.md) | the contracts, the test runner, the command table, where a new test goes |

## Installing these as skills

**They are already where your agent looks.** A clone of this repository carries the same eight
skills in three places:

| where | who reads it |
|---|---|
| `agent-skills/<name>/SKILL.md` | the source of truth, and the tool-neutral copy to point anything else at |
| `.claude/skills/<name>/SKILL.md` | Claude Code, which reads a project's own `.claude/skills/` |
| `.agents/skills/<name>/SKILL.md` | Codex and the other adopters of the Agent Skills layout |

The last two are **generated copies**, written by `node tools/sync-skills.mjs` and checked byte for
byte by `cargo test -p inillucent-compat --test documentation`. Edit `agent-skills/` and run the
script; a copy that has drifted fails a build.

Copies rather than symlinks, and that is a Windows decision rather than a preference: this
repository is developed on Windows, and a checkout without `core.symlinks` turns a symlink into a
text file containing a path, which no agent follows.

`.agents/skills/` was the project-local path in Codex's Agent Skills documentation when this was
written, on **2026-09-14**. It is somebody else's product and it can move; if your agent reads a
different path, add it to `TARGETS` in `tools/sync-skills.mjs` and record the date you checked.

For anything else, point your agent's skill or instruction loader at `agent-skills/` — nothing there
is tool-specific, and every page is readable on its own as Markdown.

## The instruction file each agent reads

One document, `AGENTS.md`, and a pointer in each place an agent looks for one:

| file | who reads it |
|---|---|
| [`AGENTS.md`](../AGENTS.md) | the document. Codex reads it directly. |
| `CLAUDE.md` | Claude Code. One line: `@AGENTS.md`. |
| `GEMINI.md` | Gemini CLI. The same line. |
| `.cursor/rules/inillucent.mdc` | Cursor. Frontmatter and the same line. |

No content is duplicated, and `documentation.rs` fails on a pointer file that grows past twenty
lines or stops naming `AGENTS.md` - because a second copy of an instruction document is the one that
goes stale.
