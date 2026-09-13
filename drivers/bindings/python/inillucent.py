"""The inillucent driver, from Python.

This is the reference binding, and it is written to be **read** as much as run.
It exists to prove that ``drivers/README.md`` plus ``include/inillucent_driver.h``
are enough for somebody who has never seen this repository to write a binding -
a claim that is worth nothing until somebody has done it in a second language.

It therefore uses ``ctypes`` from the standard library and nothing else, so it
can be dropped into any Python 3.9+ and run, and so that nothing here is hidden
behind a package that does the interesting part.

The four things every binding has to get right, all visible below:

1. **Check the ABI major version at load** and refuse a mismatch by name
   (:func:`_load`). Calling a function whose signature has moved is the failure
   this prevents, and it does not announce itself.
2. **Own every handle**, and free it exactly once, in the host language's own
   resource idiom - here ``__enter__``/``__exit__`` and ``__del__``
   (:class:`Database`, :class:`Connection`, :class:`Rows`).
3. **Check the status after every call that takes an error out-parameter**,
   build the exception from it, and free the error in a ``finally`` so an
   exception cannot leak it (:func:`_check`).
4. **Map UNSUPPORTED to its own exception type** (:class:`Unsupported`), never
   to the general one. That is the whole point of this driver arriving in
   Python, and a binding that folds it into a generic ``DatabaseError`` has
   thrown the design away.

Threads: the engine is single threaded and one file is one buffer pool. Confine
a :class:`Database` and everything under it to one thread, or serialise every
call on it yourself. There is no lock inside.
"""

from __future__ import annotations

import ctypes
import os
import sys
from ctypes import (
    POINTER,
    c_char_p,
    c_double,
    c_int32,
    c_int64,
    c_size_t,
    c_uint8,
    c_uint32,
    c_uint64,
    c_void_p,
)
from typing import Any, Iterator, Optional, Sequence

__all__ = [
    "Database",
    "Connection",
    "Rows",
    "Transaction",
    "DriverError",
    "Unsupported",
    "capabilities",
    "supports",
    "version",
    "abi_version",
]

# The ABI this file was written against. Only the major has to match: a minor
# bump adds symbols, and a major bump moves one.
ABI_MAJOR = 1

# —— statuses, copied from inillucent_driver.h ————————————————————————

OK = 0
UNSUPPORTED = 1
SYNTAX = 2
NOT_FOUND = 3
CONSTRAINT = 4
READONLY = 5
BUSY = 6
INTERRUPTED = 7
CORRUPT = 8
IO = 9
FULL = 10
TOO_BIG = 11
INVALID_STATE = 12
INTERNAL = 13

_STATUS_NAMES = {
    OK: "ok",
    UNSUPPORTED: "unsupported",
    SYNTAX: "syntax",
    NOT_FOUND: "not_found",
    CONSTRAINT: "constraint",
    READONLY: "readonly",
    BUSY: "busy",
    INTERRUPTED: "interrupted",
    CORRUPT: "corrupt",
    IO: "io",
    FULL: "full",
    TOO_BIG: "too_big",
    INVALID_STATE: "invalid_state",
    INTERNAL: "internal",
}

# —— value kinds ————————————————————————————————————————————————————

NULL = 0
INTEGER = 1
REAL = 2
TEXT = 3
BLOB = 4

# —— open flags ——————————————————————————————————————————————————————

OPEN_CREATE = 0x0001
OPEN_READONLY = 0x0002
OPEN_DIAGNOSTICS = 0x0004

# —— capability states ————————————————————————————————————————————————

SUPPORT_NO = 0
SUPPORT_YES = 1
SUPPORT_PARTIAL = -1
SUPPORT_UNKNOWN = -2


class DriverError(Exception):
    """Something the driver refused.

    Carries everything the C error carried, because throwing away ``status`` and
    keeping only the message is what forces callers back to matching on prose.
    """

    def __init__(
        self,
        status: int,
        message: str,
        feature: Optional[str] = None,
        detail: Optional[str] = None,
        offset: int = -1,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.status_name = _STATUS_NAMES.get(status, f"status {status}")
        self.message = message
        self.feature = feature
        self.detail = detail
        self.offset = offset if offset >= 0 else None

    def __str__(self) -> str:
        said = f"{self.message} [{self.status_name}]"
        if self.offset is not None:
            said += f" at byte {self.offset}"
        return said


class Unsupported(DriverError):
    """The engine has not implemented the construct.

    **Its own type on purpose.** This engine is deliberately incomplete and
    refuses what it has not built rather than answering it wrongly, so an
    application needs to be able to say "this engine cannot do that yet" rather
    than "check your spelling". :attr:`DriverError.feature` names the construct.
    """


def _library_path() -> str:
    """Return where the shared library is.

    ``INILLUCENT_DRIVER_LIB`` wins when it is set; otherwise the workspace's own
    build output is tried, debug before release, because that is where it is
    during development and there is nothing installed system-wide to find.
    """
    named = os.environ.get("INILLUCENT_DRIVER_LIB")
    if named:
        return named
    if sys.platform == "win32":
        names = ["inillucent_driver_capi.dll", "inillucent_driver.dll"]
    elif sys.platform == "darwin":
        names = ["libinillucent_driver_capi.dylib", "libinillucent_driver.dylib"]
    else:
        names = ["libinillucent_driver_capi.so", "libinillucent_driver.so"]
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
    for profile in ("debug", "release"):
        for name in names:
            candidate = os.path.join(root, "target", profile, name)
            if os.path.isfile(candidate):
                return candidate
    raise OSError(
        "cannot find the inillucent driver library. Build it with\n"
        "  cargo build --manifest-path <repo>/Cargo.toml -p inillucent-driver-capi\n"
        "or set INILLUCENT_DRIVER_LIB to its path."
    )


def _declare(lib: ctypes.CDLL) -> None:
    """Give ctypes every signature.

    **Not optional.** ctypes defaults an undeclared return type to ``int``,
    which is 32 bits: an undeclared function returning a pointer has its top
    half silently cut off on a 64-bit build, and the crash is nowhere near the
    call. Declaring them all is the one thing a ctypes binding must not skip.
    """
    lib.inillucent_abi_version.restype = c_uint32
    lib.inillucent_version.restype = c_char_p

    lib.inillucent_capability_count.restype = c_size_t
    lib.inillucent_capability.argtypes = [
        c_size_t,
        POINTER(c_char_p),
        POINTER(c_int32),
        POINTER(c_char_p),
    ]
    lib.inillucent_capability.restype = c_int32
    lib.inillucent_supports.argtypes = [c_char_p]
    lib.inillucent_supports.restype = c_int32

    lib.inillucent_open.argtypes = [c_char_p, c_uint32, POINTER(c_void_p), POINTER(c_void_p)]
    lib.inillucent_open.restype = c_int32
    for name in ("inillucent_close", "inillucent_checkpoint", "inillucent_integrity_check"):
        getattr(lib, name).argtypes = [c_void_p, POINTER(c_void_p)]
        getattr(lib, name).restype = c_int32
    lib.inillucent_backup_to.argtypes = [c_void_p, c_char_p, POINTER(c_void_p)]
    lib.inillucent_backup_to.restype = c_int32
    lib.inillucent_path.argtypes = [c_void_p]
    lib.inillucent_path.restype = c_char_p

    lib.inillucent_connect.argtypes = [c_void_p, POINTER(c_void_p), POINTER(c_void_p)]
    lib.inillucent_connect.restype = c_int32
    lib.inillucent_conn_free.argtypes = [c_void_p]
    lib.inillucent_conn_free.restype = None
    lib.inillucent_execute.argtypes = [
        c_void_p,
        c_char_p,
        c_uint64,
        POINTER(c_void_p),
        POINTER(c_void_p),
    ]
    lib.inillucent_execute.restype = c_int32
    lib.inillucent_execute_batch.argtypes = [c_void_p, c_char_p, POINTER(c_void_p)]
    lib.inillucent_execute_batch.restype = c_int32
    lib.inillucent_last_insert_rowid.argtypes = [c_void_p]
    lib.inillucent_last_insert_rowid.restype = c_int64
    lib.inillucent_total_changes.argtypes = [c_void_p]
    lib.inillucent_total_changes.restype = c_int64
    lib.inillucent_in_transaction.argtypes = [c_void_p]
    lib.inillucent_in_transaction.restype = c_int32
    lib.inillucent_schema_cookie.argtypes = [c_void_p]
    lib.inillucent_schema_cookie.restype = c_uint64
    lib.inillucent_cancel.argtypes = [c_void_p, POINTER(c_void_p)]
    lib.inillucent_cancel.restype = c_int32

    lib.inillucent_prepare.argtypes = [c_void_p, c_char_p, POINTER(c_void_p), POINTER(c_void_p)]
    lib.inillucent_prepare.restype = c_int32
    lib.inillucent_stmt_free.argtypes = [c_void_p]
    lib.inillucent_stmt_free.restype = None
    lib.inillucent_bind_null.argtypes = [c_void_p, c_uint32]
    lib.inillucent_bind_null.restype = c_int32
    lib.inillucent_bind_int.argtypes = [c_void_p, c_uint32, c_int64]
    lib.inillucent_bind_int.restype = c_int32
    lib.inillucent_bind_real.argtypes = [c_void_p, c_uint32, c_double]
    lib.inillucent_bind_real.restype = c_int32
    lib.inillucent_bind_text.argtypes = [c_void_p, c_uint32, c_char_p, c_size_t]
    lib.inillucent_bind_text.restype = c_int32
    lib.inillucent_bind_blob.argtypes = [c_void_p, c_uint32, POINTER(c_uint8), c_size_t]
    lib.inillucent_bind_blob.restype = c_int32
    lib.inillucent_clear_bindings.argtypes = [c_void_p]
    lib.inillucent_clear_bindings.restype = None
    lib.inillucent_stmt_execute.argtypes = [
        c_void_p,
        c_uint64,
        POINTER(c_void_p),
        POINTER(c_void_p),
    ]
    lib.inillucent_stmt_execute.restype = c_int32

    lib.inillucent_rows_free.argtypes = [c_void_p]
    lib.inillucent_rows_free.restype = None
    for name in (
        "inillucent_rows_column_count",
        "inillucent_rows_count",
        "inillucent_rows_total",
    ):
        getattr(lib, name).argtypes = [c_void_p]
        getattr(lib, name).restype = c_size_t
    lib.inillucent_rows_more.argtypes = [c_void_p]
    lib.inillucent_rows_more.restype = c_int32
    lib.inillucent_rows_affected.argtypes = [c_void_p]
    lib.inillucent_rows_affected.restype = c_int64
    lib.inillucent_rows_elapsed_us.argtypes = [c_void_p]
    lib.inillucent_rows_elapsed_us.restype = c_uint64
    lib.inillucent_rows_tag.argtypes = [c_void_p]
    lib.inillucent_rows_tag.restype = c_char_p
    for name in ("inillucent_rows_column_name", "inillucent_rows_column_type"):
        getattr(lib, name).argtypes = [c_void_p, c_size_t]
        getattr(lib, name).restype = c_char_p
    lib.inillucent_value_type.argtypes = [c_void_p, c_size_t, c_size_t]
    lib.inillucent_value_type.restype = c_int32
    lib.inillucent_value_int.argtypes = [c_void_p, c_size_t, c_size_t]
    lib.inillucent_value_int.restype = c_int64
    lib.inillucent_value_real.argtypes = [c_void_p, c_size_t, c_size_t]
    lib.inillucent_value_real.restype = c_double
    lib.inillucent_value_bytes.argtypes = [c_void_p, c_size_t, c_size_t, POINTER(c_size_t)]
    lib.inillucent_value_bytes.restype = POINTER(c_uint8)

    lib.inillucent_txn_begin.argtypes = [c_void_p, POINTER(c_void_p), POINTER(c_void_p)]
    lib.inillucent_txn_begin.restype = c_int32
    lib.inillucent_txn_execute.argtypes = [
        c_void_p,
        c_char_p,
        POINTER(c_uint64),
        POINTER(c_void_p),
    ]
    lib.inillucent_txn_execute.restype = c_int32
    lib.inillucent_txn_commit.argtypes = [c_void_p, POINTER(c_void_p)]
    lib.inillucent_txn_commit.restype = c_int32
    lib.inillucent_txn_rollback.argtypes = [c_void_p]
    lib.inillucent_txn_rollback.restype = None

    lib.inillucent_error_status.argtypes = [c_void_p]
    lib.inillucent_error_status.restype = c_int32
    for name in (
        "inillucent_error_message",
        "inillucent_error_feature",
        "inillucent_error_detail",
    ):
        getattr(lib, name).argtypes = [c_void_p]
        getattr(lib, name).restype = c_char_p
    lib.inillucent_error_offset.argtypes = [c_void_p]
    lib.inillucent_error_offset.restype = c_int32
    lib.inillucent_error_free.argtypes = [c_void_p]
    lib.inillucent_error_free.restype = None


def _load() -> ctypes.CDLL:
    """Load the library and refuse an ABI whose major is not ours.

    Refusing here, by name, is the point: the alternative is calling a function
    whose signature has moved, which does not fail in a way anybody can read.
    """
    lib = ctypes.CDLL(_library_path())
    _declare(lib)
    reported = lib.inillucent_abi_version()
    major = reported // 1_000_000
    if major != ABI_MAJOR:
        raise OSError(
            f"this binding was written for inillucent ABI {ABI_MAJOR}.x and the library "
            f"reports {major}.{(reported // 1000) % 1000}.{reported % 1000}. "
            "Refusing rather than calling a function whose signature may have moved."
        )
    return lib


_LIB = _load()


def _raise(error: c_void_p) -> None:
    """Turn a C error into an exception and free it.

    The ``finally`` matters: the exception is built from the C error's strings,
    which are copied into Python strings before the error is freed, and a raise
    that happened before the free would leak one error per failure.
    """
    try:
        status = _LIB.inillucent_error_status(error)
        message = _text(_LIB.inillucent_error_message(error))
        feature = _text(_LIB.inillucent_error_feature(error))
        detail = _text(_LIB.inillucent_error_detail(error))
        offset = _LIB.inillucent_error_offset(error)
    finally:
        _LIB.inillucent_error_free(error)
    kind = Unsupported if status == UNSUPPORTED else DriverError
    raise kind(status, message or "", feature, detail, offset)


def _text(pointer: Optional[bytes]) -> Optional[str]:
    """Copy a C string into a Python string, or None for NULL.

    Copying is rule 2 in the header: what comes back points inside a handle
    that will be freed, so a binding that kept the pointer would keep a dangling
    one.
    """
    if pointer is None:
        return None
    return pointer.decode("utf-8", "replace")


def _check(status: int, error: c_void_p) -> None:
    """Raise when a call failed, using the error it produced.

    A non-zero status with no error is still a failure and still raises, because
    "it failed and said nothing" is not a reason to carry on.
    """
    if status == OK:
        return
    if error:
        _raise(error)
    raise DriverError(status, f"the call failed with {_STATUS_NAMES.get(status, status)}")


def abi_version() -> str:
    """Return the library's ABI version as ``major.minor.patch``."""
    reported = _LIB.inillucent_abi_version()
    return f"{reported // 1_000_000}.{(reported // 1000) % 1000}.{reported % 1000}"


def version() -> str:
    """Return what the driver calls itself."""
    return _text(_LIB.inillucent_version()) or ""


def capabilities() -> "list[dict[str, Any]]":
    """Return every capability the engine declares.

    **Ask this before you compose a statement, not after.** The engine is
    deliberately incomplete in places, and every row here is checked against the
    running engine by a test - in both directions, so a claim of support that
    fails and a claim of absence that now works both fail it.
    """
    out = []
    for nth in range(_LIB.inillucent_capability_count()):
        name = c_char_p()
        state = c_int32()
        note = c_char_p()
        status = _LIB.inillucent_capability(
            nth, ctypes.byref(name), ctypes.byref(state), ctypes.byref(note)
        )
        if status != OK:
            continue
        out.append(
            {
                "name": _text(name.value),
                "supported": state.value,
                "note": _text(note.value),
            }
        )
    return out


def supports(name: str) -> int:
    """Return whether the engine does something, by name.

    ``SUPPORT_UNKNOWN`` means this build has never heard of the capability, and
    should be treated as "no" rather than as "yes": one that was never declared
    was certainly never checked.
    """
    return _LIB.inillucent_supports(name.encode("utf-8"))


class Rows:
    """One materialised result.

    Every value is copied into Python on the way out, so this object stays
    usable after the C result is freed - which is what lets it be returned from
    a function and iterated later, the idiom Python callers expect.
    """

    def __init__(self, handle: c_void_p) -> None:
        try:
            count = _LIB.inillucent_rows_column_count(handle)
            self.columns = [
                _text(_LIB.inillucent_rows_column_name(handle, nth)) or "" for nth in range(count)
            ]
            self.column_types = [
                _text(_LIB.inillucent_rows_column_type(handle, nth)) or "" for nth in range(count)
            ]
            self.rows = [
                [self._value(handle, row, column) for column in range(count)]
                for row in range(_LIB.inillucent_rows_count(handle))
            ]
            self.total = _LIB.inillucent_rows_total(handle)
            self.more = bool(_LIB.inillucent_rows_more(handle))
            affected = _LIB.inillucent_rows_affected(handle)
            self.affected = None if affected < 0 else affected
            self.elapsed_us = _LIB.inillucent_rows_elapsed_us(handle)
            self.tag = _text(_LIB.inillucent_rows_tag(handle)) or ""
        finally:
            _LIB.inillucent_rows_free(handle)

    @staticmethod
    def _value(handle: c_void_p, row: int, column: int) -> Any:
        """Read one cell, as the kind it actually is."""
        kind = _LIB.inillucent_value_type(handle, row, column)
        if kind == NULL:
            return None
        if kind == INTEGER:
            return _LIB.inillucent_value_int(handle, row, column)
        if kind == REAL:
            return _LIB.inillucent_value_real(handle, row, column)
        length = c_size_t()
        pointer = _LIB.inillucent_value_bytes(handle, row, column, ctypes.byref(length))
        if not pointer:
            return None
        raw = bytes(bytearray(pointer[: length.value]))
        # Text is NOT NUL-terminated and may contain a NUL byte, which is why
        # the length is read rather than the string scanned.
        return raw.decode("utf-8") if kind == TEXT else raw

    def __len__(self) -> int:
        return len(self.rows)

    def __iter__(self) -> Iterator[Sequence[Any]]:
        return iter(self.rows)

    def __getitem__(self, nth: int) -> Sequence[Any]:
        return self.rows[nth]

    def __repr__(self) -> str:
        return f"<Rows {self.tag}: {len(self.rows)} of {self.total}>"


class Transaction:
    """One transaction, held while you decide whether to commit.

    **A handle rather than a pair of calls**, because the rule it serves is that
    a check on what a write *did* must happen before the ``COMMIT``: a
    postcondition tested afterwards is a report about something that has already
    happened rather than a guard against it.

    Used as a context manager it commits on a clean exit and rolls back on an
    exception, which is what a caller means either way.
    """

    def __init__(self, handle: c_void_p) -> None:
        self._handle = handle
        self.affected: "list[int]" = []

    def execute(self, sql: str) -> int:
        """Run one statement inside the transaction, returning rows changed.

        A failure rolls the whole transaction back before it raises, so a caller
        that stops on the first error has already undone everything.
        """
        changed = c_uint64()
        error = c_void_p()
        status = _LIB.inillucent_txn_execute(
            self._handle, sql.encode("utf-8"), ctypes.byref(changed), ctypes.byref(error)
        )
        _check(status, error)
        self.affected.append(changed.value)
        return changed.value

    def commit(self) -> None:
        """Commit. The handle is spent either way."""
        error = c_void_p()
        status = _LIB.inillucent_txn_commit(self._handle, ctypes.byref(error))
        handle, self._handle = self._handle, None
        try:
            _check(status, error)
        finally:
            _LIB.inillucent_txn_rollback(handle)

    def rollback(self) -> None:
        """Roll back and free."""
        if self._handle:
            _LIB.inillucent_txn_rollback(self._handle)
            self._handle = None

    def __enter__(self) -> "Transaction":
        return self

    def __exit__(self, kind, value, trace) -> bool:
        if kind is None:
            self.commit()
        else:
            self.rollback()
        return False

    def __del__(self) -> None:
        self.rollback()


class Connection:
    """One connection to a database.

    It holds a reference to its :class:`Database` so the database cannot be
    collected first. In a language with non-deterministic finalisation that is
    not a nicety: without it, the two could be finalised in either order, and
    one of those orders frees a database with a live connection on it.
    """

    def __init__(self, database: "Database", handle: c_void_p) -> None:
        self._database = database
        self._handle = handle

    def execute(self, sql: str, params: Sequence[Any] = (), limit: Optional[int] = None) -> Rows:
        """Run one statement and return everything it produced.

        ``limit`` caps the rows handed back, not the rows produced:
        :attr:`Rows.total` is exact either way, because the engine materialises
        and the count was taken rather than estimated.
        """
        capped = (1 << 64) - 1 if limit is None else limit
        if not params:
            rows = c_void_p()
            error = c_void_p()
            status = _LIB.inillucent_execute(
                self._handle, sql.encode("utf-8"), capped, ctypes.byref(rows), ctypes.byref(error)
            )
            _check(status, error)
            return Rows(rows)
        statement = self.prepare(sql)
        try:
            return statement.execute(params, limit)
        finally:
            statement.close()

    def execute_batch(self, sql: str) -> None:
        """Run several statements separated by semicolons, for their effect."""
        error = c_void_p()
        status = _LIB.inillucent_execute_batch(self._handle, sql.encode("utf-8"), ctypes.byref(error))
        _check(status, error)

    def prepare(self, sql: str) -> "Statement":
        """Compile a statement so it can be run more than once."""
        handle = c_void_p()
        error = c_void_p()
        status = _LIB.inillucent_prepare(
            self._handle, sql.encode("utf-8"), ctypes.byref(handle), ctypes.byref(error)
        )
        _check(status, error)
        return Statement(self, handle)

    def transaction(self) -> Transaction:
        """Open a transaction."""
        handle = c_void_p()
        error = c_void_p()
        status = _LIB.inillucent_txn_begin(self._handle, ctypes.byref(handle), ctypes.byref(error))
        _check(status, error)
        return Transaction(handle)

    @property
    def last_insert_rowid(self) -> int:
        return _LIB.inillucent_last_insert_rowid(self._handle)

    @property
    def total_changes(self) -> int:
        return _LIB.inillucent_total_changes(self._handle)

    @property
    def in_transaction(self) -> bool:
        return bool(_LIB.inillucent_in_transaction(self._handle))

    @property
    def schema_cookie(self) -> int:
        return _LIB.inillucent_schema_cookie(self._handle)

    def cancel(self) -> None:
        """Ask a running statement to stop.

        ``supports("cancel")`` reports ``SUPPORT_PARTIAL`` because the request
        is read at each scan leaf and result batch. Long scans, large results,
        and slow joins stop with ``INTERRUPTED``. One indivisible operator must
        finish before it can observe the request, so callers should not promise
        an immediate stop.
        """
        error = c_void_p()
        status = _LIB.inillucent_cancel(self._handle, ctypes.byref(error))
        _check(status, error)

    def close(self) -> None:
        if self._handle:
            _LIB.inillucent_conn_free(self._handle)
            self._handle = None

    def __enter__(self) -> "Connection":
        return self

    def __exit__(self, kind, value, trace) -> bool:
        self.close()
        return False

    def __del__(self) -> None:
        self.close()


class Statement:
    """A compiled statement and its bindings."""

    def __init__(self, connection: Connection, handle: c_void_p) -> None:
        self._connection = connection
        self._handle = handle

    def execute(self, params: Sequence[Any] = (), limit: Optional[int] = None) -> Rows:
        """Bind these values and run it."""
        _LIB.inillucent_clear_bindings(self._handle)
        for nth, value in enumerate(params, start=1):
            self._bind(nth, value)
        capped = (1 << 64) - 1 if limit is None else limit
        rows = c_void_p()
        error = c_void_p()
        status = _LIB.inillucent_stmt_execute(
            self._handle, capped, ctypes.byref(rows), ctypes.byref(error)
        )
        _check(status, error)
        return Rows(rows)

    def _bind(self, index: int, value: Any) -> None:
        """Bind one value, choosing the call by the Python type.

        ``bool`` is checked before ``int`` because it is a subclass of it, and
        binding ``True`` as text would be a silently different value.
        """
        if value is None:
            _LIB.inillucent_bind_null(self._handle, index)
        elif isinstance(value, bool):
            _LIB.inillucent_bind_int(self._handle, index, int(value))
        elif isinstance(value, int):
            _LIB.inillucent_bind_int(self._handle, index, value)
        elif isinstance(value, float):
            _LIB.inillucent_bind_real(self._handle, index, value)
        elif isinstance(value, str):
            raw = value.encode("utf-8")
            _LIB.inillucent_bind_text(self._handle, index, raw, len(raw))
        elif isinstance(value, (bytes, bytearray, memoryview)):
            raw = bytes(value)
            buffer = (c_uint8 * len(raw)).from_buffer_copy(raw) if raw else (c_uint8 * 0)()
            _LIB.inillucent_bind_blob(self._handle, index, buffer, len(raw))
        else:
            raise TypeError(
                f"cannot bind a {type(value).__name__}; the driver has NULL, integers, "
                "floats, text and bytes, and converting anything else would be this "
                "binding deciding what your value means"
            )

    def close(self) -> None:
        if self._handle:
            _LIB.inillucent_stmt_free(self._handle)
            self._handle = None

    def __enter__(self) -> "Statement":
        return self

    def __exit__(self, kind, value, trace) -> bool:
        self.close()
        return False

    def __del__(self) -> None:
        self.close()


class Database:
    """One open database file."""

    def __init__(
        self,
        path: str,
        create: bool = True,
        read_only: bool = False,
        diagnostics: bool = False,
    ) -> None:
        flags = 0
        if create:
            flags |= OPEN_CREATE
        if read_only:
            flags |= OPEN_READONLY
        if diagnostics:
            flags |= OPEN_DIAGNOSTICS
        handle = c_void_p()
        error = c_void_p()
        status = _LIB.inillucent_open(
            path.encode("utf-8"), flags, ctypes.byref(handle), ctypes.byref(error)
        )
        _check(status, error)
        self._handle = handle
        self._connections: "list[Connection]" = []

    def connect(self) -> Connection:
        """Open a connection."""
        handle = c_void_p()
        error = c_void_p()
        status = _LIB.inillucent_connect(self._handle, ctypes.byref(handle), ctypes.byref(error))
        _check(status, error)
        connection = Connection(self, handle)
        self._connections.append(connection)
        return connection

    @property
    def path(self) -> str:
        return _text(_LIB.inillucent_path(self._handle)) or ""

    def checkpoint(self) -> None:
        """Make everything written so far durable."""
        error = c_void_p()
        _check(_LIB.inillucent_checkpoint(self._handle, ctypes.byref(error)), error)

    def integrity_check(self) -> None:
        """Walk every tree and raise on the first thing that is wrong."""
        error = c_void_p()
        _check(_LIB.inillucent_integrity_check(self._handle, ctypes.byref(error)), error)

    def backup_to(self, path: str) -> None:
        """Copy the database, opening and checking the copy before returning."""
        error = c_void_p()
        _check(
            _LIB.inillucent_backup_to(self._handle, path.encode("utf-8"), ctypes.byref(error)),
            error,
        )

    def close(self) -> None:
        """Checkpoint and close.

        Every connection is closed first, because the C library refuses to close
        a database with connections still open - deliberately, since freeing it
        then would leave them pointing at freed memory.
        """
        if not self._handle:
            return
        for connection in self._connections:
            connection.close()
        self._connections.clear()
        error = c_void_p()
        status = _LIB.inillucent_close(self._handle, ctypes.byref(error))
        self._handle = None
        _check(status, error)

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, kind, value, trace) -> bool:
        self.close()
        return False

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            # A finaliser that raises during interpreter shutdown produces a
            # message nobody can act on and hides whatever was actually
            # happening. An explicit close() reports properly.
            pass
