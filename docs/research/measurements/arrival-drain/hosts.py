#!/usr/bin/env python3
"""The firecracker job's runs in main's CI by host model, and the step and job limits a third
above their slowest (D209 Part 11 amended, CI 44164945050).

    hosts.py history.json   # history.py's records

GitHub's records name no CPU; a job's log does, in its Host step (lscpu), and six logs read
(LOGGED) name an Intel model and three AMD ones. The Image bench's step tells the two makers
apart in every one of them, its time a property of the host: the runs are split where it falls
between the slowest logged Intel run's and the quickest logged AMD run's. Prints each run, then
Arrival's ends by host, Guest memory's (M117) times, and the job's time outside Arrival and
M117, with the limits M560's rule gives them: a third above the slowest, in whole minutes."""
import json, math, sys
from datetime import datetime

# Job ids whose logs were read, and the CPU model each log names (lscpu's "Model name").
LOGGED = {
    133778778156: "Intel Xeon 6975P-C",  # b18952ae6, CI 44005213738
    134014115360: "Intel Xeon 6975P-C",  # 8fc54dcfb, CI 44076016255
    133841315154: "AMD EPYC 7763",  # 8d6029e4d, CI 44020677971
    134090721199: "AMD EPYC 9V45",  # a223a63bc, CI 44077430171
    134191718017: "AMD EPYC 7763",  # 86f5c7b86, CI 44125576545
    134234527940: "AMD EPYC 9V74",  # 8247e1448, CI 44138098584
}
# The judged drain (deb971420, D209 Part 11) and the six snapshot pairs (4bef86c91, D36): runs
# of commits committed from these times on have them, main's history being linear in them.
JUDGED = "2026-11-17T06:09:11Z"
PAIRS = "2026-11-17T19:04:29Z"
# The job's limit when these runs ran, and Arrival's step limit (ci.yml).
JOB_LIMIT_S, ARRIVAL_LIMIT_S = 30 * 60, 5 * 60


def at(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ") if s else None


def secs(a, b):
    return int((at(b) - at(a)).total_seconds()) if a and b else None


def rank(xs, p):
    xs = sorted(xs)
    return xs[max(1, -(-p * len(xs) // 100)) - 1]


def dist(xs):
    return f"n {len(xs)}, p50 {rank(xs, 50)}, p90 {rank(xs, 90)}, p99 {rank(xs, 99)}, max {max(xs)}" if xs else "n 0"


runs = []
for r in json.load(open(sys.argv[1])):
    steps = {s["name"]: s for s in r["steps"]}
    took = lambda name: secs((steps.get(name) or {}).get("startedAt"), (steps.get(name) or {}).get("completedAt"))
    ended = lambda name: (steps.get(name) or {}).get("conclusion")
    arrival = steps.get("Arrival") or {}
    runs.append({
        **r,
        "span": secs(r["started"], r["completed"]),
        "image": took("Image") if ended("Image") == "SUCCESS" else None,
        "arrival": took("Arrival"),
        "arrival_end": ended("Arrival") or ("open" if arrival.get("startedAt") else None),
        "before_arrival": secs(r["started"], arrival.get("startedAt")),
        "m117": took("Guest memory (M117)") if ended("Guest memory (M117)") == "SUCCESS" else None,
    })

intel = [r["image"] for r in runs if LOGGED.get(r["job"], "").startswith("Intel")]
amd = [r["image"] for r in runs if LOGGED.get(r["job"], "").startswith("AMD")]
split = (max(intel) + min(amd)) / 2
host = lambda r: None if r["image"] is None else ("Intel" if r["image"] < split else "AMD")
print(f"Image step, logged Intel {sorted(intel)} s, logged AMD {sorted(amd)} s: split at {split} s")
print(f"unlogged runs nearest the split: Intel-side max {max(r['image'] for r in runs if host(r) == 'Intel')} s, "
      f"AMD-side min {min(r['image'] for r in runs if host(r) == 'AMD')} s\n")

print("commit     job           span  image host   logged              before  arrival end      m117")
for r in runs:
    if r["arrival_end"] is None:
        continue
    print(f"{r['commit'][:10]} {r['job']} {r['span']:5} {r['image'] or '-':>5} {host(r) or '-':6} "
          f"{LOGGED.get(r['job'], '-'):19} {r['before_arrival'] or '-':>6} {r['arrival'] or '-':>7} "
          f"{r['arrival_end']:8} {r['m117'] or '-'}")


def ends(rs):
    passed = [r["arrival"] for r in rs if r["arrival_end"] == "SUCCESS"]
    job_cut = [r for r in rs if r["conclusion"] == "CANCELLED" and r["span"] >= JOB_LIMIT_S and r["arrival_end"] == "open"]
    step_cut = [r for r in rs if r["arrival_end"] == "FAILURE" and (r["arrival"] or 0) >= ARRIVAL_LIMIT_S]
    return f"passed {dist(passed)} s; cut at the job's limit in it {len(job_cut)}; cut at its own limit {len(step_cut)}"


print()
for name in ("Intel", "AMD"):
    for era, keep in (("before the judged drain", lambda r: r["committed"] < JUDGED), ("with it", lambda r: r["committed"] >= JUDGED)):
        rs = [r for r in runs if host(r) == name and keep(r)]
        print(f"Arrival on {name} hosts, {era}: {ends(rs)}")

m117 = [r["m117"] for r in runs if r["m117"] is not None]
m117_limit = math.ceil(max(m117) * 4 / 3 / 60)
print(f"\nGuest memory (M117), passed: {dist(m117)} s; a third above its slowest, "
      f"{max(m117) * 4 / 3:.1f} s: {m117_limit} min")
for name in ("Intel", "AMD"):
    print(f"  on {name} hosts: {dist([r['m117'] for r in runs if r['m117'] is not None and host(r) == name])} s")

# The job's other steps, in the runs whose Arrival and M117 both passed: its span less those two.
others = [r["span"] - r["arrival"] - r["m117"] for r in runs if r["arrival_end"] == "SUCCESS" and r["m117"] is not None]
others_limit = math.ceil(max(others) * 4 / 3 / 60)
print(f"\nThe job outside Arrival and M117: {dist(others)} s; a third above its slowest, "
      f"{max(others) * 4 / 3:.1f} s: {others_limit} min; the job: {ARRIVAL_LIMIT_S // 60} + {m117_limit} + "
      f"{others_limit} = {ARRIVAL_LIMIT_S // 60 + m117_limit + others_limit} min")
print("Before Arrival, the six pairs' runs: " + ", ".join(
    f"{r['commit'][:10]} {host(r)} {r['before_arrival']} s" for r in runs if r["committed"] >= PAIRS and r["before_arrival"]))
for name in ("Intel", "AMD"):
    rs = [r["before_arrival"] for r in runs if r["committed"] < PAIRS and r["arrival_end"] and r["before_arrival"] and host(r) == name
          and r["conclusion"] != "CANCELLED"]
    print(f"  without them, on {name} hosts (runs not cancelled): {dist(rs)} s")
