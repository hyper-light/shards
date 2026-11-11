# Finding 11: managed_cleanup.rs Returned::check (unix) ran waitpid/kill on a bare PID after
# the child could already be reaped. Run as PID 1 of a new user+PID namespace
# (unshare -Urpf --mount-proc): ns_last_pid (pid_namespaces(7)) hands A's PID to B, another
# child of this process, as a concurrently running test's child could get it.
import os
import signal


def old_check(pid):
    # managed_cleanup.rs:598-613 at bff3015, step for step.
    waited, _ = os.waitpid(pid, os.WNOHANG)
    rescued = False
    if waited == 0:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        rescued = True
    return waited, rescued


def pidfd_check(pidfd):
    # The fix: waitid(P_PIDFD) on a pidfd opened before any reap.
    try:
        info = os.waitid(os.P_PIDFD, pidfd, os.WEXITED | os.WNOHANG)
        return f"child state {info}"
    except ChildProcessError:
        return "ECHILD: this exact process was reaped"


a = os.fork()
if a == 0:
    os._exit(0)
held = os.pidfd_open(a)  # held from spawn, before anything reaps A
os.waitpid(a, 0)  # the Client reaps A (try_wait or Drop)
with open("/proc/sys/kernel/ns_last_pid", "w") as last:
    last.write(str(a - 1))
b = os.fork()
if b == 0:
    signal.pause()
    os._exit(0)
print(f"A={a} reaped; B={b}, an unrelated child, reuses A's PID: {a == b}")
print(f"pidfd check of A: {pidfd_check(held)}; B alive afterwards: {os.waitpid(b, os.WNOHANG) == (0, 0)}")
waited, rescued = old_check(a)
try:
    os.kill(b, 0)
    b_state = "alive"
except ProcessLookupError:
    b_state = "gone (killed and reaped by the old check)"
print(f"old check of A: waitpid(WNOHANG)={waited} rescue_ran={rescued}; B is {b_state}")
print("old check verdict: assertion 'waited == -1' fails although A was correctly reaped" if waited != -1 else "old check passed")
