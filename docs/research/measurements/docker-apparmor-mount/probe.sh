#!/bin/sh
# What Docker's default AppArmor profile (docker-default) lets a container with
# CAP_SYS_ADMIN do to mounts, on a host that enforces AppArmor (GitHub's Ubuntu runners),
# for shards' equivalent in its guests, which have no AppArmor. Each container is
# `--rm` and unnamed. Errnos come from python's ctypes, the syscall's own word.
set -u
IMAGE=${IMAGE:-python:3.13-alpine}
say() { printf '== %s\n' "$1"; }
say "host"; uname -srm; docker version --format '{{.Server.Version}}'
say "apparmor"; cat /sys/module/apparmor/parameters/enabled 2>&1; docker info --format '{{.SecurityOptions}}'
docker pull -q "$IMAGE" >/dev/null
# A python program run in the container: each step, its return and errno.
cat > /tmp/steps.py <<'PY'
import ctypes, errno, os, platform
libc = ctypes.CDLL(None, use_errno=True)
def call(name, *args):
    r = getattr(libc, name)(*args)
    e = ctypes.get_errno()
    print(f"{name}: {r} {errno.errorcode.get(e, e) if r != 0 else ''}".rstrip())
def sys(name, nr, *args):
    r = libc.syscall(nr, *args)
    e = ctypes.get_errno()
    print(f"{name}: {r} {errno.errorcode.get(e, e) if r < 0 else ''}".rstrip())
print("profile:", open("/proc/self/attr/current").read().strip())
os.makedirs("/mnt/t", exist_ok=True)
call("mount", b"none", b"/mnt/t", b"tmpfs", 0, None)
call("umount2", b"/mnt/t", 0)
# The new mount API: fsopen(430), open_tree(428), on both x86_64 and arm64 (generic).
sys("fsopen", 430, b"tmpfs", 0)
sys("open_tree", 428, -100, b"/etc", 1)
call("umount2", b"/etc/hostname", 2)
PY
run() {
    say "$1"
    shift
    docker run --rm -v /tmp/steps.py:/steps.py:ro "$@" "$IMAGE" python3 /steps.py 2>&1; echo "exit $?"
}
run "default caps"
run "--cap-add SYS_ADMIN" --cap-add SYS_ADMIN
run "--cap-add SYS_ADMIN, apparmor=unconfined" --cap-add SYS_ADMIN --security-opt apparmor=unconfined
run "--cap-add SYS_ADMIN, seccomp=unconfined" --cap-add SYS_ADMIN --security-opt seccomp=unconfined
run "seccomp=unconfined" --security-opt seccomp=unconfined
run "--privileged" --privileged
say "busybox mount's words, SYS_ADMIN"
docker run --rm --cap-add SYS_ADMIN alpine:3.22 sh -c 'mkdir -p /mnt/t && mount -t tmpfs none /mnt/t'; echo "exit $?"
say "an unknown profile"
docker run --rm --security-opt apparmor=no-such-profile alpine:3.22 true; echo "exit $?"
# The new mount API whole, and what else AppArmor mediates of mounts: a detached tmpfs made
# (fsopen, fsconfig, fsmount) and a file made in it through its descriptor alone, which
# needs no mount point; attaching it, and a clone of /etc, with move_mount; mount_setattr
# of a mount; fspick; pivot_root, which fails on its own checks (EBUSY: "/" is no new root
# here, pivot_root(2)) where AppArmor and seccomp let it through. Numbers from
# asm-generic/unistd.h, the same on x86_64 and arm64 but pivot_root's.
cat > /tmp/detached.py <<'PY'
import ctypes, errno, os, platform
libc = ctypes.CDLL(None, use_errno=True)
def sys(name, nr, *args):
    r = libc.syscall(nr, *args)
    e = ctypes.get_errno()
    print(f"{name}: {r} {errno.errorcode.get(e, e) if r < 0 else ''}".rstrip())
    return r
AT_FDCWD, OPEN_TREE_CLONE, MOVE_MOUNT_F_EMPTY_PATH, FSCONFIG_CMD_CREATE = -100, 1, 4, 6
os.makedirs("/mnt/t", exist_ok=True)
fs = sys("fsopen", 430, b"tmpfs", 0)
if fs >= 0:
    sys("fsconfig create", 431, fs, FSCONFIG_CMD_CREATE, None, None, 0)
    m = sys("fsmount", 432, fs, 0, 0)
    if m >= 0:
        try:
            f = os.open("made", os.O_CREAT | os.O_WRONLY, 0o600, dir_fd=m)
            print("a file in the detached tmpfs: made")
            os.close(f)
        except OSError as e:
            print(f"a file in the detached tmpfs: {errno.errorcode.get(e.errno, e.errno)}")
        sys("move_mount detached", 429, m, b"", AT_FDCWD, b"/mnt/t", MOVE_MOUNT_F_EMPTY_PATH)
t = sys("open_tree clone", 428, AT_FDCWD, b"/etc", OPEN_TREE_CLONE)
if t >= 0:
    sys("move_mount clone", 429, t, b"", AT_FDCWD, b"/mnt/t", MOVE_MOUNT_F_EMPTY_PATH)
# struct mount_attr { u64 attr_set, attr_clr, propagation, userns_fd }: MOUNT_ATTR_RDONLY.
attr = (ctypes.c_uint64 * 4)(1, 0, 0, 0)
sys("mount_setattr", 442, AT_FDCWD, b"/etc/hostname", 0, ctypes.byref(attr), 32)
sys("fspick", 433, AT_FDCWD, b"/etc/hostname", 0)
sys("pivot_root", {"x86_64": 155, "aarch64": 41}[platform.machine()], b"/", b"/")
PY
detached() {
    say "detached, $1"
    shift
    docker run --rm -v /tmp/detached.py:/detached.py:ro "$@" "$IMAGE" python3 /detached.py 2>&1; echo "exit $?"
}
detached "--cap-add SYS_ADMIN" --cap-add SYS_ADMIN
detached "--cap-add SYS_ADMIN, apparmor=unconfined" --cap-add SYS_ADMIN --security-opt apparmor=unconfined
detached "default caps"
