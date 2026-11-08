#!/usr/bin/env python3
"""PM M494: how the journal cuts a program's long line, and what it keeps of one entry.

The journald driver cuts a run's line at the journal's own LineMax (daemon/ship/journald.rs).
On a host whose journal is systemd's, as its user: (1) a line of 100,000 bytes written to the
journal as a program's output (systemd-cat, a stream), its entries' MESSAGE lengths read back;
(2) entries of the native protocol (a datagram to the journal's socket, as the driver sends
one) whose MESSAGE is 49,152, 65,536, 150,000 and 200,000 bytes, which the socket takes and
which the journal keeps, each read back. Each identifier is the run's own; nothing else is
read. Prints systemd's version and the journal's LineMax as configured.
"""
import json, os, socket, subprocess, sys, time

tag = f"shards-m494-{os.getpid()}"
print(subprocess.run(["systemctl", "--version"], capture_output=True, text=True).stdout.splitlines()[0])
conf = subprocess.run(["systemd-analyze", "cat-config", "systemd/journald.conf"], capture_output=True, text=True).stdout
print("LineMax configured:", [l for l in conf.splitlines() if "LineMax" in l])
with open("/proc/sys/net/core/wmem_default") as f:
    print("wmem_default", f.read().strip())


def kept(ident, want):
    """The MESSAGE lengths the journal keeps for `ident`, once `want` entries are there."""
    for _ in range(100):
        out = subprocess.run(["journalctl", "--user-unit=none", "-q", "--no-pager", "-o", "json",
                              f"SYSLOG_IDENTIFIER={ident}"], capture_output=True, text=True).stdout
        if not out.strip():
            out = subprocess.run(["journalctl", "-q", "--no-pager", "-o", "json", "--all",
                                  f"SYSLOG_IDENTIFIER={ident}"], capture_output=True, text=True).stdout
        lens = []
        for line in out.splitlines():
            m = json.loads(line).get("MESSAGE")
            lens.append(len(m) if isinstance(m, (str, list)) else m)
        if len(lens) >= want:
            return lens
        time.sleep(0.1)
    return lens


# (1) A stream.
ident = f"{tag}-stream"
subprocess.run(["systemd-cat", "-t", ident], input=b"x" * 100_000 + b"\n", check=True)
print("stream of 100,000 bytes, its entries' MESSAGE lengths:", kept(ident, 3))

# (2) The native protocol.
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
for n in [49_152, 65_536, 150_000, 200_000]:
    ident = f"{tag}-native-{n}"
    entry = f"SYSLOG_IDENTIFIER={ident}\n".encode() + b"MESSAGE=" + b"x" * n + b"\n"
    try:
        s.sendto(entry, "/run/systemd/journal/socket")
        sent = "sent"
    except OSError as e:
        print(f"native entry of {n} bytes: {e}")
        continue
    print(f"native entry of {n} bytes ({len(entry)} in its datagram): {sent}; kept:", kept(ident, 1))
