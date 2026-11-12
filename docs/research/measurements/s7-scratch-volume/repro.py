"""A member's mounts as shards-init makes them (crates/init/src/domains.rs, `start` and
`child` at 451672250), replayed with the same system calls in a user and mount namespace,
for a volume under /tmp and one at /tmp.

Run as `unshare -rm python3 -I repro.py DIR` on Linux 5.12 or later. DIR must be empty or
absent. In the namespace only uid 0 is mapped, so the scratch is uid 0's here, where init
gives it the member's uid (200000 + n). Ownership plays no part in either case.
"""

import ctypes
import errno
import os
import sys

# The new mount API's system calls share one number on every architecture
# (include/uapi/asm-generic/unistd.h, arch/x86/entry/syscalls/syscall_64.tbl).
SYS_OPEN_TREE, SYS_MOVE_MOUNT, SYS_FSOPEN, SYS_FSCONFIG, SYS_FSMOUNT = 428, 429, 430, 431, 432
OPEN_TREE_CLONE, OPEN_TREE_CLOEXEC = 1, os.O_CLOEXEC
FSOPEN_CLOEXEC, FSCONFIG_SET_STRING, FSCONFIG_CMD_CREATE, FSMOUNT_CLOEXEC = 1, 1, 6, 1
MOVE_MOUNT_F_EMPTY_PATH = 4
MOUNT_ATTR_NOSUID, MOUNT_ATTR_NODEV = 0x2, 0x4
AT_FDCWD = -100

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long


def call(*args):
    r = libc.syscall(*args)
    if r < 0:
        e = ctypes.get_errno()
        raise OSError(e, os.strerror(e))
    return r


def detached(fstype, options, attrs=0):
    """domains.rs `detached_with`: fsopen, fsconfig, fsmount; attached nowhere."""
    fs = call(SYS_FSOPEN, fstype.encode(), ctypes.c_uint(FSOPEN_CLOEXEC))
    try:
        for k, v in options:
            call(SYS_FSCONFIG, fs, ctypes.c_uint(FSCONFIG_SET_STRING), k.encode(), v.encode(), 0)
        call(SYS_FSCONFIG, fs, ctypes.c_uint(FSCONFIG_CMD_CREATE), None, None, 0)
        return call(SYS_FSMOUNT, fs, ctypes.c_uint(FSMOUNT_CLOEXEC), ctypes.c_uint(attrs))
    finally:
        os.close(fs)


def copy_of(path):
    """domains.rs `copy_of`: open_tree(OPEN_TREE_CLONE), as init copies a volume's mount."""
    return call(SYS_OPEN_TREE, AT_FDCWD, path.encode(), ctypes.c_uint(OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC))


def attach(fd, at):
    """child's move_mount(fd, "", AT_FDCWD, at, MOVE_MOUNT_F_EMPTY_PATH)."""
    call(SYS_MOVE_MOUNT, fd, b"", AT_FDCWD, at.encode(), ctypes.c_uint(MOVE_MOUNT_F_EMPTY_PATH))


def member(work, dest):
    """One member started as init starts it, given one volume at `dest`: its root's
    skeleton with the volume's mount point made (`skeleton`), the root (`image_root`), the
    scratch (`scratch`) holding a staged file (staged.rs `files`, through its descriptor),
    then, as `child` orders them, the root, the scratch at /tmp and the volume at `dest`."""
    say = lambda *a: print(f"  [{dest}]", *a, flush=True)
    case = os.path.join(work, dest.strip("/").replace("/", "_"))
    lower, vol, root_at = (os.path.join(case, n) for n in ("lower", "vol", "root"))
    for d in (os.path.join(lower, "tmp"), os.path.join(lower, "etc"), vol, root_at):
        os.makedirs(d)
    lower_fd = os.open(lower, os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
    skel = detached("tmpfs", [("mode", "0755")])
    # skeleton(): its mount point, each component made, `tmp` merged with the image's.
    parts = dest.strip("/").split("/")
    for i in range(1, len(parts) + 1):
        try:
            os.mkdir("/".join(parts[:i]), 0o755, dir_fd=skel)
        except FileExistsError:
            pass
    root = detached("overlay", [("lowerdir", f"/proc/self/fd/{skel}:/proc/self/fd/{lower_fd}"), ("xino", "on")])
    scratch = detached("tmpfs", [("mode", "0750"), ("uid", "0"), ("gid", "0")], MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV)
    staged = os.open("staged", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600, dir_fd=scratch)
    os.write(staged, b"staged into its home\n")
    os.close(staged)
    volume = copy_of(vol)

    attach(root, root_at)
    point = root_at + dest
    say(f"before the scratch: its root holds {dest}: {os.path.isdir(point)}")
    attach(scratch, root_at + "/tmp")
    say(f"after the scratch at /tmp: {dest} exists: {os.path.isdir(point)}")
    try:
        attach(volume, point)
    except OSError as e:
        say(f"move_mount of the volume onto {dest}: {errno.errorcode.get(e.errno, e.errno)} ({e.strerror})")
        say("as shards-init: mounting its volumes: errno", e.errno)
        return
    say(f"move_mount of the volume onto {dest}: ok")
    # What the member sees at /tmp, and what its server instance, which holds the scratch
    # (D139), sees beneath it.
    member_staged = os.path.exists(root_at + "/tmp/staged")
    say(f"the member sees its staged /tmp/staged: {member_staged}")
    with open(root_at + "/tmp/handme", "w") as f:
        f.write("a file the member hands across\n")
    try:
        os.stat("handme", dir_fd=scratch)
        in_scratch = True
    except FileNotFoundError:
        in_scratch = False
    say(f"the member's /tmp/handme is in its scratch, where its server reads: {in_scratch}")
    say(f"it is in the volume instead: {os.path.exists(os.path.join(vol, 'handme'))}")


def main():
    work = sys.argv[1]
    os.makedirs(work, exist_ok=True)
    if os.listdir(work):
        sys.exit(f"{work} is not empty")
    print(f"kernel {os.uname().release}, uid {os.getuid()} in its namespace", flush=True)
    for dest in ("/data", "/tmp/x", "/tmp"):
        member(work, dest)


if __name__ == "__main__":
    main()
