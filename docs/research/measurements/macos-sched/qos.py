#!/usr/bin/env python3
"""Each QoS class a thread may ask for (pthread_set_qos_class_self_np), one thread of each
spinning for 1.5 s, and the priority `ps -M` shows each at, 0.7 s in (spinning, below its
base as XNU ages its usage): what QoS a program started from here gets. Started from an
agent's session, its user-interactive, initiated, default and utility threads were at 15,
11, 12 and 16, none past the utility clamp's 20; started as a launchd job of the user's
session (launchd-run.sh), at 37, 37, 31 and 20, its main thread at 31, as a terminal's
programs are (PM M317).

    qos.py
"""
import ctypes, ctypes.util, os, subprocess, sys, threading, time
libc = ctypes.CDLL(ctypes.util.find_library("c"))
QOS = {"ui": 0x21, "in": 0x19, "default": 0x15, "ut": 0x11, "bg": 0x09}
def run(name):
    rc = libc.pthread_set_qos_class_self_np(QOS[name], 0)
    t0 = time.time()
    while time.time() - t0 < 1.5: pass
threads = [threading.Thread(target=run, args=(n,), name=n) for n in QOS]
for t in threads: t.start()
time.sleep(0.7)
print(subprocess.run(["ps", "-M", "-p", str(os.getpid())], capture_output=True, text=True).stdout)
for t in threads: t.join()
