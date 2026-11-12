# Runs a command; prints its exit status, wall time, CPU time and peak RSS (getrusage of
# the child), for PM M524 where no GNU time is installed.
import resource, subprocess, sys, time
t = time.monotonic()
r = subprocess.run(sys.argv[1:])
w = time.monotonic() - t
u = resource.getrusage(resource.RUSAGE_CHILDREN)
scale = 1 if sys.platform == "darwin" else 1024  # ru_maxrss: bytes on macOS, KiB on Linux
print(f"ptime: exit={r.returncode} wall={w:.3f}s user={u.ru_utime:.3f}s sys={u.ru_stime:.3f}s maxrss={u.ru_maxrss * scale / 2**20:.1f}MiB", file=sys.stderr)
sys.exit(r.returncode)
