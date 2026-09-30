"""The syscalls shards-vm makes, by thread, from `strace -f` output (collect.sh): the
evidence for its seccomp filters (docs/research/rootless-security.md R3).

Each line of strace -f's output starts with the calling thread's id. A thread belongs to
a shards-vm process once its process has exec'd a path ending in /shards-vm; a clone with
CLONE_THREAD adds a thread to its caller's process, and any other clone starts a process
that runs its parent's program until it execs. Threads are named by their
prctl(PR_SET_NAME), as Rust's std names them from the thread itself: `vcpuN` threads are
one class, the process's first thread is `main`, and a thread never named is counted
under `unnamed`.

For each class it prints the syscalls, with how many calls, and the ioctl requests,
socket domains, fcntl commands and prctl options they used.

Thread ids are reused from one strace run to the next, so each trace is read with state
of its own, and the counts are added up.

    python3 parse.py TRACE...
"""
import collections, re, sys

LINE = re.compile(r"^(\d+)\s+(?:<\.\.\. (\w+) resumed>(.*)|(\w+)\((.*))$")
RESULT = re.compile(r"=\s+(-?\d+)")

tgid = {}          # tid -> process id
exe = {}           # process id -> program
name = {}          # tid -> thread name
pending = {}       # tid -> (syscall, args) of an unfinished call
calls = collections.defaultdict(collections.Counter)      # class -> syscall -> count
detail = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))


def klass(tid):
    process = tgid.get(tid, tid)
    if tid == process:
        return "main"
    n = name.get(tid)
    if n is None:
        return "unnamed"
    return re.sub(r"\d+$", "N", n)


def is_vm(tid):
    return exe.get(tgid.get(tid, tid), "").endswith("/shards-vm")


def first_arg(args):
    return args.split(",", 1)[0].strip()


def record(tid, call, args, result):
    if call in ("clone", "clone3", "fork", "vfork") and result is not None and result > 0:
        child = result
        if "CLONE_THREAD" in args:
            tgid[child] = tgid.get(tid, tid)
        else:
            tgid[child] = child
            exe[child] = exe.get(tgid.get(tid, tid), "")
    if call == "execve" and result == 0:
        path = re.match(r'"([^"]*)"', args)
        process = tgid.setdefault(tid, tid)
        exe[process] = path.group(1) if path else ""
    if call == "prctl" and args.startswith("PR_SET_NAME"):
        found = re.search(r'"([^"]*)"', args)
        if found:
            name[tid] = found.group(1)
    if not is_vm(tid):
        return
    c = klass(tid)
    calls[c][call] += 1
    if call == "ioctl":
        parts = args.split(",")
        if len(parts) > 1:
            detail[c]["ioctl"][parts[1].strip().split("(")[0]] += 1
    elif call in ("socket", "socketpair"):
        detail[c][call][first_arg(args)] += 1
    elif call == "fcntl":
        parts = args.split(",")
        if len(parts) > 1:
            detail[c]["fcntl"][parts[1].strip()] += 1
    elif call == "prctl":
        detail[c]["prctl"][first_arg(args)] += 1


for trace in sys.argv[1:]:
  tgid.clear(); exe.clear(); name.clear(); pending.clear()
  for line in open(trace, errors="replace"):
      m = LINE.match(line.rstrip("\n"))
      if not m:
          continue
      tid = int(m.group(1))
      tgid.setdefault(tid, tid)
      if m.group(2):
          call, args = pending.pop(tid, (m.group(2), ""))
          rest = m.group(3)
      else:
          call, rest = m.group(4), m.group(5)
          if rest.endswith("<unfinished ...>"):
              pending[tid] = (call, rest[: -len("<unfinished ...>")].rstrip())
              continue
          args = rest
      found = RESULT.search(rest)
      record(tid, call, args if m.group(4) else args + rest, int(found.group(1)) if found else None)

for c in sorted(calls):
    print(f"## {c}\n")
    print("| syscall | calls |\n|---|---|")
    for call, n in sorted(calls[c].items()):
        print(f"| {call} | {n} |")
    for kind, seen in sorted(detail[c].items()):
        print(f"\n{kind}: " + ", ".join(f"{k} ({n})" for k, n in sorted(seen.items())))
    print()
