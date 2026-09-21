"""Record a real MCP client's exchange with `inillucent-mcp`, for offline replay.

Sits between a client and the server as a stdio proxy, copies every line in both
directions, and writes both sides out as a transcript
`crates/inillucent-compat/tests/mcp_replay.rs` replays. That is what lets the
replay check the server against what a real client sent rather than against this
project's idea of what one sends - which is the gap 0.1.2 shipped through: the
release scripts sent a handshake the server refused, and running the release was
what found it.

Usage, as the client's server command:

    python tools/record-mcp-transcript.py \
        --out crates/inillucent-compat/tests/fixtures/mcp/claude-code.transcript \
        -- target/debug/inillucent-mcp --db app.rdb --root .

Then point the client at *this* script instead of at the server, and do the
session you want recorded. Every line is copied through unchanged, so the client
sees exactly the server it would have seen.

## The format

One record per line, in the order it crossed:

    C <json>   a line the client sent
    S <json>   a line the server sent

Both sides are kept. The replay sends the `C` lines and compares the `S` lines,
and a person reading the file can see what provoked each answer.

## What it refuses to record

A transcript whose database is outside the scratch directory. A recorded session
carries whatever the server answered, and a server pointed at a real database
answers with a real database's table names and rows - which is how a fixture
ends up holding somebody's data. `--allow-any-database` turns the check off for
a case where the path is known to be scratch under another name, and says so in
the transcript's header.
"""

import argparse
import io
import os
import subprocess
import sys
import threading


def looks_like_scratch(argv):
    """Whether the `--db` in a server command line is a scratch path.

    Scratch is `_agent_output/`, a temporary directory, or a path with `scratch`
    or `tmp` in it. Anything else is a database somebody might care about, and
    recording against one is how private rows reach a checked-in fixture.

    @param argv - the server command line
    """
    database = None
    for index, item in enumerate(argv):
        if item == "--db" and index + 1 < len(argv):
            database = argv[index + 1]
    if database is None:
        return True, "no --db in the server command"
    lowered = database.replace("\\", "/").lower()
    markers = ("_agent_output/", "/temp/", "/tmp/", "scratch", "target/")
    if any(marker in lowered for marker in markers):
        return True, database
    return False, database


def pump(source, sink, tag, records, lock):
    """Copies one direction a line at a time, recording every line.

    @param source - the stream to read
    @param sink - the stream to write
    @param tag - 'C' or 'S', which side the line came from
    @param records - the shared list of (tag, line)
    @param lock - guards `records`
    """
    try:
        for line in source:
            text = line.decode("utf-8", errors="replace").rstrip("\r\n")
            if text:
                with lock:
                    records.append((tag, text))
            sink.write(line)
            sink.flush()
    except (OSError, ValueError):
        pass
    finally:
        try:
            sink.close()
        except OSError:
            pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, help="where to write the transcript")
    parser.add_argument("--note", default="", help="one line about what this session was")
    parser.add_argument("--allow-any-database", action="store_true",
                        help="record even when the server is pointed at a database that is not "
                             "obviously scratch")
    parser.add_argument("server", nargs=argparse.REMAINDER,
                        help="the server command, after a --")
    args = parser.parse_args()

    argv = [item for item in args.server if item != "--"]
    if not argv:
        parser.error("no server command given; put it after a --")

    scratch, database = looks_like_scratch(argv)
    if not scratch and not args.allow_any_database:
        print("refusing to record against %s: it is not a scratch database, and a transcript "
              "carries whatever the server answered. Pass --allow-any-database if you are sure."
              % database, file=sys.stderr)
        return 2

    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    records = []
    lock = threading.Lock()
    upstream = threading.Thread(
        target=pump, args=(sys.stdin.buffer, child.stdin, "C", records, lock))
    downstream = threading.Thread(
        target=pump, args=(child.stdout, sys.stdout.buffer, "S", records, lock))
    upstream.start()
    downstream.start()
    upstream.join()
    downstream.join()
    child.wait()

    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    with io.open(args.out, "w", encoding="utf-8", newline="\n") as handle:
        handle.write("# Recorded by tools/record-mcp-transcript.py.\n")
        # The paths are deliberately not recorded: they are whichever machine
        # did the recording, and a transcript carrying one is a fixture the next
        # reader has to wonder about.
        handle.write("# The server: %s, on a scratch database.\n" % os.path.basename(argv[0]))
        if args.note:
            handle.write("# %s\n" % args.note)
        if not scratch:
            handle.write("# Recorded with --allow-any-database against %s.\n" % database)
        handle.write("#\n")
        handle.write("# C lines are what the client sent; S lines are what the server answered.\n")
        handle.write("# `mcp_replay.rs` sends the C lines and compares the S lines, with the\n")
        handle.write("# request id and every timing field masked.\n")
        with lock:
            for tag, text in records:
                handle.write("%s %s\n" % (tag, text))
    print("wrote %d lines to %s" % (len(records), args.out), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
