#!/usr/bin/env python3
"""Fill a 128-descriptor shards daemon with silent slot clients (as tests/daemon.rs
silent_slot takes one), then sample the daemon's descriptor count (proc_pidinfo
PROC_PIDLISTFDS) every millisecond, printing each change with its time and the daemon log's
new lines. Usage: fdwatch.py SHARDS HOME SECONDS"""
import ctypes, os, subprocess, sys, time

shards, home, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
PROC_PIDLISTFDS = 1
FD_INFO = 8  # struct proc_fdinfo { int32 proc_fd; uint32 proc_fdtype; }

def fds(pid):
    buf = ctypes.create_string_buffer(FD_INFO * 4096)
    n = libproc.proc_pidinfo(pid, PROC_PIDLISTFDS, 0, buf, len(buf))
    if n <= 0:
        return None
    out = set()
    for i in range(n // FD_INFO):
        fd = int.from_bytes(buf.raw[i * FD_INFO:i * FD_INFO + 4], "little", signed=True)
        kind = int.from_bytes(buf.raw[i * FD_INFO + 4:i * FD_INFO + 8], "little")
        out.add((fd, kind))
    return out

env = dict(os.environ, SHARDS_HOME=home, SHARDS_LOCAL_STORE="none")
subprocess.run(["/bin/sh", "-c", 'ulimit -n 128 && exec "$0" ps', shards], env=env, check=True,
               stdout=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
pid = int(open(os.path.join(home, "daemon.pid")).read().split()[0])
log = os.path.join(home, "daemon.log")
t0 = time.monotonic()
def now():
    return f"{(time.monotonic() - t0) * 1000:9.1f} ms"
seen_log = 0
def log_new():
    global seen_log
    text = open(log).read()
    new = text[seen_log:]
    seen_log = len(text)
    return [l for l in new.splitlines() if l]

held = []
def silent(patience):
    d = os.path.join(home, "requests")
    try:
        free = os.open(os.path.join(d, "free"), os.O_RDONLY | os.O_NONBLOCK)
    except OSError:
        return False
    deadline = time.monotonic() + patience
    try:
        while True:
            try:
                rec = os.read(free, 16)
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    return False
                time.sleep(0.001)
                continue
            if len(rec) != 16:
                return False
            break
    finally:
        os.close(free)
    n = int(rec.decode().strip())
    a = os.open(os.path.join(d, f"{n}.answer"), os.O_RDONLY)
    s = os.open(os.path.join(d, f"{n}.say"), os.O_WRONLY)
    os.write(s, b"\0")
    held.append((a, s))
    return True

last = fds(pid)
print(f"{now()} daemon {pid}: {len(last)} descriptors idle", flush=True)
for l in log_new():
    print(f"{now()} log: {l}", flush=True)
OUT = "clients wait until the daemon has room"
AGAIN = "offering slots again"
state_out = False
# Fill: take slots until none comes in 200 ms while the log says it is out.
while True:
    took = silent(0.2)
    for l in log_new():
        print(f"{now()} log: {l.split(': ', 1)[-1]}", flush=True)
        if OUT in l:
            state_out = True
        if AGAIN in l:
            state_out = False
    if took:
        continue
    if state_out:
        break
print(f"{now()} out, {len(held)} clients in; watching {secs} s", flush=True)
end = time.monotonic() + secs
while time.monotonic() < end:
    cur = fds(pid)
    if cur is not None and cur != last:
        gone = sorted(last - cur)
        came = sorted(cur - last)
        print(f"{now()} {len(cur)} descriptors: -{gone} +{came}", flush=True)
        last = cur
    for l in log_new():
        print(f"{now()} log: {l.split(': ', 1)[-1]}", flush=True)
    time.sleep(0.001)
subprocess.run([shards, "daemon", "stop"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
