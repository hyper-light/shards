"""Controlled native child; no VM, timer, retry or background descendant."""
import os
import signal
import sys

stdin_control = len(sys.argv) > 2 and sys.argv[2] == "stdin"
if not stdin_control:
    signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1})
print(f"CLEANUP_CHILD_PID {os.getpid()}", file=sys.stderr, flush=True)
mode = sys.argv[1]
if mode == "early":
    raise SystemExit(7)
if mode == "partial":
    sys.stdout.buffer.write(b"rea")
    sys.stdout.buffer.flush()
elif mode == "normal":
    sys.stdout.buffer.write(b"ready\n")
    sys.stdout.buffer.flush()
elif mode != "silent":
    raise SystemExit("unknown controlled child mode")
if stdin_control:
    if sys.stdin.buffer.read(1) != b"r":
        raise SystemExit(8)
else:
    signal.sigwait({signal.SIGUSR1})
sys.stdout.buffer.write(b"released\n")
sys.stdout.buffer.flush()
