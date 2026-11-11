# Finding 11 on macOS, which has no pidfd: is (PID, start time) from proc_pidinfo(2)
# PROC_PIDTBSDINFO a process identity that survives until the reap, zombie included, and is
# gone after it? Argument 1 is xnu's findzomb (bsd/kern/proc_info.c, proc_pidinfo_internal).
import ctypes
import os
import time

libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
PROC_PIDTBSDINFO = 3
SZOMB = 5


class proc_bsdinfo(ctypes.Structure):  # sys/proc_info.h
    _fields_ = [(n, ctypes.c_uint32) for n in (
        "pbi_flags", "pbi_status", "pbi_xstatus", "pbi_pid", "pbi_ppid", "pbi_uid", "pbi_gid",
        "pbi_ruid", "pbi_rgid", "pbi_svuid", "pbi_svgid", "rfu_1")] + [
        ("pbi_comm", ctypes.c_char * 16), ("pbi_name", ctypes.c_char * 32)] + [
        (n, ctypes.c_uint32) for n in ("pbi_nfiles", "pbi_pgid", "pbi_pjobc", "e_tdev", "e_tpgid")] + [
        ("pbi_nice", ctypes.c_int32), ("pbi_start_tvsec", ctypes.c_uint64), ("pbi_start_tvusec", ctypes.c_uint64)]


def info(pid, arg):
    i = proc_bsdinfo()
    n = libc.proc_pidinfo(pid, PROC_PIDTBSDINFO, ctypes.c_uint64(arg), ctypes.byref(i), ctypes.sizeof(i))
    if n != ctypes.sizeof(i):
        return f"n={n} errno={ctypes.get_errno()}"
    return f"started=({i.pbi_start_tvsec},{i.pbi_start_tvusec}) zombie={i.pbi_status == SZOMB}"


print("sizeof proc_bsdinfo:", ctypes.sizeof(proc_bsdinfo))
a = os.fork()
if a == 0:
    os._exit(0)
print(f"A={a} running/exiting, arg 1: {info(a, 1)}")
time.sleep(0.2)  # demonstration only: let A exit, unreaped
print(f"A exited, unreaped: arg 0: {info(a, 0)}; arg 1: {info(a, 1)}")
os.waitpid(a, 0)
print(f"A reaped: arg 1: {info(a, 1)}")
b = os.fork()
if b == 0:
    time.sleep(5)
    os._exit(0)
print(f"B={b} (another process) arg 1: {info(b, 1)}")
os.kill(b, 9)
os.waitpid(b, 0)
