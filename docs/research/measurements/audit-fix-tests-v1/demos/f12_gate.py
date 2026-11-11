# Finding 12: broker_effects.rs bound the gate at 0.0.0.0 (line 384), accepted its first peer
# unverified (line 161), and unwrapped a UDP route probe (lines 236-243).
import errno
import socket
import subprocess
import sys


def route_v4():
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect(("192.0.2.1", 9))  # TEST-NET-1: selects a route, sends nothing
        return probe.getsockname()[0]
    finally:
        probe.close()


if sys.argv[1] == "no-route":
    try:
        print("route probe:", route_v4())
    except OSError as e:
        print(f"route probe without a default route: OSError errno={e.errno} ({errno.errorcode[e.errno]}): the old .unwrap() panics here")
    sys.exit(0)

route = route_v4()
old = socket.socket()
old.bind(("0.0.0.0", 0))
old.listen()
port = old.getsockname()[1]
stranger = socket.create_connection(("127.0.0.1", port))
peer, address = old.accept()
print(f"old gate 0.0.0.0:{port}: first peer accepted unverified: {address} (route address {route})")
for s in (peer, stranger, old):
    s.close()

new = socket.socket()
new.bind((route, 0))
new.listen()
port = new.getsockname()[1]
try:
    socket.create_connection(("127.0.0.1", port), timeout=2)
    print("new gate: loopback reached it")
except ConnectionRefusedError:
    print(f"new gate {route}:{port}: loopback refused (not bound there)")
# The fix verifies a peer by this host's addresses (the test reads them with getifaddrs(3); here
# `ip -o -4 addr`): the kernel picks the source of a connection to a local address, not always the
# gate's own address.
ours = {line.split()[3].split("/")[0] for line in subprocess.run(
    ["ip", "-o", "-4", "addr", "show"], capture_output=True, text=True, check=True).stdout.splitlines()}
own = socket.create_connection((route, port))
peer, address = new.accept()
print(f"new gate: a connection from this host came from {address[0]}: equal to the gate address {route}: "
      f"{address[0] == route}; one of this host's addresses {sorted(ours)}: {address[0] in ours}")
print("a peer at 203.0.113.9 (TEST-NET-3, not this host's) is this host's:", "203.0.113.9" in ours)
for s in (peer, own, new):
    s.close()
