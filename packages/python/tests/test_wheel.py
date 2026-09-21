"""The wheel's contents, checked against what a wheel has to contain.

Invariant: **the binaries and the library are in the package, and the driver can
find them.** A Python wheel that installs the Python and leaves the native part
out installs a package whose every call fails at import or at the first
statement, and the failure a person sees is a missing shared object rather than
"the wheel was built wrong".

The defect this suite was written beside (task-1932, H12): none of the language
packages ran in CI at all, so a wrapper could be broken for a whole release
without anything failing. These are the checks that are worth having for this
one - what is in the package, and that a round trip works against the binary
that is in it.

Run it with::

    python -m pytest packages/python/tests

or, with no pytest on the machine::

    python packages/python/tests/test_wheel.py
"""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
SOURCE = HERE.parent / "src"
sys.path.insert(0, str(SOURCE))

PROGRAMS = ("inillucent", "inillucent-shell", "inillucent-mcp", "inillucent-migrate")


def package() -> Path:
    """Returns the package directory inside the source tree."""
    return SOURCE / "inillucent"


def test_the_four_programs_are_in_the_package() -> None:
    """Every program the release ships is in `_bin`, or the wheel is incomplete."""
    binaries = package() / "_bin"
    assert binaries.is_dir(), f"{binaries} is missing, so the wheel carries no programs"
    suffix = ".exe" if os.name == "nt" else ""
    missing = [
        name for name in PROGRAMS if not (binaries / f"{name}{suffix}").is_file()
    ]
    assert not missing, (
        f"{', '.join(missing)} are not in {binaries}. Build them first: "
        "python packages/python/build.py"
    )


def test_the_driver_library_is_in_the_package() -> None:
    """The C ABI library is what `Database` loads, so it has to be beside it."""
    libraries = package() / "_lib"
    assert libraries.is_dir(), f"{libraries} is missing, so the driver has nothing to load"
    found = [
        entry
        for entry in libraries.iterdir()
        if entry.suffix in {".dll", ".so", ".dylib"}
        and "inillucent_driver_capi" in entry.name
    ]
    assert found, (
        f"no inillucent_driver_capi library in {libraries}; the wheel would install a "
        "driver that cannot open anything"
    )


def test_a_statement_round_trips_through_the_driver() -> None:
    """A table written through the driver is readable through the driver.

    The smallest end to end check there is, and the one that fails when the
    library in the wheel is the wrong architecture or a stale build: the import
    succeeds, the open succeeds, and the first statement does not.
    """
    from inillucent import Database  # noqa: PLC0415 - after sys.path is set

    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "probe.rdb"
        with Database(str(path)) as database:
            connection = database.connect()
            connection.execute(
                "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)"
            )
            connection.execute("INSERT INTO people VALUES (?1, ?2)", [1, "Ada"])
            rows = connection.execute("SELECT id, name FROM people", limit=200)
            assert rows.total == 1, f"one row was written and {rows.total} came back"
            assert rows.rows[0][1] == "Ada", f"the row came back as {rows.rows[0]!r}"


def staged() -> bool:
    """Whether ``build.py`` has staged the wheel's native half.

    ``_bin`` and ``_lib`` are gitignored: they are filled by
    ``packages/python/build.py`` from the archives a release builds, so on an
    ordinary checkout they are not there at all.

    **Every test in this file failed on every machine that had not staged a
    wheel**, which is every development machine - and a test that always fails
    is as useless as one that always passes, which is why nothing referenced
    this file. It skips now, the way the standard says a suite with a missing
    prerequisite skips, and `INILLUCENT_STRICT=1` turns that skip into a
    non-zero exit so a release run cannot read a skip as a pass.
    """
    return (package() / "_bin").is_dir() and (package() / "_lib").is_dir()


def main() -> int:
    """Runs every test in this file, for a machine with no pytest."""
    if not staged():
        print(
            "the wheel's native half is not staged: run `python packages/python/build.py` "
            "against a dist/ the release filled; skipping",
            file=sys.stderr,
        )
        return 1 if os.environ.get("INILLUCENT_STRICT") == "1" else 0
    failures = []
    for name, test in sorted(globals().items()):
        if not name.startswith("test_") or not callable(test):
            continue
        try:
            test()
            print(f"  ok   {name}")
        except Exception as failure:  # noqa: BLE001 - a runner reports everything
            failures.append(f"{name}: {failure}")
            print(f"  FAIL {name}")
    for failure in failures:
        print(f"\n{failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
