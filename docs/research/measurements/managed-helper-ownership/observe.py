#!/usr/bin/env python3
"""One actual owned helper and ready QuickJS worker, independently held by pidfd."""
from pathlib import Path
import array
import ctypes
import json
import os
import select
import signal
import socket
import struct
import subprocess
import sys

binary, directory, mode, within = sys.argv[1:]
root = Path(directory)
within = int(within)
assert within > 0 and mode in ("death", "control-error")
libc = ctypes.CDLL(None, use_errno=True)
# Only this disposable fixture becomes the subreaper of its own descendants.
assert libc.prctl(36, 1, 0, 0, 0) == 0  # PR_SET_CHILD_SUBREAPER, linux/prctl.h.
record = {"binary": str(Path(binary).resolve()), "mode": mode,
          "notification_budget_seconds": within, "effects": {}, "cleanup": {}}
helper = None
helper_fd = worker_fd = worker_pid = None
control = theirs = ours = program = None
fd_before = len(list(Path("/proc/self/fd").iterdir()))


def ready(fd, milliseconds):
    watch = select.poll()
    watch.register(fd, select.POLLIN | select.POLLHUP | select.POLLERR)
    return watch.poll(milliseconds)


def received(sock, count):
    out = bytearray()
    while len(out) < count:
        assert ready(sock.fileno(), within * 1000), "notification deadline"
        chunk = sock.recv(count - len(out))
        assert chunk, ("unexpected EOF", len(out), count)
        out.extend(chunk)
    return bytes(out)


def frame(sock):
    head = received(sock, 5)
    kind, length = struct.unpack("!cI", head)
    return kind.decode("ascii"), received(sock, length)


def send_frame(sock, kind, body):
    sock.sendall(struct.pack("!cI", kind, len(body)) + body)


try:
    control, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    ours, program = socket.socketpair()
    with (root / (mode + ".stderr.log")).open("wb") as log:
        helper = subprocess.Popen([binary, str(theirs.fileno())], pass_fds=(theirs.fileno(),),
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                  stderr=log)
    helper_fd = os.pidfd_open(helper.pid)
    theirs.close()
    control.sendmsg([b"R" + struct.pack("!Q", 1)],
                    [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [program.fileno()]))])
    program.close()
    send_frame(ours, b"P", b"return await shards.receive();")
    kind, call = frame(ours)
    assert kind == "C" and json.loads(call) == {"name": "receive", "args": {}}
    # One read after the real worker CALL, never a process-name search or PID poll.
    children = (Path("/proc") / str(helper.pid) / "task" / str(helper.pid) / "children").read_text().split()
    assert len(children) == 1, children
    worker_pid = int(children[0])
    worker_fd = os.pidfd_open(worker_pid)
    record["effects"]["ready"] = {"helper": helper.pid, "worker": worker_pid,
        "tasks": len(list((Path("/proc") / str(helper.pid) / "task").iterdir())),
        "call": json.loads(call), "worker_pidfd_initial": ready(worker_fd, 0)}
    assert not ready(worker_fd, 0)
    if mode == "death":
        signal.pidfd_send_signal(helper_fd, signal.SIGKILL)
    else:
        control.send(b"?")
    record["effects"]["helper_exit"] = helper.wait(timeout=within)
    # Notification wait bounds this observation; it is not a universal lifetime proof.
    death_event = ready(worker_fd, within * 1000)
    record["effects"]["worker_death_notification"] = death_event
    if not death_event:
        # Demonstrate actual execution after owner death rather than infer a leak
        # merely from absent readiness or an alive-looking process table entry.
        send_frame(ours, b"R", b'{"value":"completed after helper exit"}')
        kind, answer = frame(ours)
        record["effects"]["after_helper_exit"] = {"kind": kind, "body": answer.decode()}
        assert ready(worker_fd, within * 1000), "worker did not end after answer"
    if death_event:
        assert ready(ours.fileno(), within * 1000), "worker socket did not retire"
        try:
            assert ours.recv(1) == b"", "worker emitted bytes after helper exit"
            record["effects"]["worker_socket_terminal"] = "EOF"
        except ConnectionResetError as error:
            record["effects"]["worker_socket_terminal"] = {"reset": error.errno}
    try:
        got, status = os.waitpid(worker_pid, 0)
        record["cleanup"]["worker_waitpid"] = {"pid": got, "status": status}
        if death_event:
            assert os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGKILL
    except ChildProcessError as error:
        # A controlled helper error now reaps its child itself. ECHILD alone
        # proves nothing: require independent held-pidfd HUP and Pid:-1 too.
        assert mode == "control-error" and death_event
        retired = ready(worker_fd, 0)
        assert any(events & select.POLLHUP for _, events in retired), retired
        fields = dict(line.split(":", 1) for line in
                      (Path("/proc/self/fdinfo") / str(worker_fd)).read_text().splitlines())
        assert fields.get("Pid", "").strip() == "-1", fields
        record["cleanup"]["helper_reaped"] = {"waitpid_errno": error.errno,
            "pidfd_events": retired, "pidfd_pid": fields["Pid"].strip(),
            "exit_status": "not observed by nonparent fixture"}
    worker_pid = None
    record["passed"] = bool(death_event)
except BaseException as error:
    record["fixture_error"] = {"type": type(error).__name__, "text": str(error)}
    if control is not None and ready(control.fileno(), within * 1000):
        # The existing ENDED wire is kind + u64 ID + i32 code + i32 status.
        packet = control.recv(struct.calcsize("!cQii"))
        if len(packet) == struct.calcsize("!cQii"):
            kind, identity, code, status = struct.unpack("!cQii", packet)
            record["effects"]["control_after_failure"] = {
                "kind": kind.decode("ascii"), "id": identity, "code": code, "status": status}
        else:
            record["effects"]["unexpected_control_bytes"] = len(packet)
    raise
finally:
    if helper is not None and helper.poll() is None:
        signal.pidfd_send_signal(helper_fd, signal.SIGKILL)
        record["cleanup"]["helper_cancel_exit"] = helper.wait(timeout=within)
    if worker_pid is not None and worker_fd is not None:
        try:
            signal.pidfd_send_signal(worker_fd, signal.SIGKILL)
        except ProcessLookupError:
            pass
        assert ready(worker_fd, within * 1000), "cleanup did not observe worker exit"
        got, status = os.waitpid(worker_pid, 0)
        record["cleanup"]["worker_cancel_waitpid"] = {"pid": got, "status": status}
    # If setup failed before capturing the worker, reparenting to this fixture's
    # own subreaper makes any just-born child visible after helper.wait(). There
    # is one RUN and no unrelated child here; never scan or signal global PIDs.
    left = (Path("/proc/self/task") / str(os.getpid()) / "children").read_text().split()
    late = []
    for text in left:
        pid = int(text)
        held = os.pidfd_open(pid)
        try:
            signal.pidfd_send_signal(held, signal.SIGKILL)
            assert ready(held, within * 1000), "late-child cleanup deadline"
            got, status = os.waitpid(pid, 0)
            late.append({"pid": got, "status": status})
        finally:
            os.close(held)
    record["cleanup"]["late_owned_children"] = late
    for item in (control, theirs, ours, program):
        if item is not None:
            item.close()
    for fd in (helper_fd, worker_fd):
        if fd is not None:
            os.close(fd)
    record["cleanup"]["fd_before"] = fd_before
    record["cleanup"]["fd_after"] = len(list(Path("/proc/self/fd").iterdir()))
    record["cleanup"]["limit"] = "Held-helper/worker waits and own FDs only; no cgroup/kernel cost proof."
    (root / (mode + ".json")).write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record), flush=True)
assert record["passed"], "ready worker executed after its helper exited"
