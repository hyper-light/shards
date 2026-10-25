#!/usr/bin/env python3
"""A read held short of its budget for its whole read at each least room, beside the same
read with no budget, by runner CPU (D188): the DAX budget job's runs of measure.py's
`short` budget, as the FUSE budget E2E's phases set it (--as-e2e) or with a pool of none,
its arms the tree's least room (`none`) and builds whose least room is 2 to 16 times it
(least-x*.patch). For each CPU, room, budget and file:
n, p50, p90, p99 and max of the read's seconds; how many of the short reads the daemon's
log says were held short throughout, and their seconds, their guest's pages taken a MiB and
their slice's refaults; per MiB read, the guest's reclaim in the run's cgroup
(pages scanned and taken, and memory.events' `high`, the times the run went past its
hold); the slice's refaults of file pages, the template's among them, per read; where the
guest's dd waited, and where the VM process's threads other than its vCPUs waited, by
wchan, in the samples' looks; and in the slice's looks during the short reads, what
its budget had left below it (memory.high less memory.current, short of none where it was
past it), its file pages processes mapped (its microVM's template's among them), its
anonymous memory, its VM processes' resident files and how much of their template's memory
file its host's page cache held, with how much of it each read saw taken, and the share's
copy of the guests' files there at its most, the guest's refaults and the looks that found
its dd held at its run's memory.high, and the whole host's reclaim (each read's row says its
own); and how many of the short reads ran away, two looks or more finding its
mapped files under 5 MiB and its anonymous memory within 16 MiB of its budget: its guest's
page cache filling the budget, its host taking back its RAM, which its guest read back (PM
M318).

    rooms.py [--after N] RUN...   (each run's job logs fetched, its artifacts kept in
                                   ./art-RUN-COPY; --after N leaves out rounds 1 to N, as
                                   the first rounds of runs before 99f68dc7 booted cold)
"""
import collections, json, os, re, subprocess, sys

MIB_READ = 1024
# A look whose slice's mapped files are under this many bytes has had its RAM taken back.
MIB_RAN = 5 << 20


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


def zones_of(words):
    """Each zone's free pages, min watermark and managed pages, from a look's `zones` part
    (measure.py GUEST_LIGHT): {zone: {"free": n, "min": n, "managed": n}}."""
    zones, name = {}, None
    it = iter(words)
    for w in it:
        if w in ("free", "min", "managed"):
            v = next(it, "")
            if name is not None and v.isdigit():
                zones[name][w] = int(v)
        elif not w.isdigit():
            name = w
            zones[name] = {}
    return zones


def kernel_short(zones):
    """Whether a look's zones up to Normal hold fewer free pages than half their min
    watermarks: the guest kernel's direct reclaimers then wait for kswapd
    (mm/vmscan.c `allow_direct_reclaim`, which counts the zones up to ZONE_NORMAL with
    managed pages)."""
    low = [z for n, z in zones.items() if n in ("DMA", "DMA32", "Normal") and z.get("managed")]
    if not low:
        return None
    return sum(z.get("free", 0) for z in low) <= sum(z.get("min", 0) for z in low) // 2


def guest(path):
    """The run's cgroup's reclaim over the read (pages scanned and taken, `high` events),
    where dd waited in each look, and the guest kernel's own: the looks whose zones up to
    Normal were short of half their min watermarks, and those that found dd throttled in
    the kernel's direct reclaim (`throttle_direct_reclaim`), each with its zones and
    MemFree, from the guest's samples."""
    first = last = None
    waits = collections.Counter()
    kernel = {"looks": 0, "short": 0, "throttled": [], "kmsg": [], "unplugged": 0}
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return None
    managed = None
    for line in lines:
        if line.startswith("kmsg "):
            kernel["kmsg"].append(line[5:])
            continue
        parts = line.split("|")
        if len(parts) < 2:
            continue
        head, cg = parts[0].split(), parts[1].split()
        if "dd" in head:
            for w in head[head.index("dd") + 1 : head.index("dd") + 6]:
                if w not in ("-", "0"):
                    waits[w] += 1
        zones = next((zones_of(p.split()[1:]) for p in parts[2:] if p.split()[:1] == ["zones"]), None)
        if zones:
            kernel["looks"] += 1
            kernel["short"] += bool(kernel_short(zones))
            # Memory plugged that went between looks, as its run read (its Normal zone's
            # managed pages, where the guest onlines what it plugs, D148).
            now = zones.get("Normal", {}).get("managed")
            if managed is not None and now is not None and now + 1024 < managed:
                kernel["unplugged"] += 1
            managed = now if now is not None else managed
            if "throttle_direct_reclaim" in head:
                free = next((p.split() for p in parts[2:] if "MemFree:" in p), [])
                mem = free[free.index("MemFree:") + 1] if "MemFree:" in free[:-1] else "?"
                held = cg[cg.index("held") + 1] if "held" in cg[:-1] else "?"
                kernel["throttled"].append({"zones": zones, "memfree_kb": mem, "uptime": head[1] if len(head) > 1 else "?",
                                            "current": cg[1] if len(cg) > 1 else "?", "held": held})
        if cg and cg[0] == "current":
            # Its value first, where the sampler read one (40136820527's lost it), then names
            # and values.
            rest = cg[2:] if len(cg) > 1 and cg[1].isdigit() else cg[1:]
            pairs = dict(zip(rest[::2], rest[1::2]))
            # And the guest kernel's own direct reclaim and its throttle (mm/vmscan.c), where
            # the sampler says them.
            for part in parts[2:]:
                words = part.split()
                if words[:1] == ["vm"]:
                    pairs.update(zip(words[1::2], words[2::2]))
            keys = ("pgscan", "pgsteal", "high", "workingset_refault_file", "pgscan_direct_throttle",
                    "pgscan_direct", "allocstall_dma32", "allocstall_normal", "allocstall_movable")
            now = {k: int(pairs[k]) for k in keys if pairs.get(k, "").isdigit()}
            if all(k in now for k in ("pgscan", "pgsteal")):
                first = first or now
                last = now
    if not first:
        return None
    return {k: last[k] - first[k] for k in first if k in last}, waits, kernel


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
            looks.append({"below": int(words[5]) - int(words[3]), "high": int(words[5])})
        elif words[0] == "threads":
            for t in words[2:]:
                name, _, rest = t.partition(":")
                state, _, wchan = rest.partition(":")
                # memory.high's throttle sleeps killable, in D (mm/memcontrol.c).
                if not name.startswith("vcpu") and state == "D":
                    waits[wchan] += 1
        elif words[0] == "host" and looks:
            # The whole host's reclaim and stalls (measure.py `host_reclaim`).
            kv = dict(zip(words[1::2], words[2::2]))
            looks[-1]["host"] = {k: int(v) for k, v in kv.items() if v.isdigit()}
        elif words[0] == "copy" and len(words) >= 2 and words[1].isdigit() and looks:
            # The share's copy of what its guest reads, in the host's page cache.
            looks[-1]["copy"] = int(words[1])
        elif words[0] == "files" and len(words) >= 4 and words[2] == "RssFile" and looks:
            # Each VM process's resident files, summed, and of its template's memory file
            # what the page cache holds: one file its warm VMs share, so the most.
            if words[3].isdigit():
                looks[-1]["rss_file"] = looks[-1].get("rss_file", 0) + (int(words[3]) << 10)
            if len(words) >= 6 and words[4] == "memory" and words[5].isdigit():
                looks[-1]["template"] = max(looks[-1].get("template", 0), int(words[5]))
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

def taken(r):
    """Of the read's template, the MiB its host's page cache held at its first look past
    the least it held at a later one: its guest's RAM its host took back as it read (PM
    M318); None where its looks did not say."""
    held = [l["template"] for l in (r["host"][2] if r["host"] else []) if "template" in l]
    return (held[0] - min(held)) / (1 << 20) if held else None


def host_took(r):
    """The MiB the whole host's reclaim took over the read, kswapd's and direct, from its
    first look to its last; None where its looks did not say."""
    took = [l["host"] for l in (r["host"][2] if r["host"] else []) if "host" in l]
    if len(took) < 2:
        return None
    pages = sum(took[-1].get(k, 0) - took[0].get(k, 0) for k in ("pgsteal_kswapd", "pgsteal_direct"))
    return pages * 4096 / (1 << 20)


def most_copy(r):
    """The most of the share's copy of the guests' files its host's page cache held at a
    look during the read, MiB; None where its looks did not say."""
    held = [l["copy"] for l in (r["host"][2] if r["host"] else []) if "copy" in l]
    return max(held) / (1 << 20) if held else None


for r in rows:
    g = r["guest"][0] if r["guest"] else {}
    took = taken(r)
    print(f"{r['run']} {r['copy']} {r['cpu'][:22]:22} {r['arm']:10} {r['high']:8} {r['file']:7} {r['secs']:7.2f} s "
          f"high {g.get('high')} pgsteal {g.get('pgsteal')} refaults {r['host'][0] if r['host'] else None} "
          + (f"template taken {took:.1f} MiB " if took is not None else "")
          + (f"copy at most {kept:.1f} MiB " if (kept := most_copy(r)) is not None else "")
          + (f"guest refaults {g['workingset_refault_file']} " if "workingset_refault_file" in g else "")
          + (f"held {r['guest'][1].get('__mem_cgroup_handle_over_high', 0)} " if r["guest"] else "")
          + (f"host reclaim {hosted:.1f} MiB " if (hosted := host_took(r)) is not None else "")
          + (f"direct throttle {g['pgscan_direct_throttle']} " if "pgscan_direct_throttle" in g else "")
          + (f"throttled {len(r['guest'][2]['throttled'])} zones short {r['guest'][2]['short']} of "
             f"{r['guest'][2]['looks']} " if r["guest"] and r["guest"][2]["looks"] else "")
          + f"{(r['verdict'] or '').split(';')[0]}")
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
                # The guest kernel's direct reclaim, throttled while its zones up to Normal
                # are short of half their min watermarks (step 2 of the flakes plan).
                ks = [r["guest"][2] for r in sel if r["guest"] and r["guest"][2]["looks"]]
                if ks and high != "infinity":
                    throttled = [t for k in ks for t in k["throttled"]]
                    print(f"    guest kernel: looks with its zones short {sum(k['short'] for k in ks)} of "
                          f"{sum(k['looks'] for k in ks)}; reads with dd throttled in direct reclaim "
                          f"{sum(1 for k in ks if k['throttled'])} of {len(ks)}; pgscan_direct_throttle a read "
                          f"{summary([r['guest'][0].get('pgscan_direct_throttle', 0) for r in sel if r['guest']], 0)}")
                    print(f"    guest kernel: reads whose plugged memory went as they read "
                          f"{sum(1 for k in ks if k['unplugged'])} of {len(ks)}")
                    for t in throttled[:6]:
                        z = " ".join(f"{n} {v.get('free')}/{v.get('min')}/{v.get('managed')}"
                                     for n, v in t["zones"].items() if v.get("managed"))
                        print(f"      throttled at {t['uptime']} s, run {t['current']} held at {t['held']}, "
                              f"MemFree {t['memfree_kb']} kB: free/min/managed {z}")
                    for k in [k for k in ks if k["throttled"] or k["unplugged"]][:4]:
                        for said in k["kmsg"][-6:]:
                            print(f"      kmsg {said}")
                if high != "short":
                    continue
                held = [r for r in sel if r["verdict"] and r["verdict"].startswith("held short throughout")]
                print(f"    held short throughout: {len(held)} of {len(sel)}, s {summary([r['secs'] for r in held], 2)}")
                print(f"      their guest pgsteal a MiB {summary([r['guest'][0]['pgsteal'] / MIB_READ for r in held if r['guest']], 1)}")
                print(f"      their slice refaults {summary([r['host'][0] for r in held if r['host']], 0)}")
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
                ran = [r for r in sel if r["host"] and sum(
                    l.get("file_mapped", MIB_RAN) < MIB_RAN and l.get("anon", 0) > l["high"] - (16 << 20)
                    for l in r["host"][2]) >= 2]
                print(f"    ran away: {len(ran)} of {len(sel)}")
                looks = [l for r in sel if r["host"] for l in r["host"][2]]
                print(f"    below its budget KiB a look {summary([l['below'] / 1024 for l in looks], 0)}")
                for k in ("file_mapped", "anon", "rss_file", "template", "copy"):
                    print(f"    its {k} MiB a look {summary([l[k] / (1 << 20) for l in looks if k in l], 1)}")
                print(f"    its template taken a read, MiB {summary([taken(r) for r in sel], 1)}")
