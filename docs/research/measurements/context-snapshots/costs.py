#!/usr/bin/env python3
"""PM M242: what each way of taking a tree of N files of 1 KiB as one version each costs on
APFS, its phases timed, rounds interleaved:

- one clone a file (fclonefileat), each then read;
- the tree cloned at once (clonefile), then its clones read;
- the files read in place, the tree then cloned at once as the barrier, each lstat again;
- reading in place alone, which takes no one version.

And a clone tree's removal by 1, 2, 4 and 8 threads, a directory each in turn.

    costs.py DIR N ROUNDS
"""
import ctypes
import os
import shutil
import sys
import threading
import time

d, n, rounds = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
tree = os.path.join(d, "costs-tree")
if os.path.exists(tree):
    shutil.rmtree(tree)
for i in range(n):
    sub = os.path.join(tree, f"d{i // 100}")
    os.makedirs(sub, exist_ok=True)
    with open(os.path.join(sub, f"f{i}"), "wb") as f:
        f.write(b"x" * 1024)
rel = [os.path.relpath(os.path.join(r, f), tree) for r, _, fs in os.walk(tree) for f in fs]
CLONE_NOFOLLOW, CLONE_NOOWNERCOPY = 1, 2


def read_all(root):
    for p in rel:
        fd = os.open(os.path.join(root, p), os.O_RDONLY | os.O_NOFOLLOW)
        os.fstat(fd)
        os.read(fd, 1025)
        os.close(fd)


def clone_tree(dst):
    if libc.clonefile(tree.encode(), dst.encode(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY) != 0:
        raise OSError(ctypes.get_errno(), "clonefile")


def clone_each(dst):
    os.makedirs(dst)
    for i, p in enumerate(rel):
        fd = os.open(os.path.join(tree, p), os.O_RDONLY | os.O_NOFOLLOW)
        if libc.fclonefileat(fd, -2, os.path.join(dst, str(i)).encode(), 0) != 0:
            raise OSError(ctypes.get_errno(), "fclonefileat")
        os.close(fd)
        c = os.open(os.path.join(dst, str(i)), os.O_RDONLY)
        os.read(c, 1025)
        os.close(c)


def remove(root, threads):
    dirs = [os.path.join(root, x) for x in os.listdir(root)]

    def work():
        while True:
            try:
                dd = dirs.pop()
            except IndexError:
                return
            if os.path.isdir(dd):
                for f in os.listdir(dd):
                    os.unlink(os.path.join(dd, f))
                os.rmdir(dd)
            else:
                os.unlink(dd)

    ts = [threading.Thread(target=work) for _ in range(threads)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    os.rmdir(root)


phases = {}


def timed(name, f):
    t0 = time.perf_counter()
    f()
    phases.setdefault(name, []).append((time.perf_counter() - t0) * 1e3)


read_all(tree)
for r in range(rounds):
    each, at_once, barrier = (os.path.join(d, f"costs-{w}-{r}") for w in ("each", "once", "barrier"))
    timed("a clone a file, each read", lambda: clone_each(each))
    timed("its clones removed", lambda: remove(each, 1))
    timed("the tree cloned at once", lambda: clone_tree(at_once))
    timed("its clones read", lambda: read_all(at_once))
    timed("the tree's clones removed", lambda: remove(at_once, 1))
    timed("read in place", lambda: read_all(tree))
    timed("the tree cloned at once, a barrier", lambda: clone_tree(barrier))
    timed("each lstat again", lambda: [os.lstat(os.path.join(tree, p)) for p in rel])
    for k in (1, 2, 4, 8):
        timed(f"the barrier's clones removed, {k} threads", lambda: remove(barrier, k))
        if k != 8:
            clone_tree(barrier)
for name, v in phases.items():
    v.sort()
    pick = lambda q: v[min(len(v) - 1, int(len(v) * q))]
    print(f"{name}, {n} files: n={len(v)} p50={pick(0.5):.1f} p90={pick(0.9):.1f} p99={pick(0.99):.1f} max={v[-1]:.1f} ms")
