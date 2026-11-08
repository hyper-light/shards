"""The resend test's kernel facts on Linux: a listener's SO_RCVBUF of 1 shuts its accepted
socket's window; 1 KiB writes then stay unacknowledged (SIOCOUTQ); the receiver's
SO_LINGER-0 close resets the connection, and the sender's next write, or its read, fails.

PM M233: in a Linux microVM (see probe.py); the facts stream.rs's resend test stands on.
"""
import fcntl, socket, struct, time
def outq(s): return struct.unpack("i", fcntl.ioctl(s.fileno(), 0x5411, b"\0" * 4))[0]
l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1); l.bind(("127.0.0.1", 0)); l.listen(4)
c = socket.create_connection(l.getsockname())
written = 0
for i in range(64):
    c.sendall(bytes([i]) * 1024); written += 1024
    time.sleep(0.01)
    if outq(c) > 2048:
        break
print("messages", i + 1, "written", written, "unacked", outq(c), "sndbuf", c.getsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF))
time.sleep(0.1); print("unacked 100 ms on", outq(c))
r, _ = l.accept(); print("receiver rcvbuf", r.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF))
r.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)); r.close()
time.sleep(0.05)
print("SO_ERROR after reset", c.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR), "unacked", outq(c))
try:
    c.sendall(b"last"); print("write after reset: ok")
except OSError as e:
    print("write after reset:", e)
