# Does a process's I/O counter grow over an interval in which its CPU does not?
# Child modes: sock (reads a TCP socket the parent feeds 64 KiB every GAP ms),
# file (writes 1 MiB to a file every GAP ms), fsync (same, fsync after each).
# The parent samples the child every ~1 ms: CPU (macOS proc_pid_rusage V4 user+system;
# Linux /proc/<pid>/schedstat run time, ns), and I/O (macOS ri_diskio_bytesread/written;
# Linux /proc/<pid>/io rchar, wchar, read_bytes, write_bytes).
# Run: python3 -I io_vs_cpu.py [sock] [file] [fsync]. Method and results: D209 (#77).
import os, sys, socket, time, ctypes, subprocess, tempfile
GAP = 0.05; DUR = 3.0
def child(mode, port, path):
    if mode == "sock":
        s = socket.create_connection(("127.0.0.1", port))
        while s.recv(1 << 20): pass
    else:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC)
        buf = b"x" * (1 << 20); end = time.time() + DUR
        while time.time() < end:
            os.write(fd, buf)
            if mode == "fsync": os.fsync(fd)
            time.sleep(GAP)
if len(sys.argv) > 1 and sys.argv[1] == "child":
    child(sys.argv[2], int(sys.argv[3]), sys.argv[4]); sys.exit(0)
if sys.platform == "darwin":
    lib = ctypes.CDLL("/usr/lib/libproc.dylib")
    def sample(pid):
        buf = (ctypes.c_uint64 * 64)()
        if lib.proc_pid_rusage(pid, 4, ctypes.byref(buf)) != 0: return None
        u = list(buf)[2:]  # past the 16-byte uuid
        return (u[0] + u[1], (u[16], u[17]))  # mach ticks; diskio read, written
else:
    def sample(pid):
        try:
            cpu = int(open(f"/proc/{pid}/schedstat").read().split()[0])
            io = dict(l.split(": ") for l in open(f"/proc/{pid}/io").read().splitlines())
            return (cpu, tuple(int(io[k]) for k in ("rchar", "wchar", "read_bytes", "write_bytes")))
        except OSError: return None
for mode in sys.argv[1:] or ["sock", "file", "fsync"]:
    d = tempfile.mkdtemp(); path = os.path.join(d, "f")
    lsn = socket.socket(); lsn.bind(("127.0.0.1", 0)); lsn.listen(1)
    p = subprocess.Popen([sys.executable, "-I", __file__, "child", mode, str(lsn.getsockname()[1]), path])
    conn = lsn.accept()[0] if mode == "sock" else None
    prev = sample(p.pid); n = io_only = io_any = cpu_any = 0
    end = time.time() + DUR; last_feed = 0
    while time.time() < end:
        if conn and time.time() - last_feed >= GAP:
            conn.sendall(b"y" * 65536); last_feed = time.time()
        time.sleep(0.001)
        cur = sample(p.pid)
        if cur is None or prev is None: break
        cpu_grew = cur[0] > prev[0]; io_grew = any(a > b for a, b in zip(cur[1], prev[1]))
        n += 1; io_any += io_grew; cpu_any += cpu_grew; io_only += io_grew and not cpu_grew
        prev = cur
    if conn: conn.close()
    p.wait()
    print(f"{sys.platform} {mode}: samples {n}, cpu grew {cpu_any}, io grew {io_any}, io grew with no cpu {io_only}; last io {prev[1] if prev else None}")
