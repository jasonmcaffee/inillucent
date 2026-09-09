"""Record a real PostgreSQL or MySQL exchange, so a test can replay it offline.

Sits between `inillucent-remote` and a real server, copies every byte in both
directions, and writes the server's side out as a transcript the protocol tests
serve back over a loopback socket. That is what lets
`crates/inillucent-remote/tests/protocol.rs` pass on a clone of this repository
with no database installed, while still checking the client against bytes a real
server actually sent rather than against this project's idea of the protocol.

Usage:

    python tools/record-wire-transcript.py --listen 15432 --server 127.0.0.1:5439 \
        --out crates/inillucent-remote/tests/fixtures/postgres-scram.transcript

Then point the client at the listening port and run one migration. The
transcript is a text file, one record per line:

    S <hex>   bytes the server sent
    C <hex>   bytes the client sent

The replayer only needs the `S` lines; the `C` lines are kept so a person can
read what provoked each response, and so a future test can assert the client's
own bytes.
"""

import argparse
import socket
import threading


def pump(source, sink, tag, records, lock, done):
    """Copies one direction, recording every chunk.

    @param source - the socket to read
    @param sink - the socket to write
    @param tag - 'S' or 'C', which side these bytes came from
    @param records - the shared list of (tag, bytes)
    @param lock - guards `records`
    @param done - set when either direction closes
    """
    try:
        while not done.is_set():
            chunk = source.recv(65536)
            if not chunk:
                break
            with lock:
                records.append((tag, chunk))
            sink.sendall(chunk)
    except OSError:
        pass
    finally:
        done.set()
        for handle in (source, sink):
            try:
                handle.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", type=int, required=True)
    parser.add_argument("--server", required=True)
    parser.add_argument("--out", required=True)
    arguments = parser.parse_args()

    host, port = arguments.server.rsplit(":", 1)
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", arguments.listen))
    listener.listen(1)
    print(f"listening on 127.0.0.1:{arguments.listen}, forwarding to {arguments.server}")

    client, _ = listener.accept()
    upstream = socket.create_connection((host, int(port)))
    records = []
    lock = threading.Lock()
    done = threading.Event()
    threads = [
        threading.Thread(target=pump, args=(client, upstream, "C", records, lock, done)),
        threading.Thread(target=pump, args=(upstream, client, "S", records, lock, done)),
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    with open(arguments.out, "w", encoding="utf-8") as handle:
        for tag, chunk in records:
            handle.write(f"{tag} {chunk.hex()}\n")
    print(f"wrote {len(records)} records to {arguments.out}")


if __name__ == "__main__":
    main()
