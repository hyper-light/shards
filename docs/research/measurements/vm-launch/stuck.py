# What a VM process waiting in its launch shows of itself (PM M166): a fresh copy of
# VM_BINARY is launched while the host assesses a backlog of other new executables, and
# while it has sent nothing on its grants socket, its task info (proc_pidinfo
# PROC_PIDTASKINFO) is read every 20 ms: its user and system time, threads and syscalls.
# Then the same once it has asked. Whether a process the host has not started running is
# told apart from one running and stuck.
#   python3 -I stuck.py VM_BINARY KERNEL OUT_DIR BACKLOG
import ctypes, os, select, shutil, signal, socket, sys, threading, time

vm_given, kernel, out, backlog = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
os.makedirs(out, exist_ok=True)
libproc = ctypes.CDLL("/usr/lib/libproc.dylib")

class TaskInfo(ctypes.Structure):
    _fields_ = [("virtual_size", ctypes.c_uint64), ("resident_size", ctypes.c_uint64),
                ("total_user", ctypes.c_uint64), ("total_system", ctypes.c_uint64),
                ("threads_user", ctypes.c_uint64), ("threads_system", ctypes.c_uint64),
                ("policy", ctypes.c_int32), ("faults", ctypes.c_int32), ("pageins", ctypes.c_int32),
                ("cow_faults", ctypes.c_int32), ("messages_sent", ctypes.c_int32),
                ("messages_received", ctypes.c_int32), ("syscalls_mach", ctypes.c_int32),
                ("syscalls_unix", ctypes.c_int32), ("csw", ctypes.c_int32), ("threadnum", ctypes.c_int32),
                ("numrunning", ctypes.c_int32), ("priority", ctypes.c_int32)]

def info(pid):
    t = TaskInfo()
    n = libproc.proc_pidinfo(pid, 4, ctypes.c_uint64(0), ctypes.byref(t), ctypes.sizeof(t))
    if n != ctypes.sizeof(t):
        return None
    return (t.total_user, t.total_system, t.threadnum, t.syscalls_unix, t.syscalls_mach)

def spawn(path):
    ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    pid = os.posix_spawn(path, [path, "run", "--kernel", kernel, "--grants", "3"], {},
                         file_actions=[(os.POSIX_SPAWN_DUP2, theirs.fileno(), 3)])
    theirs.close()
    return pid, ours

def end(pid, ours):
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)
    ours.close()

# The backlog: fresh copies launched at once, left to the host to assess.
held = []
for i in range(backlog):
    d = os.path.join(out, "backlog", str(i))
    os.makedirs(d, exist_ok=True)
    p = os.path.join(d, "shards-vm")
    shutil.copy2(vm_given, p)
    held.append(spawn(p))
d = os.path.join(out, "probe")
os.makedirs(d, exist_ok=True)
probe = os.path.join(d, "shards-vm")
shutil.copy2(vm_given, probe)
began = time.monotonic()
pid, ours = spawn(probe)
while True:
    ready, _, _ = select.select([ours], [], [], 0.02)
    t = time.monotonic() - began
    i = info(pid)
    if ready:
        print(f"{t*1000:9.1f} ms  asked   user_ns, system_ns, threads, unix syscalls, mach syscalls = {i}")
        break
    print(f"{t*1000:9.1f} ms  waiting user_ns, system_ns, threads, unix syscalls, mach syscalls = {i}")
    if t > 120:
        break
end(pid, ours)
for p, o in held:
    end(p, o)
