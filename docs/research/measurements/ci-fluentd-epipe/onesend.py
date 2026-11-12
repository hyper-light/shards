#!/usr/bin/env python3
"""What one blocking send(2) of 16 MiB says when its receiver reads 4,096 bytes and closes
with the rest unread (as a receiver past its bound does) while the send is under way: the
bytes it says it sent, or its error; then what the next send(2) says. Over loopback TCP and
a unix stream socket, each with the platform's defaults, as shards' stream makes them.

    python3 onesend.py
"""
import errno, os, platform, socket, tempfile, threading

def name(e):
    return errno.errorcode.get(e.errno, str(e.errno))

def pair(kind, d):
    if kind == "unix":
        path = os.path.join(d, "s")
        l = socket.socket(socket.AF_UNIX)
        l.bind(path)
        l.listen(1)
        w = socket.socket(socket.AF_UNIX)
        w.connect(path)
    else:
        l = socket.socket()
        l.bind(("127.0.0.1", 0))
        l.listen(1)
        w = socket.create_connection(l.getsockname())
    r, _ = l.accept()
    l.close()
    return w, r

print(f"host {platform.node()} {platform.system()} {platform.release()} {platform.machine()}")
for kind in ("tcp", "unix"):
    for _ in range(3):
        with tempfile.TemporaryDirectory() as d:
            w, r = pair(kind, d)
            def receive():
                r.recv(4096)
                r.close()
            t = threading.Thread(target=receive)
            t.start()
            said = []
            for _ in range(2):
                try:
                    said.append(f"sent {w.send(b'L' * (16 << 20))}")
                except OSError as e:
                    said.append(name(e))
            t.join()
            w.close()
            print(f"{kind}: first send(2) of {16 << 20}: {said[0]}; next: {said[1]}")
