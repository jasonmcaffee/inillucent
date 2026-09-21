"""Builds tests/workloads/nikaya/statements.sql from Nikaya's own source.

Usage:

    python tools/extract-nikaya-workload.py                 # rewrite the workload file
    python tools/extract-nikaya-workload.py --check         # say whether it is stale, write nothing
    python tools/extract-nikaya-workload.py --out <path>    # write somewhere else
    python tools/extract-nikaya-workload.py --nikaya <dir>  # a checkout somewhere else

Statements only. Nikaya's data is private mail, so what is carried across is the
text of every statement the server prepares, plus a parameter value per
placeholder inferred from the column the placeholder stands for.

`--check` exits 0 when the checked-in file is what this would write, 1 when it
is not, and 2 when the Nikaya checkout is not on this machine - which is how
`crates/inillucent-compat/tests/workload_freshness.rs` tells "stale" from
"cannot tell". A copy goes stale; something has to notice.
"""
import argparse
import io
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_NIKAYA = r"C:/jason/dev/nikaya/server"
DEFAULT_OUT = os.path.join(HERE, "tests", "workloads", "nikaya", "statements.sql")

START = re.compile(r'^\s*(SELECT|INSERT|UPDATE|DELETE|WITH|CREATE|ALTER|DROP|REPLACE)\b', re.I)
TEMPLATE = re.compile(r"\{[A-Za-z_][A-Za-z0-9_]*\}")

# `schema_migration` is the one table Nikaya creates outside a migration file:
# `db.rs` runs it before it can read the ledger that says which migrations have
# run. It is carried here so the replay builds the same schema the server does.
LEDGER = """CREATE TABLE IF NOT EXISTS schema_migration (
  name       TEXT PRIMARY KEY,
  applied_at INTEGER NOT NULL
);"""

# The value bound for each storage class. Representative rather than random: the
# seed in `story_workload_replay.rs` writes rows carrying exactly these, so a
# predicate `= ?1` matches something rather than nothing.
SAMPLE = {"int": 1, "text": "row-0001", "vector": "[0.1,0.2,0.3,0.4]"}


def literals(src):
    """Every SQL statement literal in the server, with its file and line."""
    found = []
    for base, _dirs, files in os.walk(src):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(base, name)
            lines = io.open(path, encoding="utf-8", errors="replace").read().split("\n")
            i = 0
            while i < len(lines):
                line = lines[i]
                for opener in ('r#"', '"'):
                    idx = line.find(opener)
                    if idx < 0:
                        continue
                    rest = line[idx + len(opener):]
                    if not START.match(rest):
                        continue
                    closer = '"#' if opener == 'r#"' else '"'
                    body = []
                    if closer in rest:
                        body.append(rest[:rest.index(closer)])
                        j = i
                    else:
                        body.append(rest)
                        j = i + 1
                        while j < len(lines) and closer not in lines[j]:
                            body.append(lines[j])
                            j += 1
                        if j < len(lines):
                            body.append(lines[j][:lines[j].index(closer)])
                    statement = "\n".join(body).strip()
                    rel = os.path.relpath(path, src).replace("\\", "/")
                    found.append((rel, i + 1, statement))
                    i = j
                    break
                i += 1
    return found


def is_a_template(statement):
    """Whether the literal is a `format!` template rather than a statement.

    Nikaya builds several statements by interpolating a column list, a table
    name or a key - `SELECT {CHUNK_COLUMNS} {CHUNK_JOINS} WHERE ...` and the
    `staged.{table}` paging in `services/adopt.rs`. What reaches the engine is
    whatever the interpolation produced, and the literal is not SQL: running it
    would assert that the engine refuses a brace. They are counted and named in
    the header rather than silently dropped.
    """
    return bool(TEMPLATE.search(statement))


def schema_text(migrations):
    """The migrations, in order, each with its file name."""
    return [(name, io.open(os.path.join(migrations, name), encoding="utf-8").read().strip())
            for name in sorted(os.listdir(migrations))]


def kind(declared):
    """The storage class a declared type means here."""
    declared = declared.upper()
    if declared.startswith("INTEGER"):
        return "int"
    if declared.startswith("TEXT"):
        return "text"
    if declared.startswith("VECTOR"):
        return "vector"
    return None


def columns_by_table(migrations):
    """table -> {column: storage class}, read from the migrations."""
    tables = {}
    current = None
    for _name, text in [("db.rs", LEDGER)] + schema_text(migrations):
        for raw in text.split("\n"):
            line = raw.strip().rstrip(",")
            if line.startswith("--") or not line:
                continue
            made = re.match(r"CREATE TABLE(?: IF NOT EXISTS)? ([a-z_]+)", line, re.I)
            if made:
                current = made.group(1)
                tables.setdefault(current, {})
                continue
            altered = re.match(r"ALTER TABLE ([a-z_]+) ADD COLUMN ([a-z_]+) ([A-Z()0-9]+)",
                               line, re.I)
            if altered:
                tables.setdefault(altered.group(1), {})[altered.group(2)] = kind(altered.group(3))
                continue
            if line.startswith(")"):
                current = None
                continue
            if current is None:
                continue
            parts = line.split()
            if len(parts) < 2:
                continue
            column, declared = parts[0], parts[1].upper()
            if not re.match(r"^[a-z_][a-z0-9_]*$", column):
                continue
            if column.upper() in ("UNIQUE", "PRIMARY", "CHECK", "FOREIGN", "REFERENCES"):
                continue
            guess = kind(declared)
            if guess:
                tables[current][column] = guess
    return tables


def tables_named(statement, known):
    """Every table the statement names, in the order it names them."""
    flat = " ".join(statement.split())
    named = []
    for match in re.finditer(r"\b(?:FROM|JOIN|INTO|UPDATE)\s+([a-z_][a-z0-9_]*)", flat, re.I):
        name = match.group(1).lower()
        if name in known and name not in named:
            named.append(name)
    return named


def tidy(statement):
    """Re-indents a statement lifted out of Rust, which carries the call's indent."""
    lines = [line.rstrip() for line in statement.split("\n")]
    body = [line for line in lines[1:] if line.strip()]
    if body:
        common = min(len(line) - len(line.lstrip()) for line in body)
        lines = [lines[0]] + [line[common:] if line.strip() else "" for line in lines[1:]]
    return "\n".join(lines).strip()


def parameters(statement, tables):
    """One value per `?N`, from the column each placeholder stands for."""
    flat = " ".join(statement.split())
    highest = 0
    for match in re.finditer(r"\?(\d+)", flat):
        highest = max(highest, int(match.group(1)))
    if highest == 0:
        return [], []

    lookup = {}
    for table in reversed(tables_named(statement, tables)):
        lookup.update(tables.get(table, {}))

    values = {}

    # `INSERT INTO t (a, b, c) VALUES (?1, ?2, ?3)` binds by position in the
    # column list, which is the only place the column is written.
    insert = re.search(
        r"INSERT(?:\s+OR\s+\w+)?\s+INTO\s+([a-z_]+)\s*\(([^)]*)\)\s*VALUES\s*\(([^)]*)\)",
        flat, re.I)
    if insert:
        listed = [name.strip().lower() for name in insert.group(2).split(",")]
        supplied = [value.strip() for value in insert.group(3).split(",")]
        columns = tables.get(insert.group(1).lower(), {})
        for column, value in zip(listed, supplied):
            slot = re.match(r"^\?(\d+)$", value)
            if slot and column in columns:
                values[int(slot.group(1))] = SAMPLE[columns[column]]

    # Everywhere else the column is beside the placeholder.
    for match in re.finditer(
            r"([A-Za-z_][A-Za-z0-9_.]*)\s*(?:=|>=|<=|<>|!=|>|<)\s*\?(\d+)", flat):
        column = match.group(1).split(".")[-1].lower()
        slot = int(match.group(2))
        if slot not in values and column in lookup:
            values[slot] = SAMPLE[lookup[column]]

    # `COALESCE(column, ?n)` stands for the column beside it, which the
    # comparison above does not see because there is no operator.
    for match in re.finditer(r"COALESCE\(\s*([A-Za-z_][A-Za-z0-9_.]*)\s*,\s*\?(\d+)", flat, re.I):
        column = match.group(1).split(".")[-1].lower()
        slot = int(match.group(2))
        if slot not in values and column in lookup:
            values[slot] = SAMPLE[lookup[column]]

    # A count is a count whatever precedes it.
    for match in re.finditer(r"\b(?:LIMIT|OFFSET)\s+\?(\d+)", flat, re.I):
        values[int(match.group(1))] = 25

    unresolved = [slot for slot in range(1, highest + 1) if slot not in values]
    return [values.get(slot, "row-0001") for slot in range(1, highest + 1)], unresolved


HEADER = '''-- Nikaya's own statements, so a consumer's corpus is something the suite runs.
--
-- Built by `tools/extract-nikaya-workload.py` from `%s`:
-- every SQL statement literal in `src/`, de-duplicated, with the file and line
-- it came from, and the schema its migrations build.
--
-- ## Why the statements and not the data
--
-- Because the escape that prompted this was a *refusal*. A compound `SELECT`
-- used as a derived table was declined by the binder, and a document view
-- answered HTTP 500 - and no amount of the right data would have found it,
-- because the statement never compiled. What was missing was the statement.
--
-- Nikaya's data is private mail and none of it is here. `story_workload_replay.rs`
-- builds rows against this schema with values it chooses, and the `-- params:`
-- line under each statement is one value per placeholder, taken from the column
-- the placeholder stands for, so a predicate matches a row rather than nothing.
-- A `-- inferred:` line under a statement names the placeholders that stand
-- beside no column this schema declares; those are bound as text.
--
-- ## The format
--
-- Two sections, each opened by a line that is nothing but its marker. The
-- schema section is the migrations, concatenated, and is what the replay builds
-- before it runs anything. The statements section is one block per statement: a
-- statement line naming its source, a params line holding a JSON array, and the
-- SQL, ending in a semicolon.
--
-- A statement the engine refuses is a failure unless its source is named in
-- `allow.list` beside this file with a ticket key - the same arrangement
-- `differential_part8.rs` has, and for the same reason: a list that only grows
-- is a list of things nobody will take off it, so an allow listed statement
-- that starts working fails too.
--
-- ## What is not here
--
-- %d statements are carried. %d literals are not, because they are `format!`
-- templates rather than statements: Nikaya interpolates a column list, a table
-- name or a key into them, and what reaches the engine is whatever the
-- interpolation produced. Running the literal would assert that the engine
-- refuses a brace. They are:
--
%s
--
-- ## Regenerating
--
--     python tools/extract-nikaya-workload.py
--
-- When Nikaya adds a statement this file goes stale, and
-- `workload_freshness::the_workload_matches_the_consumers_source` says so on a
-- machine that has the Nikaya checkout and prints `; skipping` on one that does
-- not.

'''


def render(nikaya):
    """The whole workload file, as text."""
    src = os.path.join(nikaya, "src")
    migrations = os.path.join(nikaya, "migrations", "inillucent")
    tables = columns_by_table(migrations)

    seen = set()
    rows = []
    for rel, line, statement in literals(src):
        key = " ".join(statement.split())
        if key in seen:
            continue
        seen.add(key)
        rows.append((rel, line, tidy(statement)))

    templates = [row for row in rows if is_a_template(row[2])]
    rows = [row for row in rows if not is_a_template(row[2])]

    out = [HEADER % (nikaya.replace("\\", "/"), len(rows), len(templates),
                     "\n".join("--   %s:%d" % (rel, line) for rel, line, _ in templates))]
    out.append("-- section: schema\n\n")
    out.append("-- from src/db.rs, which creates the migration ledger before it can read it\n")
    out.append(LEDGER + "\n\n")
    for name, text in schema_text(migrations):
        out.append("-- from migrations/inillucent/%s\n" % name)
        out.append(text + "\n\n")
    out.append("-- section: statements\n\n")
    for rel, line, statement in rows:
        values, unresolved = parameters(statement, tables)
        out.append("-- statement: %s:%d\n" % (rel, line))
        out.append("-- params: %s\n" % json.dumps(values))
        if unresolved:
            out.append("-- inferred: slot %s stands beside no column this schema declares\n"
                       % ", ".join(str(slot) for slot in unresolved))
        out.append(statement.rstrip().rstrip(";") + ";\n\n")
    return "".join(out), len(rows), len(templates)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="say whether the checked-in file is stale; write nothing")
    parser.add_argument("--out", default=DEFAULT_OUT, help="where to write it")
    parser.add_argument("--nikaya", default=os.environ.get("NIKAYA_ROOT", DEFAULT_NIKAYA),
                        help="the Nikaya server checkout")
    args = parser.parse_args()

    if not os.path.isdir(os.path.join(args.nikaya, "src")):
        print("the Nikaya checkout is not at %s; skipping" % args.nikaya)
        return 2

    text, statements, templates = render(args.nikaya)
    if args.check:
        current = io.open(args.out, encoding="utf-8").read() if os.path.isfile(args.out) else ""
        if current.replace("\r\n", "\n") == text:
            print("%s is what %d statements and %d templates say it should be"
                  % (args.out, statements, templates))
            return 0
        print("%s no longer matches Nikaya's source. Run:\n"
              "    python tools/extract-nikaya-workload.py" % args.out)
        return 1

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with io.open(args.out, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
    print("wrote %d statements to %s, skipping %d templates" % (statements, args.out, templates))
    return 0


if __name__ == "__main__":
    sys.exit(main())
