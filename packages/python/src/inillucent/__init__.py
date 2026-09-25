"""inillucent, from Python.

Two things arrive with ``pip install inillucent``, and they are different tools
for different jobs:

**The driver** — :class:`Database`, :class:`Connection`, :class:`Rows` — is the
real binding, over the C ABI, in process. Use it when you are writing an
application against the database::

    from inillucent import Database

    with Database("app.rdb") as database:
        connection = database.connect()
        connection.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)")
        connection.execute("INSERT INTO people VALUES (?1, ?2)", [1, "Ada"])
        rows = connection.execute("SELECT id, name FROM people", limit=200)
        print(rows.rows, rows.total)

**The command line** — :func:`run` — shells out to the ``inillucent`` binary
that ships in this wheel and hands back its result object. Use it for the things
the driver has no call for: ``describe``, ``import``, ``export``, ``dump``,
``backup``, the dot commands. Each call is a process, so it is the wrong tool
for a loop over a million rows::

    from inillucent import run

    print(run("describe", db="app.rdb", table="people")["ddl"])

The four programs are also on ``PATH`` after an install: ``inillucent``,
``inillucent-shell``, ``inillucent-mcp`` and ``inillucent-migrate``.

**Unsupported is its own exception.** :class:`Unsupported` means the engine has
not built that construct; it is *not* :class:`SyntaxError`-shaped and rewording
the statement will not help. Catching them together throws away the one
distinction this driver was designed around.

Threads: the engine is single threaded and one file is one buffer pool. Confine
a :class:`Database` and everything under it to one thread, or serialise every
call on it yourself. There is no lock inside.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any, Mapping, Sequence

__version__ = "0.1.9"

_HERE = Path(__file__).resolve().parent


def _bundled_library() -> Path | None:
    """Return the C ABI library that shipped in this wheel, if one did.

    A source install has no ``_lib`` directory, and in that case the driver's
    own search - ``INILLUCENT_DRIVER_LIB``, then the workspace's ``target/`` -
    is left to do its job. That is what lets somebody working in the repository
    ``pip install -e`` this package and get the library they just built.
    """
    for name in (
        "inillucent_driver_capi.dll",
        "libinillucent_driver_capi.dylib",
        "libinillucent_driver_capi.so",
    ):
        candidate = _HERE / "_lib" / name
        if candidate.is_file():
            return candidate
    return None


# Point the driver at the bundled library **before** importing it, because it
# loads the library at import time. Setting the variable rather than patching
# the module keeps `driver.py` a byte-for-byte copy of the reference binding in
# `drivers/bindings/python/`, which is the file `drivers/README.md` tells a
# binding author to read. A patched copy would be a second binding.
_library = _bundled_library()
if _library is not None and "INILLUCENT_DRIVER_LIB" not in os.environ:
    os.environ["INILLUCENT_DRIVER_LIB"] = str(_library)

from . import driver as _driver  # noqa: E402  (the environment has to be set first)

# Re-exported by name rather than with a star import, so that a symbol removed
# from the reference binding fails here at import time with the name in the
# message, rather than at the call site in somebody's application.
Database = _driver.Database
Connection = _driver.Connection
Transaction = _driver.Transaction
Rows = _driver.Rows
DriverError = _driver.DriverError
Unsupported = _driver.Unsupported
capabilities = _driver.capabilities
supports = _driver.supports
abi_version = _driver.abi_version

#: The base exception, under the name the rest of this package uses. `Error` and
#: `DriverError` are the same class; the second is the reference binding's own
#: name and is kept so code written from `drivers/README.md` runs unchanged.
Error = _driver.DriverError


def binary(program: str = "inillucent") -> Path:
    """Return the path of one of the four bundled programs.

    :param program: ``inillucent``, ``inillucent-shell``, ``inillucent-mcp`` or
        ``inillucent-migrate``
    """
    allowed = {"inillucent", "inillucent-shell", "inillucent-mcp", "inillucent-migrate"}
    if program not in allowed:
        raise ValueError(f"inillucent has no program called {program!r}")
    suffix = ".exe" if os.name == "nt" else ""
    candidate = _HERE / "_bin" / f"{program}{suffix}"
    if candidate.is_file():
        return candidate
    raise FileNotFoundError(
        f"{program} did not ship in this install of inillucent.\n"
        f"  Looked in {candidate.parent}.\n"
        f"  A source install carries no binaries: build them with\n"
        f"    cargo build --release -p inillucent-cli\n"
        f"  or install the wheel for your platform from PyPI."
    )


def run(
    command: str,
    db: str | os.PathLike[str] | None = None,
    **arguments: Any,
) -> dict[str, Any]:
    """Run one ``inillucent`` command and return its result object.

    The result is the command line's ``--output json`` contract: ``ok``,
    ``command``, ``columns``, ``rows``, ``total``, ``more``, ``changes``,
    ``last_insert_rowid``, ``elapsed_ms`` and ``text`` on success; ``ok``,
    ``status``, ``message`` and sometimes ``feature`` on failure.

    A refusal is **returned**, not raised, because the interesting failures here
    are answers - "no such table", "this construct is not built yet". Only a
    broken invocation raises.

    :param command: the verb, such as ``query`` or ``describe``
    :param db: the database file
    :param arguments: the named arguments that verb takes
    """
    argv: list[str] = [str(binary()), command, "--output", "json"]
    if db is not None:
        argv += ["--db", str(db)]
    for name, value in arguments.items():
        if value is None or value is False:
            continue
        flag = "--" + name.replace("_", "-")
        if value is True:
            argv.append(flag)
        elif isinstance(value, (list, tuple)):
            # `params` and `vector` are JSON arguments, and the command line
            # reads them as JSON - so they are serialised rather than joined.
            argv += [flag, json.dumps(list(value))]
        else:
            argv += [flag, str(value)]
    finished = subprocess.run(argv, capture_output=True, text=True)
    output = finished.stdout.strip()
    if output.startswith("{"):
        return json.loads(output)
    raise RuntimeError(
        f"inillucent {command} could not be run "
        f"(exit {finished.returncode}): {finished.stderr.strip() or output}"
    )


def query(
    sql: str,
    db: str | os.PathLike[str] | None = None,
    params: Sequence[Any] | None = None,
    limit: int | None = None,
) -> list[Mapping[str, Any]]:
    """Run a query through the command line and return rows as dictionaries.

    The shape most callers want from a script. For an application, open a
    :class:`Database` instead: this is a process per call.

    :param sql: the statement
    :param db: the database file
    :param params: the values for ``?1``, ``?2``, ...
    :param limit: how many rows to hand back
    """
    result = run("query", db=db, sql=sql, params=params, limit=limit)
    if not result.get("ok"):
        raise _refusal(result)
    names = [column["name"] for column in result["columns"]]
    return [
        dict(zip(names, (_decode_value(value) for value in row)))
        for row in result["rows"]
    ]


def _decode_value(value: Any) -> Any:
    """Return one cell of a result as the Python value it stands for.

    **Bytes went in and could not come back** (task-2066 §4.1.15). A blob left
    the command line as the string ``x'00ff'``, typed ``text`` in the column
    list, so nothing told it apart from a TEXT column holding that text - while
    the parameter encoder has always sent bytes as ``{"blob": "<hex>"}``. The
    two halves of the same grammar now agree, and ``bytes`` read out of one
    query bind straight into the next.

    This is the subprocess half of the package. :class:`Database` goes through
    the C ABI and has always had the type.

    Anything that is not an envelope is returned unchanged.

    :param value: one cell, as ``--output json`` rendered it
    """
    if isinstance(value, dict) and list(value) == ["blob"]:
        held = value["blob"]
        if isinstance(held, str):
            try:
                return bytes.fromhex(held)
            except ValueError:
                return value
    return value


def _refusal(result: Mapping[str, Any]) -> DriverError:
    """Return the exception one failed command answers with.

    **Every failed query used to raise ``TypeError`` (task-1979, D14).**
    ``Error`` is ``DriverError``, whose first argument is the numeric status, so
    ``Error(message)`` passed the message where the status belongs and left the
    message missing - and what an application saw for "no such table" was
    ``__init__() missing 1 required positional argument: 'message'``.

    The envelope carries the status as a *name*, which is what a caller reads,
    so it is turned back into the number the exception carries. A name this
    build does not know maps to ``INTERNAL`` rather than to nothing, because an
    exception with the wrong status is still better than a ``KeyError`` from
    inside the error path.

    :param result: the ``--output json`` object of a command that failed
    """
    named = str(result.get("status", "internal"))
    status = _STATUS_BY_NAME.get(named, _driver.INTERNAL)
    kind = Unsupported if status == _driver.UNSUPPORTED else DriverError
    return kind(
        status,
        str(result.get("message", "the command failed")),
        result.get("feature"),
        result.get("detail"),
        int(result.get("offset", -1)),
    )


#: Every status name the command line prints, back to the number the driver's
#: own exceptions carry. Built from the driver's table rather than written out,
#: so a status added there needs no second edit here.
_STATUS_BY_NAME = {name: status for status, name in _driver._STATUS_NAMES.items()}


__all__ = [
    "Database",
    "Connection",
    "Transaction",
    "Rows",
    "Error",
    "DriverError",
    "Unsupported",
    "capabilities",
    "supports",
    "abi_version",
    "binary",
    "run",
    "query",
    "__version__",
]
