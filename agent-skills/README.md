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

They are plain directories, so they work wherever a `SKILL.md` tree does. For Claude Code:

```sh
# one skill
ln -s "$PWD/agent-skills/inillucent-migrate" ~/.claude/skills/inillucent-migrate

# or all of them
for skill in agent-skills/*/; do
  ln -s "$PWD/$skill" ~/.claude/skills/"$(basename "$skill")"
done
```

On Windows, `New-Item -ItemType Junction`. For anything else, point your agent's skill or
instruction loader at this directory — nothing here is tool-specific, and every page is readable on
its own as Markdown.
