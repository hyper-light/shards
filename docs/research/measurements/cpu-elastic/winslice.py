#!/usr/bin/env python3
"""slice.py's twin for Windows (D191's Windows candidate): from outside, while it runs, gives
the vCPU threads of one home's VM processes a priority of their own, so that phases.py can
compare one build's starts on a contended host with and without it, two homes of the same
build, as M440 compared Linux's slice before building it. Every --every ms it looks at each
`shards-vm.exe` process descended from HOME's daemon (HOME\\daemon.pid; Toolhelp's process
snapshot gives each process's parent) and sets each of its threads named `vcpu*` (with --all,
every thread; GetThreadDescription reads the names Rust's std gives them) it has not set yet:

- --priority above_normal (the candidate's, base priority 9 in a NORMAL_PRIORITY_CLASS
  process against its host's threads' 8), highest (10), or normal (as now): SetThreadPriority.
  A thread already above it (the hold's governor, at TIME_CRITICAL) is left as it is.
- --high-qos: power throttling off as well (SetThreadInformation, ThreadPowerThrottling,
  THREAD_POWER_THROTTLING_EXECUTION_SPEED off: HighQoS), which changes the cores and clock a
  thread runs at, not when it runs (Microsoft, Quality of Service): to tell the two apart.

What this sets stays: unlike the candidate built in (platform/windows_thread.rs), no governor
moves a thread that runs long back to normal, which a start's measurement does not need.

    winslice.py HOME [--priority above_normal|highest|normal] [--high-qos] [--every MS] [--all]

Runs until killed (Ctrl-C, or a terminate: the counts are printed at each look's end too);
prints how many threads it set, the most vCPU threads one VM process had, and the first VM
process's CPU affinity mask, as it ends."""
import argparse, ctypes, os, signal, sys, time
from ctypes import wintypes

p = argparse.ArgumentParser()
p.add_argument("home")
p.add_argument("--priority", choices=["above_normal", "highest", "normal"], default="above_normal")
p.add_argument("--high-qos", action="store_true")
p.add_argument("--every", type=float, default=5)
p.add_argument("--all", action="store_true")
a = p.parse_args()

PRIORITY = {"normal": 0, "above_normal": 1, "highest": 2}[a.priority]
TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD = 0x2, 0x4
THREAD_SET_INFORMATION, THREAD_SET_LIMITED_INFORMATION = 0x20, 0x400
THREAD_QUERY_LIMITED_INFORMATION = 0x800
PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
ThreadPowerThrottling, THREAD_POWER_THROTTLING_EXECUTION_SPEED = 3, 0x1
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value


class PROCESSENTRY32W(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
                ("th32ProcessID", wintypes.DWORD), ("th32DefaultHeapID", ctypes.c_size_t),
                ("th32ModuleID", wintypes.DWORD), ("cntThreads", wintypes.DWORD),
                ("th32ParentProcessID", wintypes.DWORD), ("pcPriClassBase", wintypes.LONG),
                ("dwFlags", wintypes.DWORD), ("szExeFile", wintypes.WCHAR * 260)]


class THREADENTRY32(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
                ("th32ThreadID", wintypes.DWORD), ("th32OwnerProcessID", wintypes.DWORD),
                ("tpBasePri", wintypes.LONG), ("tpDeltaPri", wintypes.LONG), ("dwFlags", wintypes.DWORD)]


class THREAD_POWER_THROTTLING_STATE(ctypes.Structure):
    _fields_ = [("Version", wintypes.ULONG), ("ControlMask", wintypes.ULONG), ("StateMask", wintypes.ULONG)]


k = ctypes.WinDLL("kernel32", use_last_error=True)
k.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
k.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
for f, s in (("Process32FirstW", PROCESSENTRY32W), ("Process32NextW", PROCESSENTRY32W),
             ("Thread32First", THREADENTRY32), ("Thread32Next", THREADENTRY32)):
    getattr(k, f).argtypes = [wintypes.HANDLE, ctypes.POINTER(s)]
k.OpenThread.restype = wintypes.HANDLE
k.OpenThread.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
k.OpenProcess.restype = wintypes.HANDLE
k.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
k.CloseHandle.argtypes = [wintypes.HANDLE]
k.GetThreadPriority.argtypes = [wintypes.HANDLE]
k.SetThreadPriority.argtypes = [wintypes.HANDLE, ctypes.c_int]
k.SetThreadInformation.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
k.GetThreadDescription.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.LPWSTR)]
k.GetThreadDescription.restype = ctypes.c_long
k.LocalFree.argtypes = [ctypes.c_void_p]
k.GetProcessAffinityMask.argtypes = [wintypes.HANDLE, ctypes.POINTER(ctypes.c_size_t),
                                     ctypes.POINTER(ctypes.c_size_t)]

home = os.path.realpath(a.home)
done, failed, vcpus, mask = set(), 0, {}, None
stop = False


def ended(*_):
    globals()["stop"] = True


signal.signal(signal.SIGINT, ended)
signal.signal(signal.SIGTERM, ended)


def snapshot():
    """Each process's parent and name, and each process's threads."""
    snap = k.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS | TH32CS_SNAPTHREAD, 0)
    if snap in (None, INVALID_HANDLE_VALUE):
        return {}, {}
    procs, threads = {}, {}
    try:
        pe = PROCESSENTRY32W(dwSize=ctypes.sizeof(PROCESSENTRY32W))
        ok = k.Process32FirstW(snap, ctypes.byref(pe))
        while ok:
            procs[pe.th32ProcessID] = (pe.th32ParentProcessID, pe.szExeFile.lower())
            ok = k.Process32NextW(snap, ctypes.byref(pe))
        te = THREADENTRY32(dwSize=ctypes.sizeof(THREADENTRY32))
        ok = k.Thread32First(snap, ctypes.byref(te))
        while ok:
            threads.setdefault(te.th32OwnerProcessID, []).append(te.th32ThreadID)
            ok = k.Thread32Next(snap, ctypes.byref(te))
    finally:
        k.CloseHandle(snap)
    return procs, threads


def daemon():
    try:
        return int(open(os.path.join(home, "daemon.pid")).read().split()[0])
    except (OSError, ValueError, IndexError):
        return None


def descends(pid, ancestor, procs):
    seen = set()
    while pid in procs and pid not in seen:
        seen.add(pid)
        parent = procs[pid][0]
        if parent == ancestor:
            return True
        pid = parent
    return False


def name_of(h):
    text = wintypes.LPWSTR()
    if k.GetThreadDescription(h, ctypes.byref(text)) < 0:
        return ""
    name = text.value or ""
    k.LocalFree(text)
    return name


def affinity(pid):
    h = k.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return None
    process, system = ctypes.c_size_t(), ctypes.c_size_t()
    got = k.GetProcessAffinityMask(h, ctypes.byref(process), ctypes.byref(system))
    k.CloseHandle(h)
    return hex(process.value) if got else None


def report():
    print(f"winslice.py: {len(done) - failed} threads set to {a.priority}"
          f"{' and HighQoS' if a.high_qos else ''}, {failed} failed; at most {max(vcpus.values(), default=0)} vCPU threads "
          f"a VM process; the first VM process's affinity {mask}", flush=True)


access = THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_LIMITED_INFORMATION
if a.high_qos:
    access |= THREAD_SET_INFORMATION
qos = THREAD_POWER_THROTTLING_STATE(1, THREAD_POWER_THROTTLING_EXECUTION_SPEED, 0)
looks = 0
while not stop:
    ancestor = daemon()
    procs, threads = snapshot() if ancestor else ({}, {})
    for pid, (_, exe) in procs.items():
        if exe != "shards-vm.exe" or not descends(pid, ancestor, procs):
            continue
        if mask is None:
            mask = affinity(pid)
        for tid in threads.get(pid, []):
            if (pid, tid) in done:
                continue
            h = k.OpenThread(access, False, tid)
            if not h:
                continue
            try:
                name = name_of(h)
                if name.startswith("vcpu"):
                    vcpus[pid] = vcpus.get(pid, 0) + 1
                elif not a.all:
                    continue
                done.add((pid, tid))
                if k.GetThreadPriority(h) > PRIORITY:
                    continue
                set_ok = k.SetThreadPriority(h, PRIORITY)
                if set_ok and a.high_qos:
                    set_ok = k.SetThreadInformation(h, ThreadPowerThrottling, ctypes.byref(qos),
                                                    ctypes.sizeof(qos))
                if not set_ok:
                    failed += 1
                    if failed <= 3:
                        print(f"thread {tid} ({name}) of {pid}: error {ctypes.get_last_error()}",
                              file=sys.stderr)
            finally:
                k.CloseHandle(h)
    looks += 1
    if looks % 2000 == 0:
        report()
    time.sleep(a.every / 1000)
report()
