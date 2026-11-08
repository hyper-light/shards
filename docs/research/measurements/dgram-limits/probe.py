"""What a datagram socket sends and takes, by its buffers' sizes (PM M232, D163): a unix
datagram of 3,000, 16,684 and 40,000 bytes sent with a send buffer one byte short of it,
as long as it, and 16 bytes longer, to a receiver with room; then received by one whose
receive buffer is as long as it plus 0 to 512 bytes; and a UDP datagram of 16,684 bytes
with the default send buffer and a raised one. Also the host's limits (sysctl).

    python3 probe.py
"""
import os, socket, subprocess, tempfile

for key in ("net.local.dgram.maxdgram", "net.local.dgram.recvspace", "net.inet.udp.maxdgram"):
    out = subprocess.run(["sysctl", "-n", key], capture_output=True, text=True)
    print(key, out.stdout.strip() or out.stderr.strip())

d = tempfile.mkdtemp()
path = os.path.join(d, "g")
r = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
r.bind(path)
r.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 20)
for size in (3000, 16684, 40000):
    for snd in (size - 1, size, size + 16):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, snd)
        try:
            s.sendto(b"x" * size, path)
            r.recv(1 << 17)
            print(f"unix {size} bytes, send buffer {snd}: sent")
        except OSError as e:
            print(f"unix {size} bytes, send buffer {snd}: errno {e.errno} {e.strerror}")
        s.close()
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1 << 20)
for extra in (0, 16, 64, 512):
    r2 = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    p2 = f"{path}{extra}"
    r2.bind(p2)
    r2.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 16684 + extra)
    try:
        s.sendto(b"x" * 16684, p2)
        r2.recv(1 << 17)
        print(f"unix 16684 bytes, receive buffer +{extra}: taken")
    except OSError as e:
        print(f"unix 16684 bytes, receive buffer +{extra}: errno {e.errno} {e.strerror}")
    r2.close()
    os.unlink(p2)
u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
u.bind(("127.0.0.1", 0))
for snd in (None, 16684):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    if snd:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, snd)
    try:
        s.sendto(b"x" * 16684, u.getsockname())
        u.recv(1 << 17)
        print(f"udp 16684 bytes, send buffer {snd or 'default'}: sent")
    except OSError as e:
        print(f"udp 16684 bytes, send buffer {snd or 'default'}: errno {e.errno} {e.strerror}")
    s.close()
r.close()
os.unlink(path)
os.rmdir(d)
