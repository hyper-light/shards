"""Controlled native child; no VM, timer, retry or background descendant."""
import os
import signal
import sys

signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1})
print(f"CLEANUP_CHILD_PID {os.getpid()}", file=sys.stderr, flush=True)
mode = sys.argv[1]
if mode == "early":
    raise SystemExit(7)
if mode == "partial":
    sys.stdout.write("rea")
    sys.stdout.flush()
elif mode == "normal":
    print("ready", flush=True)
elif mode != "silent":
    raise SystemExit("unknown controlled child mode")
signal.sigwait({signal.SIGUSR1})
print("released", flush=True)
