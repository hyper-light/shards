"""Whether a TLS client's last writes reach its server when the client closes its socket
with the server's TLS 1.3 session tickets unread, as the syslog shipper did (D163 CI).

Each round: a TLS 1.3 server on loopback (2 session tickets, OpenSSL's default) reads until
the connection ends; the client writes "out\n" and "err\n" as two records, then ends its
connection as `mode` says:
  close     closes the socket at once (the shipper before the fix)
  nodelay   TCP_NODELAY on, then closes at once
  drain     shuts its write side, reads until the server's EOF, then closes
A mode prefixed "late-" first waits 50 ms after the handshake, so that the server's tickets
have come, unread, before the client writes and closes, as a shipper's run ends.
Prints how many rounds the server got both lines in, and how it saw the end.

PM M233. Run with a throwaway certificate, on macOS and in a Linux microVM:
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
      -keyout key.pem -out cert.pem -days 2 -subj /CN=localhost
  python3 probe.py cert.pem key.pem 200 late-close
  shards run --rm -v "$PWD:/probe:ro" python:3-alpine \
      python3 /probe/probe.py /probe/cert.pem /probe/key.pem 200 late-close
"""

import socket
import ssl
import sys
import threading
import time
from collections import Counter

cert, key, rounds, mode = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]


def serve(listener, ctx, out):
    conn, _ = listener.accept()
    s = ctx.wrap_socket(conn, server_side=True)
    data, end = b"", "eof"
    try:
        while True:
            b = s.recv(4096)
            if not b:
                break
            data += b
    except ConnectionResetError:
        end = "reset"
    except (ssl.SSLError, OSError) as e:
        end = type(e).__name__
    out.append((data, end))
    s.close()


got = Counter()
ends = Counter()
for _ in range(rounds):
    sctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    sctx.minimum_version = ssl.TLSVersion.TLSv1_3
    sctx.load_cert_chain(cert, key)
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    out = []
    t = threading.Thread(target=serve, args=(listener, sctx, out))
    t.start()
    cctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    cctx.check_hostname = False
    cctx.verify_mode = ssl.CERT_NONE
    raw = socket.create_connection(listener.getsockname())
    late = mode.startswith("late-")
    how = mode.removeprefix("late-")
    if how == "nodelay":
        raw.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    c = cctx.wrap_socket(raw, server_hostname="localhost")
    if late:
        time.sleep(0.05)
    c.sendall(b"out\n")
    c.sendall(b"err\n")
    if how == "drain":
        # The SSLSocket holds the descriptor now; its shutdown leaves TLS and shuts TCP's
        # write side, and its reads then read the socket's bytes as they come.
        c.shutdown(socket.SHUT_WR)
        while c.recv(4096):
            pass
    c.close()
    t.join()
    listener.close()
    data, end = out[0]
    got[data.decode(errors="replace")] += 1
    ends[end] += 1

print(mode, dict(got), dict(ends))
