# Agent skills for inillucent

Each folder here is a skill: a page of instructions an AI agent follows for one job with inillucent.
Each folder holds one `SKILL.md`. The file starts with YAML front matter that has a `name` and a
`description`, and the instructions follow.

If you do not know which skill you need, start at [`AGENTS.md`](../AGENTS.md). It separates using
inillucent from changing it.

## Skills for using inillucent

| Skill | Open it when you want to |
|---|---|
| [`inillucent-quickstart`](inillucent-quickstart/SKILL.md) | install inillucent, make a database, run SQL, read the JSON result and the exit codes |
| [`inillucent-query`](inillucent-query/SKILL.md) | explore and query a database somebody else built |
| [`inillucent-migrate`](inillucent-migrate/SKILL.md) | copy a SQLite file, a PostgreSQL database or a MySQL database into inillucent, with every table verified |
| [`inillucent-search`](inillucent-search/SKILL.md) | add keyword or vector search: `VECTOR(N)`, HNSW indexes, FTS5, hybrid search |
| [`inillucent-embed`](inillucent-embed/SKILL.md) | use inillucent inside an application from Rust, Python, Node, Go, PHP or C |
| [`inillucent-mcp`](inillucent-mcp/SKILL.md) | give an AI agent a database over MCP, with limits on what it can reach |
| [`inillucent-troubleshoot`](inillucent-troubleshoot/SKILL.md) | find out why inillucent did something you did not expect |

## Skill for changing inillucent

| Skill | Open it when you want to |
|---|---|
| [`inillucent-develop`](inillucent-develop/SKILL.md) | change this repository: the rules tests enforce, the test runner, the command table, where a new test goes |

## Where agents find these skills

A clone of this repository has the same eight skills in three places:

| Folder | Who reads it |
|---|---|
| `agent-skills/<name>/SKILL.md` | the source. Edit these. Point any other agent at this folder |
| `.claude/skills/<name>/SKILL.md` | Claude Code, which reads a project's `.claude/skills/` folder |
| `.agents/skills/<name>/SKILL.md` | Codex and other agents that use the Agent Skills folder layout |

The copies in `.claude/skills/` and `.agents/skills/` are written by `node tools/sync-skills.mjs`.
`cargo test -p inillucent-compat --test tooling documentation::` fails when a copy differs from its source by
one byte. So edit `agent-skills/`, then run the script.

The copies are plain files, not symbolic links. This repository is developed on Windows, and a
Windows checkout without `core.symlinks` turns a symbolic link into a text file that holds a path.
No agent follows that file.

`.agents/skills/` was the project folder named in Codex's Agent Skills documentation on 2026-09-14.
If your agent reads a different folder, add it to `TARGETS` in `tools/sync-skills.mjs` and write down
the date you checked.

To use the skills in every project, copy the folders into your agent's own skill folder, for example
`~/.claude/skills/` for Claude Code. Every page is plain Markdown and also reads correctly on its own.

## The instruction file each agent reads

The instructions live in one file, `AGENTS.md`. Every other agent's instruction file only points to
it:

| File | Who reads it | What it holds |
|---|---|---|
| [`AGENTS.md`](../AGENTS.md) | Codex, and every agent below through its pointer | the instructions |
| `CLAUDE.md` | Claude Code | the line `@AGENTS.md`, and the rule about ticket numbers |
| `GEMINI.md` | Gemini CLI | the line `@AGENTS.md`, and the rule about ticket numbers |
| `.cursor/rules/inillucent.mdc` | Cursor | front matter and the line `@AGENTS.md` |

`cargo test -p inillucent-compat --test tooling documentation::` fails when a pointer file grows past twenty
lines or stops naming `AGENTS.md`. A second copy of the instructions would go out of date.
