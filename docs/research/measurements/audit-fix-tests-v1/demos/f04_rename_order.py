# Finding 4: the confined agent writes "confined net=..." and flushes, then sets its comm to
# confined-ready (crates/testguest/src/workload.rs:5404-5408). A reader that acts on the line
# reads the name: the line does not imply the rename. Natural trials count how often the
# name is still the old one; the held variant pins the order deterministically.
import ctypes
import os
import signal
import sys

libc = ctypes.CDLL(None, use_errno=True)
PR_SET_NAME = 15


def trial(hold):
    line_r, line_w = os.pipe()
    gate_r, gate_w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(line_r)
        os.close(gate_w)
        os.write(line_w, b"confined net=lo\n")
        if hold:
            os.read(gate_r, 1)  # any delay between the two steps
        libc.prctl(PR_SET_NAME, b"confined-ready", 0, 0, 0)
        signal.pause()
        os._exit(0)
    os.close(line_w)
    os.close(gate_r)
    line = os.read(line_r, 64)
    with open(f"/proc/{pid}/comm") as comm:
        name = comm.read().strip()
    os.write(gate_w, b"x")
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    os.close(line_r)
    os.close(gate_w)
    assert line == b"confined net=lo\n"
    return name


trials = int(sys.argv[1])
names = [trial(False) for _ in range(trials)]
stale = sum(1 for n in names if n != "confined-ready")
print(f"natural trials={trials} line-before-rename observed={stale} names={sorted(set(names))}")
held = trial(True)
print(f"held trial: name after the line = {held!r} (renamed: {held == 'confined-ready'})")
