"""A fake PostgreSQL front door that answers an SSLRequest, for tests/transport.rs.

It exists so that the TLS half of `inillucent-remote` can be tested against a
real TLS implementation without a PostgreSQL server installed. It speaks
exactly as much of the protocol as the transport negotiation needs: read the
eight-byte SSLRequest, answer with one byte, and - in `accept` mode - wrap the
socket and complete a handshake.

It reports what it saw on standard error, because standard output carries the
one line the Rust side parses to learn which port it took. What it reports is
the point: a case asserts that the server received nothing but the request,
which is how "the client refused before it sent a password" is checked rather
than assumed.

Usage:  tls_server.py <accept|refuse|plain> <certificate> <key>
"""

import socket
import ssl
import sys


def main() -> int:
    """Serves exactly one connection and reports what arrived on it."""
    if len(sys.argv) < 4:
        print("usage: tls_server.py <accept|refuse|plain> <certificate> <key>", file=sys.stderr)
        return 2
    mode, certificate, key = sys.argv[1], sys.argv[2], sys.argv[3]

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    port = listener.getsockname()[1]
    # The one line the Rust side reads, flushed so it is not held in a buffer
    # while the test waits for it.
    print(f"port {port}", flush=True)

    listener.settimeout(30)
    try:
        client, _ = listener.accept()
    except OSError as error:
        print(f"accept failed: {error}", file=sys.stderr, flush=True)
        return 1
    client.settimeout(30)

    try:
        request = client.recv(8)
    except OSError as error:
        print(f"read failed: {error}", file=sys.stderr, flush=True)
        return 1
    print(f"received {len(request)} bytes: {request.hex()}", file=sys.stderr, flush=True)

    if mode == "plain":
        # Nothing was asked for; report whatever else arrives.
        report_rest(client)
        return 0

    if mode == "refuse":
        # 'N' is how a server with TLS turned off answers. A client that
        # continued after this would be sending its password in the clear.
        client.sendall(b"N")
        report_rest(client)
        return 0

    client.sendall(b"S")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certfile=certificate, keyfile=key)
    try:
        wrapped = context.wrap_socket(client, server_side=True)
    except (ssl.SSLError, OSError) as error:
        # A refusal by the *client* lands here, which is the expected outcome of
        # the untrusted and mismatched cases.
        print(f"handshake refused by the client: {error}", file=sys.stderr, flush=True)
        return 0
    print("handshake ok", file=sys.stderr, flush=True)
    report_rest(wrapped)
    return 0


def report_rest(sock) -> None:
    """Reads whatever else the client sends and reports it.

    @param sock: the connected socket, plain or wrapped
    """
    sock.settimeout(3)
    try:
        while True:
            more = sock.recv(4096)
            if not more:
                break
            print(f"then {more!r}", file=sys.stderr, flush=True)
    except OSError:
        pass
    try:
        sock.close()
    except OSError:
        pass


if __name__ == "__main__":
    sys.exit(main())
