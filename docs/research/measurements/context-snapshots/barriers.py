#!/usr/bin/env python3
"""PM M242: which calls wait out a write under way on APFS, the barrier a context file's
take needs where APFS stamps a write's times as it ends (PM M100).

A writer rewrites a file of SIZE bytes, each version one pwrite(2) of one byte value; while
one is under way, each candidate is made on a second descriptor, and judged to have waited
it out if it returned after the write had ended. Then directory clones made during writes:
whether each waited, and whether the file in it was one version.

    barriers.py DIR SIZE TRIALS
"""
import ctypes
import os
import shutil
import sys
import threading
import time

d, SIZE, TRIALS = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
tree = os.path.join(d, "barrier-tree")
if os.path.exists(tree):
    shutil.rmtree(tree)
os.makedirs(os.path.join(tree, "sub"))
for i in range(50):
    with open(os.path.join(tree, "sub", f"g{i}"), "wb") as f:
        f.write(b"y" * 1024)
path = os.path.join(tree, "sub", "f")
with open(path, "wb") as f:
    f.write(b"A" * SIZE)
exists = os.path.join(d, "barrier-exists")
open(exists, "wb").close()
readonly = os.path.join(d, "barrier-ro")
os.makedirs(readonly, exist_ok=True)
os.chmod(readonly, 0o500)

versions = [bytes([66 + i]) * SIZE for i in range(2)]
state = {"n": 0, "end": 0.0}
lock = threading.Lock()
going = threading.Event()
stop = threading.Event()


def writer():
    w = os.open(path, os.O_WRONLY)
    i = 0
    while not stop.is_set():
        i ^= 1
        with lock:
            state["n"] += 1
            state["end"] = 0.0
        going.set()
        os.pwrite(w, versions[i], 0)
        with lock:
            state["end"] = time.monotonic()
        going.clear()
        time.sleep(0.002)
    os.close(w)


r = os.open(path, os.O_RDONLY)


class Log2Phys(ctypes.Structure):
    _fields_ = [("flags", ctypes.c_uint), ("contigbytes", ctypes.c_int64), ("devoffset", ctypes.c_int64)]


class AttrList(ctypes.Structure):
    _fields_ = [
        ("bitmapcount", ctypes.c_ushort),
        ("reserved", ctypes.c_uint16),
        ("commonattr", ctypes.c_uint32),
        ("volattr", ctypes.c_uint32),
        ("dirattr", ctypes.c_uint32),
        ("fileattr", ctypes.c_uint32),
        ("forkattr", ctypes.c_uint32),
    ]


# <sys/fcntl.h>, <sys/attr.h>
F_GETPATH, F_LOG2PHYS_EXT, F_BARRIERFSYNC = 50, 65, 85
ATTR_CMN_MODTIME, ATTR_FILE_DATALENGTH, ATTR_FILE_DATAALLOCSIZE = 0x400, 0x200, 0x400
clones = [0]


def fclone(dst, flags=0):
    return lambda: libc.fclonefileat(r, -2, dst.encode(), flags)


def fclone_made():
    clones[0] += 1
    dst = os.path.join(d, f"barrier-clone-{clones[0]}")
    libc.fclonefileat(r, -2, dst.encode(), 0)
    os.unlink(dst)


def attrs():
    a = AttrList(5, 0, ATTR_CMN_MODTIME, 0, 0, ATTR_FILE_DATALENGTH | ATTR_FILE_DATAALLOCSIZE, 0)
    buf = ctypes.create_string_buffer(256)
    libc.fgetattrlist(r, ctypes.byref(a), buf, 256, 0)


def log2phys():
    p = Log2Phys(0, 1, 0)
    libc.fcntl(r, F_LOG2PHYS_EXT, ctypes.byref(p))


def getpath():
    buf = ctypes.create_string_buffer(1024)
    libc.fcntl(r, F_GETPATH, buf)


def flock():
    import fcntl

    fcntl.flock(r, fcntl.LOCK_SH)
    fcntl.flock(r, fcntl.LOCK_UN)


candidates = {
    "fstat": lambda: os.fstat(r),
    "pread of its last byte": lambda: os.pread(r, 1, SIZE - 1),
    "lseek SEEK_DATA": lambda: os.lseek(r, 0, 4),
    "lseek SEEK_HOLE": lambda: os.lseek(r, 0, 3),
    "fsync": lambda: os.fsync(r),
    "F_BARRIERFSYNC": lambda: libc.fcntl(r, F_BARRIERFSYNC, 0),
    "F_LOG2PHYS_EXT": log2phys,
    "F_GETPATH": getpath,
    "fgetattrlist of its lengths": attrs,
    "fgetxattr": lambda: libc.fgetxattr(r, b"user.none", ctypes.create_string_buffer(64), 64, 0, 0),
    "flock": flock,
    "fclonefileat onto an existing name (EEXIST)": fclone(exists),
    "fclonefileat into a read-only directory (EACCES)": fclone(os.path.join(readonly, "x")),
    "fclonefileat": fclone_made,
}


def under_way():
    """Waits for a write to begin; its number, or none if it ended already."""
    going.wait()
    time.sleep(0.0005)
    with lock:
        return None if state["end"] else state["n"]


def ended(n):
    """Waits for write `n` to end: when it did, or none if another began first."""
    while True:
        with lock:
            if state["n"] != n:
                return None
            if state["end"]:
                return state["end"]
        time.sleep(0.0001)


t = threading.Thread(target=writer)
t.start()
waited = {k: [] for k in candidates}
dirs = {"made": 0, "waited": 0, "torn": 0}
try:
    for _ in range(TRIALS):
        for name, call in candidates.items():
            n = under_way()
            if n is None:
                continue
            call()
            back = time.monotonic()
            end = ended(n)
            if end is not None:
                waited[name].append(back >= end - 0.0002)
        n = under_way()
        if n is None:
            continue
        out = os.path.join(d, f"barrier-tree-clone-{dirs['made']}")
        if libc.clonefile(tree.encode(), out.encode(), 1) != 0:
            raise OSError(ctypes.get_errno(), "clonefile")
        back = time.monotonic()
        end = ended(n)
        with open(os.path.join(out, "sub", "f"), "rb") as f:
            data = f.read()
        dirs["made"] += 1
        dirs["torn"] += len(set(data)) != 1
        dirs["waited"] += end is not None and back >= end - 0.0002
        shutil.rmtree(out)
finally:
    stop.set()
    t.join()
    os.chmod(readonly, 0o700)
for name, w in waited.items():
    print(f"{name}: n={len(w)}, waited out the write {sum(w)}")
print(f"clonefile of its directory: n={dirs['made']}, waited out the write {dirs['waited']}, torn {dirs['torn']}")
