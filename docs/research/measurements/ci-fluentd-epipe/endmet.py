#!/usr/bin/env python3
"""What a stream's write meets once its receiver has ended the connection, by transport
(loopback TCP, TLS over it, a unix stream socket), each socket with the platform's defaults
as shards' stream makes them (crates/shards/src/daemon/ship/stream.rs, Conn::open):

- blocked: the writer writes 16 MiB while its receiver reads 4,096 bytes and closes with the
  rest unread, as a receiver past its bound does (Fluent Bit's forward input, fw_conn.c
  fw_conn_event); what the write returns.
- after: the receiver reads all it was sent and closes (nothing unread); then the writer
  writes twice, 100 ms apart; what each write returns.
- after, unread: as after, but the receiver closes with 1 byte of the writer's unread.

    python3 absorbed.py   # the connection's absorption; this file its ends
    python3 endmet.py
"""
import errno, os, platform, socket, ssl, subprocess, sys, tempfile, threading, time

def name(e):
    if isinstance(e, OSError) and e.errno:
        return errno.errorcode.get(e.errno, str(e.errno))
    return type(e).__name__

def pair(kind, d):
    """A connected (writer, receiver) of `kind`."""
    if kind == "unix":
        path = os.path.join(d, "s")
        l = socket.socket(socket.AF_UNIX)
        l.bind(path)
        l.listen(1)
        w = socket.socket(socket.AF_UNIX)
        w.connect(path)
        r, _ = l.accept()
        l.close()
        os.unlink(path)
        return w, r
    l = socket.socket()
    l.bind(("127.0.0.1", 0))
    l.listen(1)
    w = socket.create_connection(l.getsockname())
    r, _ = l.accept()
    l.close()
    if kind == "tls":
        key, cert = os.path.join(d, "k.pem"), os.path.join(d, "c.pem")
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key,
                        "-out", cert, "-days", "1", "-subj", "/CN=localhost"],
                       check=True, capture_output=True)
        sctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        sctx.load_cert_chain(cert, key)
        cctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        cctx.load_verify_locations(cert)
        out = {}
        t = threading.Thread(target=lambda: out.setdefault("r", sctx.wrap_socket(r, server_side=True)))
        t.start()
        w = cctx.wrap_socket(w, server_hostname="localhost")
        t.join()
        r = out["r"]
    return w, r

def blocked(kind, d):
    w, r = pair(kind, d)
    def receive():
        r.recv(4096)
        r.close()
    t = threading.Thread(target=receive)
    t.start()
    try:
        w.sendall(b"L" * (16 << 20))
        got = "ok"
    except Exception as e:
        got = name(e)
    t.join()
    w.close()
    return got

def after(kind, d, unread):
    w, r = pair(kind, d)
    w.sendall(b"first")
    time.sleep(0.1)
    r.recv(4096 if not unread else 4)
    r.close()
    time.sleep(0.1)
    got = []
    for _ in range(2):
        try:
            w.sendall(b"next")
            got.append("ok")
        except Exception as e:
            got.append(name(e))
        time.sleep(0.1)
    w.close()
    return ", ".join(got)

print(f"host {platform.node()} {platform.system()} {platform.release()} {platform.machine()}, "
      f"Python {platform.python_version()}, {ssl.OPENSSL_VERSION}")
for kind in ("tcp", "tls", "unix"):
    with tempfile.TemporaryDirectory() as d:
        b = blocked(kind, d)
    with tempfile.TemporaryDirectory() as d:
        a = after(kind, d, False)
    with tempfile.TemporaryDirectory() as d:
        u = after(kind, d, True)
    print(f"{kind}: blocked: {b}; after: {a}; after, unread: {u}")
