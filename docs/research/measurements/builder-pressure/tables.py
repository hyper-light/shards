#!/usr/bin/env python3
"""measure.py's output as the PM entry's tables, each run alone or with --pool every run given
as one (runs of one host): for each build and arm (free, held), over the rounds after the
caches' (round 0's free arms, where a run has one; round 1's before it had): its time, its scope's anonymous peak and budget
(Linux), and its builder's give-backs, each answer's time from its word, what it freed of
its files, what it asked to unplug and what its guest then held plugged less (its own samples,
a second on), and each build's sums; the rooms its budget left told it and what its guest
held the build within, and what it refused; each as n, p50, p90, p99 and max.
Then the rule fixed before any number: a held build is slower than its free arm beyond the
rounds' spread where its fastest round is slower than the free arm's slowest.

    tables.py [--pool] OUT.txt...
"""
import sys
from collections import defaultdict

MIB = 1 << 20


def fields(line):
    return dict(w.split("=", 1) for w in line.split()[1:] if "=" in w)


def pct(values, q):
    v = sorted(values)
    return v[min(len(v) - 1, int(q / 100 * len(v)))]


def stats(values, unit="", scale=1):
    if not values:
        return "-"
    f = lambda v: f"{v / scale:.1f}" if scale != 1 or isinstance(v, float) else f"{v}"
    return (f"n {len(values)} p50 {f(pct(values, 50))} p90 {f(pct(values, 90))} p99 {f(pct(values, 99))} "
            f"max {f(max(values))}{unit}")


def read(path, run, into):
    """`path`'s lines into `into`, its rounds keyed by `run`; those that filled the builds'
    caches (round 0, or round 1 where it has no round 0) as round 0."""
    lines = open(path, errors="replace").read().splitlines()
    warm = any(l.startswith("build ") and " round=0 " in l for l in lines)
    builds, gives, head, plugged, own, io = into
    for line in lines:
        if line.startswith(("host ", "builds ")) and "=" not in line.split()[1]:
            head.append(line.rstrip())
        elif line.startswith("build "):
            f = fields(line)
            builds[(f["build"], f["arm"], (run, counted(f, warm)))] = f
        elif line.startswith("give "):
            f = fields(line)
            gives[(f["build"], f["arm"], (run, counted(f, warm)))].append(f)
        elif line.startswith(("phase ", "keep ", "merge ", "room ")):
            f = fields(line)
            own[(f["build"], f["arm"], (run, counted(f, warm)))].append((line.split()[0], float(f["t"]), f))
        elif line.startswith("host ") and "build_write_bytes" in line:
            f = fields(line)
            io[(f["build"], f["arm"], (run, counted(f, warm)))].append(f)
        elif line.startswith("guest "):
            f = fields(line)
            io[(f["build"], f["arm"], (run, counted(f, warm)))].append(f)
            if "plugged" in f:
                plugged[(f["build"], f["arm"], (run, counted(f, warm)))].append((float(f["t"]), int(f["plugged"])))


def steps_of(f):
    """A build line's steps' times, by step number: none where it says none."""
    return {k: float(v) for k, v in (p.split(":") for p in f.get("steps", "-").split(",") if ":" in p)}


def counted(f, warm):
    """The round of a line's fields, 0 for one that filled the builds' caches."""
    r = int(f["round"])
    return r if warm or r > 1 else 0


def unplugged(samples, t):
    """What the guest held plugged less, from its last sample before `t` (an answer's) to its
    first a second or more after: what an ask to unplug did."""
    before = [p for at, p in samples if at <= t]
    after = [p for at, p in samples if at >= t + 1]
    if not before or not after:
        return None
    return max(before[-1] - after[0], 0)


# The phases of a step's work after its builder's (give.patch's `phase` lines, each its end,
# timed from the line before it): `run` its builder's work through its root's changes in
# (from its caches attached), `cache-keep` the rest of its changes to their staging file and
# its caches' kept and merged, `layer` its layer written, `blob` the layer made durable.
PHASES = ("run", "cache-keep", "layer", "blob")
# What a build's pages cost it, each a count so far in its samples (cold.patch's guest lines,
# measure.py's host lines): its guest's major faults and file refaults and the times its
# guest's kernel throttled it past its hold (the build cgroup's memory.events `high`), its
# VM process's major faults on this host, and the times this host's kernel throttled its
# scope past its budget.
COSTS = (("pgmajfault", "guest major faults", 1, ""), ("refault_file", "guest refaults", 1, ""),
         ("high_events", "guest throttled", 1, ""), ("vm_majflt", "VM major faults", 1, ""),
         ("scope_high_events", "host throttled", 1, ""))
# What each of the build's processes dirtied and read (host lines' /proc/PID/io fields).
IO = (("vm_write_bytes", "VM process dirtied"), ("build_write_bytes", "build dirtied"),
      ("build_read_bytes", "build read"), ("writer_write_bytes", "writer dirtied"))


def own_of(own, io, key):
    """A build's sums of its own lines: each phase's seconds, its caches' changes made
    durable and their merges (bytes, ms), and what each process dirtied and read (bytes)."""
    sums = defaultdict(float)
    before = None
    for kind, t, f in sorted(own.get(key, []), key=lambda e: e[1]):
        if kind == "phase":
            if before is not None and f.get("name") in PHASES:
                sums[f["name"]] += t - before
            before = t
        elif kind == "keep":
            sums["keep_bytes"] += int(f.get("bytes", 0))
            sums["keep_sync_ms"] += int(f.get("sync_ms", 0))
        elif kind == "merge":
            sums["merges"] += 1
            sums["merge_bytes"] += int(f.get("bytes", 0))
            sums["merge_write_ms"] += int(f.get("write_ms", 0))
            sums["merge_sync_ms"] += int(f.get("sync_ms", 0))
    samples = io.get(key, [])
    for k in [k for k, _ in IO] + [k for k, _, _, _ in COSTS]:
        got = [int(f[k]) for f in samples if k in f]
        if got:
            sums[k] = max(got) - min(got)
    # The least room the build told its builders (give.patch's `room` lines, its own count),
    # and the least its budget left it as this host's kernel counts it, the budget less
    # what the scope was charged (memory.current), in its samples.
    told = [int(f["told"]) for kind, _, f in own.get(key, []) if kind == "room" and f.get("told", "none") != "none"]
    if told:
        sums["told_least"] = min(told)
    return sums


def report(title, into):
    builds, gives, head, plugged, own, io = into
    print(f"== {title}")
    for h in head:
        print(h)
    for name in sorted({b for b, _, _ in builds}):
        times, arm_rounds = {}, {}
        for arm in ("free", "held"):
            rounds = sorted(r for b, m, r in builds if b == name and m == arm and r[1] > 0
                            and builds[(b, m, r)].get("exit") == "0")
            failed = sorted(r for b, m, r in builds if b == name and m == arm and builds[(b, m, r)].get("exit") != "0")
            s = [float(builds[(name, arm, r)]["s"]) for r in rounds]
            times[arm] = s
            arm_rounds[arm] = rounds
            print(f"-- {name} {arm}: rounds past the caches' {len(rounds)}, failed {len(failed)}")
            print(f"   build time        {stats(s, ' s')}")
            peaks = [int(builds[(name, arm, r)].get("scope_anon_peak", 0)) for r in rounds]
            if any(peaks):
                print(f"   scope anon peak   {stats(peaks, ' MiB', MIB)}")
            helds = [int(builds[(name, arm, r)].get("scope_held_peak", 0)) for r in rounds]
            if any(helds):
                print(f"   scope held peak   {stats(helds, ' MiB', MIB)}")
            budgets = [int(builds[(name, arm, r)].get("budget", 0)) for r in rounds]
            if any(budgets):
                print(f"   budget            {stats(budgets, ' MiB', MIB)}")
                # How far short of what the build held unbudgeted its budget was: the held
                # peak of the round's free arm less it.
                shorts = [int(builds[k]["scope_held_peak"]) - int(builds[(name, arm, r)]["budget"])
                          for r in rounds for k in [(name, "free", r)]
                          if int(builds.get(k, {}).get("scope_held_peak", 0))]
                if shorts:
                    print(f"   short of its peak {stats(shorts, ' MiB', MIB)}")
                # Whether its writer outside the budget ran (D188): where it did not, the
                # build's own writes were charged to the budget (PM M442).
                seen = [r for r in rounds if int(builds[(name, arm, r)].get("writer_seen", 0)) > 0]
                if any("writer_seen" in builds[(name, arm, r)] for r in rounds):
                    print(f"   writer outside    in {len(seen)} of {len(rounds)} builds")
                    files = [int(builds[(name, arm, r)]["writer_file_peak"]) for r in seen]
                    dirty = [int(builds[(name, arm, r)]["writer_dirty_peak"]) for r in seen]
                    if seen:
                        print(f"   writer file peak  {stats(files, ' MiB', MIB)}")
                        print(f"   writer dirty peak {stats(dirty, ' MiB', MIB)}")
            answers = [g for r in rounds for g in gives.get((name, arm, r), []) if "answered_ms" in g]
            unanswered = [g for r in rounds for g in gives.get((name, arm, r), []) if "unanswered_ms" in g]
            if answers or unanswered:
                print(f"   answers           {len(answers)}, unanswered {len(unanswered)}")
                print(f"   answered in       {stats([int(g['answered_ms']) for g in answers], ' ms')}")
                print(f"   freed an answer   {stats([int(g['freed']) for g in answers], ' MiB', MIB)}")
                print(f"   unplug asked      {stats([int(g['unplugging']) for g in answers], ' MiB', MIB)}")
                done = [d for r in rounds for g in gives.get((name, arm, r), []) if "answered_ms" in g
                        for d in [unplugged(plugged.get((name, arm, r), []), float(g["t"]))] if d is not None]
                print(f"   unplug done       {stats(done, ' MiB', MIB)}")
                sums = [sum(int(g["freed"]) for g in gives.get((name, arm, r), []) if "answered_ms" in g) for r in rounds]
                print(f"   freed a build     {stats(sums, ' MiB', MIB)}")
            helds = [g for r in rounds for g in gives.get((name, arm, r), []) if "held_ms" in g]
            if helds:
                within = [int(g["within"]) for g in helds if g["within"] != "none"]
                print(f"   rooms told        {len(helds)}, held within {stats(within, ' MiB', MIB)}")
            refusals = [g for r in rounds for g in gives.get((name, arm, r), []) if "refused_ms" in g]
            if refusals:
                print(f"   refusals          {len(refusals)}, the last: {refusals[-1]['why'].replace('_', ' ')}")
        # The build's own phases, its caches' writes and its processes' I/O, free against
        # held: which of a step's end's parts a budget slows, and whose writes they are.
        mine = {arm: [own_of(own, io, (name, arm, r)) for r in arm_rounds.get(arm, [])]
                for arm in ("free", "held")}
        for r, m in zip(arm_rounds.get("held", []), mine["held"]):
            budget = int(builds[(name, "held", r)].get("budget", 0))
            real = [budget - int(f["scope_current"]) for f in io.get((name, "held", r), []) if "scope_current" in f]
            if budget and real:
                m["real_least"] = min(real)
        rows = [(p, f"phase {p}", 1, " s") for p in PHASES] + [
            ("keep_bytes", "caches kept", MIB, " MiB"), ("keep_sync_ms", "kept, synced in", 1, " ms"),
            ("merges", "merges", 1, ""), ("merge_bytes", "merged", MIB, " MiB"),
            ("merge_write_ms", "merged, written in", 1, " ms"), ("merge_sync_ms", "merged, synced in", 1, " ms"),
        ] + [(k, said, MIB, " MiB") for k, said in IO] + list(COSTS) + [
            ("told_least", "room told, least", MIB, " MiB"), ("real_least", "room real, least", MIB, " MiB")]
        for k, said, scale, unit in rows:
            got = {arm: [m[k] for m in mine[arm] if k in m] for arm in ("free", "held")}
            if got["free"] or got["held"]:
                print(f"   {said:<19} free {stats(got['free'], unit, scale)}; held {stats(got['held'], unit, scale)}")
        # Each step's time as the build's display says it (`#N DONE`), free against held.
        for n in sorted({k for arm in ("free", "held") for r in arm_rounds.get(arm, [])
                         for k in steps_of(builds[(name, arm, r)])}, key=int):
            took = {arm: [steps_of(builds[(name, arm, r)])[n] for r in arm_rounds.get(arm, [])
                          if n in steps_of(builds[(name, arm, r)])] for arm in ("free", "held")}
            if took["free"] and took["held"]:
                print(f"   step #{n:<3} free {stats(took['free'], ' s')}; held {stats(took['held'], ' s')}")
        free, held = times.get("free", []), times.get("held", [])
        if free and held:
            worse = min(held) > max(free)
            print(f"   rule: held slower than free beyond the rounds' spread: {'yes' if worse else 'no'} "
                  f"(held's fastest {min(held):.1f} s, free's slowest {max(free):.1f} s)")


def main(args):
    pool = "--pool" in args
    paths = [a for a in args if a != "--pool"]
    if pool:
        into = ({}, defaultdict(list), [], defaultdict(list), defaultdict(list), defaultdict(list))
        for run, path in enumerate(paths):
            read(path, run, into)
        report(" + ".join(paths), into)
        return
    for path in paths:
        into = ({}, defaultdict(list), [], defaultdict(list), defaultdict(list), defaultdict(list))
        read(path, 0, into)
        report(path, into)


if __name__ == "__main__":
    if len([a for a in sys.argv[1:] if a != "--pool"]) < 1:
        raise SystemExit(__doc__)
    main(sys.argv[1:])
