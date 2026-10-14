"""What a TCP sender's kernel says of the bytes its receiver's TCP has not acknowledged,
and its retransmission timeout: Linux's SIOCOUTQ and TCP_INFO's tcpi_rto (us), macOS's
TCP_CONNECTION_INFO's tcpi_snd_sbbytes and tcpi_rto (ms).

1. 1,000 bytes written to a receiver that reads none: its kernel acknowledges them.
2. Its receive buffer set to 4,096 bytes (SO_RCVBUF), written on until the sender's
   send buffer takes no more: what is held, unsent, counts, where the window closes.
3. The receiver, its buffer grown again, reads all: the count falls to nothing.
4. The sender's shutdown(SHUT_WR): its FIN, until acknowledged.

PM M233: python3 acked.py, on macOS and in a Linux microVM (see probe.py).
"""

import fcntl
import socket
import struct
import sys
import time


def state(s):
    """Unacknowledged bytes, the RTO, and the TCP state's number (Linux: tcp_states.h,
    1 ESTABLISHED, 4 FIN_WAIT1, 5 FIN_WAIT2; macOS: tcp_fsm.h, 4 ESTABLISHED,
    6 FIN_WAIT_1, 9 FIN_WAIT_2)."""
    if sys.platform == "linux":
        unacked = struct.unpack("i", fcntl.ioctl(s.fileno(), 0x5411, b"\0" * 4))[0]
        info = s.getsockopt(socket.IPPROTO_TCP, socket.TCP_INFO, 104)
        rto = struct.unpack_from("I", info, 8)[0] / 1000
        return unacked, f"rto {rto:.0f} ms", f"state {info[0]}"
    info = s.getsockopt(socket.IPPROTO_TCP, 0x106, 256)
    rto, sbbytes = struct.unpack_from("I", info, 12)[0], struct.unpack_from("I", info, 32)[0]
    return sbbytes, f"rto {rto} ms", f"state {info[0]}"


listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(1)
c = socket.create_connection(listener.getsockname())
r, _ = listener.accept()

c.sendall(b"x" * 1000)
time.sleep(0.2)
print(sys.platform, "1. 1000 written, none read:", state(c))

r.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
c.setblocking(False)
sent = 0
try:
    while True:
        sent += c.send(b"y" * 65536)
except BlockingIOError:
    pass
time.sleep(0.2)
print(
    sys.platform,
    f"2. {sent} more written; the receiver's buffer {r.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF)}:",
    state(c),
)

r.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1 << 22)
r.settimeout(30)
got = 0
while got < 1000 + sent:
    got += len(r.recv(1 << 20))
time.sleep(0.2)
print(sys.platform, f"3. {got} read:", state(c))

c.shutdown(socket.SHUT_WR)
print(sys.platform, "4. shut, at once:", state(c))
time.sleep(0.2)
print(sys.platform, "4. shut, 200 ms on:", state(c))
