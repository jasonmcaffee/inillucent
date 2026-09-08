"""The console scripts: find the bundled program and become it.

Each of the four is two lines of work - resolve the binary, hand the process
over - and the reason they are here rather than being ``scripts`` entries in the
wheel is that a console script written by pip is a Python file it can put on
``PATH`` on every platform, and a raw executable in ``scripts/`` is not: on
Windows pip does not make a ``.exe`` shim for one, so ``inillucent`` would
install and then not be a command.

``os.execv`` on POSIX and a subprocess on Windows, because Windows has no execv
that replaces the process - its ``os.execv`` starts a new one and returns, which
detaches the child from the console and breaks every pipeline the shell built.
The exit code is passed through either way, and it carries meaning: 3 is "the
engine has not built that".
"""

from __future__ import annotations

import os
import subprocess
import sys

from . import binary


def _become(program: str) -> None:
    """Run one of the four programs with this process's arguments.

    :param program: which program to run
    """
    try:
        path = str(binary(program))
    except FileNotFoundError as why:
        sys.stderr.write(f"{why}\n")
        raise SystemExit(1)
    arguments = sys.argv[1:]
    if os.name == "nt":
        finished = subprocess.run([path, *arguments])
        raise SystemExit(finished.returncode)
    os.execv(path, [path, *arguments])


def cli() -> None:
    """The ``inillucent`` command."""
    _become("inillucent")


def shell() -> None:
    """The ``inillucent-shell`` command."""
    _become("inillucent-shell")


def mcp() -> None:
    """The ``inillucent-mcp`` command."""
    _become("inillucent-mcp")


def migrate() -> None:
    """The ``inillucent-migrate`` command."""
    _become("inillucent-migrate")


if __name__ == "__main__":
    cli()
