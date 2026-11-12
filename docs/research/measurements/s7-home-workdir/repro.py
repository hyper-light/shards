"""A member's root and home as shards-init makes them (crates/init/src/domains.rs `skeleton`,
`image_root`, `scratch` and `start`, staged.rs `files`, then `child`'s mounts, at ad600dd52),
replayed with the same system calls in a user and mount namespace, for two cases of kiln
plan 01a1cd8e862e7169a074db3d5fa33f3d step 7:

- parents: a volume at /home/node/.cache, where the image has /home/node 1000:1000 0750.
  What owner and mode does the member's /home/node show?
- home: HOME=/home/claude (a harness.json's run.env, 997f4e47e) on a volume given there, and
  `.claude/.credentials.json` staged for the member (D231). Where does it land?

Run as `unshare --map-auto --map-root-user --mount python3 -I repro.py DIR` on Linux 5.12
or later, DIR empty or absent: --map-auto maps the subordinate IDs, so that the image's
directory can be another user's. The namespace's root stands in for init.
"""

import ctypes
import errno
import os
import stat
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


def skeleton(lower_fd, dirs):
    """domains.rs `skeleton` at ad600dd52: proc, dev, sys and tmp where the image lacks them,
    etc always, then every component of each mount point (`dirs`), each 0755 in the tmpfs."""
    skel = detached("tmpfs", [("mode", "0755")])

    def lacks(path):
        try:
            os.stat(path, dir_fd=lower_fd, follow_symlinks=False)
            return False
        except FileNotFoundError:
            return True

    for d in ("proc", "dev", "sys", "tmp", "etc"):
        if lacks(d) or d == "etc":
            os.mkdir(d, 0o755, dir_fd=skel)
    for target in dirs:
        parts = target.strip("/").split("/")
        for i in range(1, len(parts) + 1):
            try:
                os.mkdir("/".join(parts[:i]), 0o755, dir_fd=skel)
            except FileExistsError:
                pass
    return skel


def image_root(skel, lower_fd):
    """domains.rs `image_root` without plug roots: the skeleton over the image, read-only."""
    layers = f"/proc/self/fd/{skel}:/proc/self/fd/{lower_fd}"
    return detached("overlay", [("lowerdir", layers), ("xino", "on")])


def shown(path):
    st = os.lstat(path)
    return f"{st.st_uid}:{st.st_gid} {stat.S_IMODE(st.st_mode):04o}"


def parents(work):
    say = lambda *a: print("  [parents]", *a, flush=True)
    lower, vol, root_at = (os.path.join(work, "parents", n) for n in ("lower", "vol", "root"))
    for d in (os.path.join(lower, "home", "node"), os.path.join(lower, "tmp"), os.path.join(lower, "etc"), vol, root_at):
        os.makedirs(d)
    os.chown(os.path.join(lower, "home", "node"), 1000, 1000)
    os.chmod(os.path.join(lower, "home", "node"), 0o750)
    lower_fd = os.open(lower, os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
    skel = skeleton(lower_fd, ["home/node/.cache"])
    attach(image_root(skel, lower_fd), root_at)
    for p in ("/home", "/home/node"):
        say(f"the image's {p}: {shown(lower + p)}; the member's: {shown(root_at + p)}")
    attach(copy_of(vol), root_at + "/home/node/.cache")
    say(f"the volume mounted at /home/node/.cache: {os.path.ismount(root_at + '/home/node/.cache')}")


def home(work):
    say = lambda *a: print("  [home]", *a, flush=True)
    lower, vol, root_at = (os.path.join(work, "home", n) for n in ("lower", "vol", "root"))
    for d in (os.path.join(lower, "home"), os.path.join(lower, "tmp"), os.path.join(lower, "etc"), vol, root_at):
        os.makedirs(d)
    lower_fd = os.open(lower, os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
    skel = skeleton(lower_fd, ["home/claude"])
    root = image_root(skel, lower_fd)
    scratch = detached("tmpfs", [("mode", "0750"), ("uid", "0"), ("gid", "0")], MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV)
    # staged.rs `files`, as `start` calls it: beneath the scratch's descriptor, its
    # directories 0700, the file O_EXCL and O_NOFOLLOW.
    os.mkdir(".claude", 0o700, dir_fd=scratch)
    d = os.open(".claude", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=scratch)
    f = os.open(".credentials.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=d)
    os.write(f, b'{"token":"staged"}\n')
    os.close(f)
    os.close(d)
    volume = copy_of(vol)
    # `child`: the root, the scratch at /tmp, then the volume at its mount point.
    attach(root, root_at)
    attach(scratch, root_at + "/tmp")
    attach(volume, root_at + "/home/claude")
    member_home = "/home/claude"
    say(f"HOME={member_home}, a volume there: {os.path.ismount(root_at + member_home)}")
    for p in (member_home + "/.claude/.credentials.json", "/tmp/.claude/.credentials.json"):
        say(f"the member's {p} exists: {os.path.exists(root_at + p)}")
    say(f"the volume itself holds .claude/.credentials.json: {os.path.exists(os.path.join(vol, '.claude/.credentials.json'))}")


def main():
    work = sys.argv[1]
    os.makedirs(work, exist_ok=True)
    if os.listdir(work):
        sys.exit(f"{work} is not empty")
    print(f"kernel {os.uname().release}, uid {os.getuid()} in its namespace", flush=True)
    for case in (parents, home):
        try:
            case(work)
        except OSError as e:
            print(f"  [{case.__name__}] failed: {errno.errorcode.get(e.errno, e.errno)} ({e.strerror})", flush=True)


if __name__ == "__main__":
    main()
