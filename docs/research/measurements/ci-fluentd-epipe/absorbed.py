#!/usr/bin/env python3
"""How many bytes a stream connection takes from its writer before the writer blocks, where
its receiver reads once (4,096 bytes, as the ship tests' receivers read) and then nothing: over
loopback TCP and a unix stream socket, each socket made with the platform's defaults, as
shards' stream (crates/shards/src/daemon/ship/stream.rs, Conn::open) makes them. The writer
writes without blocking until no write has taken a byte for a second, so that the kernel's
autotuning has grown its buffers as far as it does. The ship tests' message that a receiver
ends its connection on as it is written must be longer than this (ci-fluentd-epipe README).

    python3 absorbed.py [ROUNDS]
"""
import os, socket, sys, tempfile, threading, time

def absorbed(listener, connect):
    got = threading.Event()
    held = []
    def receive():
        conn, _ = listener.accept()
        held.append(conn)
        conn.recv(4096)
        got.set()
    t = threading.Thread(target=receive)
    t.start()
    w = connect()
    w.setblocking(False)
    chunk = b"x" * 65536
    total, idle = 0, None
    while True:
        try:
            n = w.send(chunk)
            total += n
            idle = None
        except BlockingIOError:
            now = time.monotonic()
            idle = idle or now
            if now - idle >= 1.0 and got.is_set():
                break
            time.sleep(0.05)
    t.join()
    w.close()
    for c in held:
        c.close()
    return total

def tcp():
    l = socket.socket()
    l.bind(("127.0.0.1", 0))
    l.listen(1)
    n = absorbed(l, lambda: socket.create_connection(l.getsockname()))
    l.close()
    return n

def unix():
    d = tempfile.mkdtemp()
    path = os.path.join(d, "s")
    l = socket.socket(socket.AF_UNIX)
    l.bind(path)
    l.listen(1)
    def connect():
        s = socket.socket(socket.AF_UNIX)
        s.connect(path)
        return s
    n = absorbed(l, connect)
    l.close()
    os.unlink(path)
    os.rmdir(d)
    return n

rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 5
import platform
print(f"host {platform.node()} {platform.system()} {platform.release()} {platform.machine()}")
for name, f in (("tcp", tcp), ("unix", unix)):
    ns = sorted(f() for _ in range(rounds))
    print(f"{name}: n={rounds} min={ns[0]} max={ns[-1]} all={ns}")
