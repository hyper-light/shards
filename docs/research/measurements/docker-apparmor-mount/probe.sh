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
