#!/usr/bin/env python3
"""A read held short of its budget for its whole read at each least room, beside the same
read with no budget, by runner CPU (D188): the DAX budget job's runs of measure.py's
`short` budget, as the FUSE budget E2E's phases set it (--as-e2e) or with a pool of none,
its arms the tree's least room (`none`) and builds whose least room is 2 to 16 times it
(least-x*.patch). For each CPU, room, budget and file:
n, p50, p90, p99 and max of the read's seconds; how many of the short reads the daemon's
log says were held short throughout; per MiB read, the guest's reclaim in the run's cgroup
(pages scanned and taken, and memory.events' `high`, the times the run went past its
hold); the slice's refaults of file pages, the template's among them, per read; where the
guest's dd waited, and where the VM process's threads other than its vCPUs waited, by
wchan, in the samples' looks; and in the slice's looks during the short reads, what
its budget had left below it (memory.high less memory.current, short of none where it was
past it), its file pages processes mapped (its microVM's template's among them) and its
anonymous memory.

    rooms.py [--after N] RUN...   (each run's job logs fetched, its artifacts kept in
                                   ./art-RUN-COPY; --after N leaves out rounds 1 to N, as
                                   the first rounds of runs before 99f68dc7 booted cold)
"""
import collections, json, os, re, subprocess, sys

MIB_READ = 1024


def sh(*args):
    return subprocess.run(args, capture_output=True, text=True).stdout


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def summary(xs, digits=1):
    xs = [x for x in xs if x is not None]
    if not xs:
        return "none"
    return (f"n {len(xs):2} p50 {pct(xs, .5):8.{digits}f} p90 {pct(xs, .9):8.{digits}f} "
            f"p99 {pct(xs, .99):8.{digits}f} max {max(xs):8.{digits}f}")


def guest(path):
    """The run's cgroup's reclaim over the read (pages scanned and taken, `high` events) and
    where dd waited in each look, from the guest's samples."""
    first = last = None
    waits = collections.Counter()
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return None
    for line in lines:
        parts = line.split("|")
        if len(parts) < 2:
            continue
        head, cg = parts[0].split(), parts[1].split()
        if "dd" in head:
            for w in head[head.index("dd") + 1 : head.index("dd") + 6]:
                if w not in ("-", "0"):
                    waits[w] += 1
        if cg and cg[0] == "current":
            pairs = dict(zip(cg[2::2], cg[3::2]))
            if all(k in pairs for k in ("pgscan", "pgsteal", "high")):
                now = {k: int(pairs[k]) for k in ("pgscan", "pgsteal", "high")}
                first = first or now
                last = now
    if not first:
        return None
    return {k: last[k] - first[k] for k in first}, waits


def host(path):
    """The slice's file refaults over the read, where the VM processes' threads other than
    their vCPUs waited in each look, and each look's memory: whether the slice was at its
    budget, its mapped file pages and its anonymous memory, from the slice's samples."""
    first = last = None
    waits = collections.Counter()
    looks = []
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return None
    for line in lines:
        words = line.split()
        if not words:
            continue
        if words[0] == "==" and len(words) >= 6 and words[3].isdigit() and words[5].isdigit():
            looks.append({"below": int(words[5]) - int(words[3])})
        elif words[0] == "threads":
            for t in words[2:]:
                name, _, rest = t.partition(":")
                state, _, wchan = rest.partition(":")
                # memory.high's throttle sleeps killable, in D (mm/memcontrol.c).
                if not name.startswith("vcpu") and state == "D":
                    waits[wchan] += 1
        elif "workingset_refault_file" in words:
            pairs = dict(zip(words[::2], words[1::2]))
            for k in ("anon", "file_mapped"):
                if looks and pairs.get(k, "").isdigit():
                    looks[-1][k] = int(pairs[k])
            if pairs.get("workingset_refault_file", "None").isdigit():
                v = int(pairs["workingset_refault_file"])
                first = v if first is None else first
                last = v
    if first is None:
        return None
    return last - first, waits, looks


args = sys.argv[1:]
after = 0
if args[:1] == ["--after"]:
    after, args = int(args[1]), args[2:]
rows = []
failures = collections.defaultdict(list)
for run in args:
    jobs = json.loads(sh("gh", "run", "view", run, "-R", "hyper-light/shards", "--json", "jobs"))["jobs"]
    for job in jobs:
        copy = job["name"].split()[-1].rstrip(")")
        log = sh("gh", "api", "--allow-escape-sequences",
                 f"repos/hyper-light/shards/actions/jobs/{job['databaseId']}/logs")
        log = re.sub(r"\x1b\[[0-9;]*m", "", log)
        cpu = next((l.split("Model name:")[1].strip() for l in log.splitlines()
                    if "Model name:" in l and "lscpu" not in l), "?")
        art = f"art-{run}-{copy}"
        if not os.path.isdir(art):
            subprocess.run(["gh", "run", "download", run, "-R", "hyper-light/shards", "-n",
                            f"dax-budget-x86_64-{copy}", "-D", art], capture_output=True)
        verdicts = {m[1]: m[3] for m in re.finditer(r"(?m)(\S+): budget (\d+) KiB; ([^\n]*)$", log)}
        # Reads whose run did not start or read (--as-e2e), by budget and arm.
        for m in re.finditer(r"(?m)^(infinity|past128|late|short)-(.+)-(sparse|written)-\d+(?:-\d+)?: failed: (.*)$", log):
            failures[(cpu, m[2], m[1])].append(m[4][:120])
        # Each read's name after its seconds where a round holds more than one of a budget
        # (measure.py --as-e2e).
        for m in re.finditer(r"round (\d+) (\S+) (\S+) (\S+): ([\d.]+) s(?: (\S+))?", log):
            rnd, high, arm, file, secs, named = m.groups()
            if int(rnd) <= after:
                continue
            name = named or f"{high}-{arm}-{file}-{rnd}"
            rows.append({"run": run, "copy": copy, "cpu": cpu, "high": high, "arm": arm, "file": file,
                         "secs": float(secs), "verdict": verdicts.get(name),
                         "guest": guest(os.path.join(art, name + ".guest")),
                         "host": host(os.path.join(art, name + ".txt"))})

for r in rows:
    g = r["guest"][0] if r["guest"] else {}
    print(f"{r['run']} {r['copy']} {r['cpu'][:22]:22} {r['arm']:10} {r['high']:8} {r['file']:7} {r['secs']:7.2f} s "
          f"high {g.get('high')} pgsteal {g.get('pgsteal')} refaults {r['host'][0] if r['host'] else None} "
          f"{(r['verdict'] or '').split(';')[0]}")
print()
landed = collections.defaultdict(set)
for r in rows:
    landed[r["cpu"]].add((r["run"], r["copy"]))
for cpu in sorted({r["cpu"] for r in rows}):
    print(f"== {cpu} ({len(landed[cpu])} runners)")
    arms = sorted({r["arm"] for r in rows if r["cpu"] == cpu},
                  key=lambda a: int(a.split("x")[-1]) if "-x" in a else 1)
    for arm in arms:
        for file in ("sparse", "written"):
            for high in ("short", "late", "past128", "infinity"):
                sel = [r for r in rows if r["cpu"] == cpu and r["arm"] == arm and r["file"] == file and r["high"] == high]
                if not sel:
                    continue
                lost = failures.get((cpu, arm, high), [])
                print(f"  {arm:10} {file:7} {high:8} s {summary([r['secs'] for r in sel], 2)}"
                      + (f"; failed {len(lost)}: {lost[0]}" if lost else ""))
                if high != "short":
                    continue
                held = [r for r in sel if r["verdict"] and r["verdict"].startswith("held short throughout")]
                print(f"    held short throughout: {len(held)} of {len(sel)}")
                gs = [r["guest"][0] for r in sel if r["guest"]]
                for k in ("high", "pgsteal", "pgscan"):
                    print(f"    guest {k} a MiB  {summary([g[k] / MIB_READ for g in gs], 1)}")
                print(f"    slice refaults  {summary([r['host'][0] for r in sel if r['host']], 0)}")
                dd, threads = collections.Counter(), collections.Counter()
                for r in sel:
                    if r["guest"]:
                        dd.update(r["guest"][1])
                    if r["host"]:
                        threads.update(r["host"][1])
                print(f"    dd waits        {dd.most_common(4)}")
                print(f"    VM threads wait {threads.most_common(4)}")
                looks = [l for r in sel if r["host"] for l in r["host"][2]]
                print(f"    below its budget KiB a look {summary([l['below'] / 1024 for l in looks], 0)}")
                for k in ("file_mapped", "anon"):
                    print(f"    its {k} MiB a look {summary([l[k] / (1 << 20) for l in looks if k in l], 1)}")
