"""What a unix stream socket's sender can learn of the bytes its receiver has not read:
SIOCOUTQ (Linux) or SO_NWRITE (macOS), after 1000 bytes are written and none read.

PM M233: python3 unixq.py, on macOS and in a Linux microVM (see probe.py).
"""
import fcntl, socket, struct, sys
a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
a.sendall(b"x" * 1000)
if sys.platform == "linux":
    n = struct.unpack("i", fcntl.ioctl(a.fileno(), 0x5411, b"\0" * 4))[0]
    print("linux SIOCOUTQ unread:", n)
    b.recv(400)
    print("linux SIOCOUTQ after 400 read:", struct.unpack("i", fcntl.ioctl(a.fileno(), 0x5411, b"\0" * 4))[0])
else:
    print("macos SO_NWRITE unread:", a.getsockopt(socket.SOL_SOCKET, 0x1024))
    b.recv(400)
    print("macos SO_NWRITE after 400 read:", a.getsockopt(socket.SOL_SOCKET, 0x1024))
