#!/usr/bin/env python3
"""A sustained vsock stream through a VM, for A/B runs and profiles (PM M57).

Boots the test guest's `vsock` mode under each given shards-vm, greets it on host port
5000, then streams through its echo on port 1234 for SECS seconds: a writer sends 256 KiB
buffers while the reader takes the echo back. Prints, for each run, the VM process's CPU
time and the echoed rate. Builds alternate, ROUNDS times.

  stream.py [--rounds N] [--secs S] [--sample DIR] KERNEL GUEST SHARDS_VM...

With --sample, macOS's sample(1) profiles each VM process while it streams, into DIR.
Signed copies of shards-vm are those the E2E tests place in target/e2e/ (scripts/hvf-run).
"""

import argparse
import os
import socket
import subprocess
import tempfile
import threading
import time


def run(vm_bin, kernel, guest, secs, sample_to):
    d = tempfile.mkdtemp(prefix="vss", dir="/tmp")  # short: sun_path is 104 bytes
    sock = os.path.join(d, "v.sock")
    host = socket.socket(socket.AF_UNIX)
    host.bind(sock + "_5000")
    host.listen(1)
    vm = subprocess.Popen(
        [vm_bin, "run", "--kernel", kernel, "--init", guest, "--memory", "256",
         "--cmdline", "console=ttyS0 quiet shards_test=vsock", "--vsock", sock],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        # The guest reads the host's line to its end.
        c, _ = host.accept()
        c.recv(100)
        c.sendall(b"hello from the host\n")
        c.close()
        for line in vm.stdout:
            if b"SHARDS-TEST READY" in line:
                break
        s = socket.socket(socket.AF_UNIX)
        s.connect(sock)
        s.sendall(b"CONNECT 1234\n")
        line = b""
        while not line.endswith(b"\n"):
            line += s.recv(1)
        assert line.startswith(b"OK"), line
        stop = time.time() + secs

        def write():
            buf = b"\x5a" * (256 << 10)
            while time.time() < stop:
                s.sendall(buf)
            s.shutdown(socket.SHUT_WR)

        writer = threading.Thread(target=write)
        writer.start()
        prof = None
        if sample_to:
            os.makedirs(sample_to, exist_ok=True)
            out = os.path.join(sample_to, f"{os.path.basename(os.path.dirname(vm_bin))}-{vm.pid}.txt")
            prof = subprocess.Popen(["sample", str(vm.pid), str(max(1, int(secs) - 2)), "-file", out],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        start, echoed = time.time(), 0
        while True:
            b = s.recv(1 << 20)
            if not b:
                break
            echoed += len(b)
        elapsed = time.time() - start
        writer.join()
        if prof:
            prof.wait()
        cpu = subprocess.run(["ps", "-o", "time=", "-p", str(vm.pid)],
                             capture_output=True, text=True).stdout.strip()
        return cpu, echoed, elapsed
    finally:
        vm.kill()
        vm.wait()
        subprocess.run(["rm", "-rf", d])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=4)
    ap.add_argument("--secs", type=float, default=10)
    ap.add_argument("--sample")
    ap.add_argument("kernel")
    ap.add_argument("guest")
    ap.add_argument("vms", nargs="+")
    a = ap.parse_args()
    for r in range(1, a.rounds + 1):
        for vm_bin in a.vms:
            cpu, echoed, elapsed = run(vm_bin, a.kernel, a.guest, a.secs, a.sample)
            print(f"round {r} {vm_bin}: cpu {cpu} | echoed {echoed >> 20} MiB in {elapsed:.1f} s: "
                  f"{echoed / elapsed / 2**20:.0f} MiB/s", flush=True)


if __name__ == "__main__":
    main()
