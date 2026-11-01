#!/usr/bin/env python3
"""CI's native jobs' tests' steps against their limits (PM M245, M420).

    limits.py CACHE RUNS

RUNS lists CI runs, a run's id first on each line (runs.txt). For each, GitHub's record of its
jobs (`gh api`), kept in CACHE. Prints, for each native job that tests, its tests' step's
time in the runs where it ran through (n, p50, p90, p99, max, minutes), how many ran out of
its limit, and its other steps' slowest; a job's time is its steps', from the first's start to
the last's end (GitHub once recorded a job's end an hour past its last step's). For x86_64
Linux it also reads each log (kept in CACHE): the step split at cargo's `Finished` line into
building the tests and running them, and for each step that ran out, the binary it was in, how
long since the suite last moved (a binary started, a result printed), and what the rest of the
suite took from that binary's start in the runs that ran through.
"""

import datetime
import json
import math
import os
import re
import subprocess
import sys

REPO = "hyper-light/shards"
NATIVE = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin",
          "x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc (windows-2022)")
LINUX = "x86_64-unknown-linux-gnu"
STAMP = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z ")
RUNNING = re.compile(r"     Running (\S+) \(target/[^)]*/deps/([A-Za-z0-9_]+)-[0-9a-f]+\)")


def gh(path, out):
    if not os.path.exists(out):
        data = subprocess.run(["gh", "api", "--allow-escape-sequences", path],
                              capture_output=True, check=True).stdout
        with open(out, "wb") as f:
            f.write(data)
    with open(out, "rb") as f:
        return f.read()


def minutes(a, b):
    when = lambda s: datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))
    return (when(b) - when(a)).total_seconds() / 60


def at(line):
    m = STAMP.match(line)
    return datetime.datetime.fromisoformat(m.group(1)[:26]) if m else None


def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, max(0, math.ceil(p * len(v)) - 1))]


def summary(v):
    if not v:
        return "n 0"
    return (f"n {len(v)} p50 {pct(v, .5):.1f} p90 {pct(v, .9):.1f} p99 {pct(v, .99):.1f} "
            f"max {max(v):.1f}")


def log(cache, job):
    """The tests' step of x86_64 Linux's log: its start, cargo's Finished, its end, whether it
    ran out, and each binary's start."""
    text = gh(f"repos/{REPO}/actions/jobs/{job}/logs", os.path.join(cache, f"{job}.log"))
    text = re.sub(r"\x1b\[[0-9;]*m", "", text.decode("utf-8", "replace"))
    step = dict(start=None, built=None, end=None, out=False, binaries=[], moved=None)
    for line in text.splitlines():
        if step["start"] is None:
            if f'##[group]Run case "{LINUX}" in' in line:
                step["start"] = at(line)
            continue
        if step["built"] is None and "Finished `test-release` profile" in line:
            step["built"] = at(line)
        m = RUNNING.search(line)
        if m:
            step["binaries"].append((f"{m.group(1)} {m.group(2)}", at(line)))
        if m or "test result:" in line:
            step["moved"] = at(line)
        if "has timed out after" in line:
            step["end"], step["out"] = at(line), True
            break
        if "##[group]Run " in line:
            step["end"] = at(line)
            break
    return step


def main():
    cache, runs = sys.argv[1], sys.argv[2]
    os.makedirs(cache, exist_ok=True)
    ids = [line.split()[0] for line in open(runs) if line.strip()]
    tests, others, out, linux = {}, {}, {}, []
    for run in ids:
        record = json.loads(gh(f"repos/{REPO}/actions/runs/{run}/jobs?per_page=50",
                               os.path.join(cache, f"jobs-{run}.json")))
        for job in record["jobs"]:
            name = job["name"]
            if name not in NATIVE or job.get("conclusion") not in ("success", "failure"):
                continue
            steps = [s for s in job.get("steps") or [] if s.get("started_at") and s.get("completed_at")]
            ran = [s for s in steps if s["name"] == "Tests" and s.get("conclusion") in ("success", "failure")]
            if not ran:
                continue
            took = minutes(ran[0]["started_at"], ran[0]["completed_at"])
            span = minutes(min(s["started_at"] for s in steps), max(s["completed_at"] for s in steps))
            others.setdefault(name, []).append(span - took)
            if took > limit_of(name):
                out.setdefault(name, []).append((run, took))
            else:
                tests.setdefault(name, []).append(took)
            if name == LINUX:
                linux.append((run, log(cache, job["id"])))
    for name in NATIVE:
        print(f"{name}: tests' step, ran through: {summary(tests.get(name, []))}; ran out: "
              f"{len(out.get(name, []))} {[r for r, _ in out.get(name, [])]}; other steps: "
              f"{summary(others.get(name, []))}")
    through = [s for _, s in linux if not s["out"] and s["built"] and s["end"]]
    built = [(s["built"] - s["start"]).total_seconds() / 60 for s in through]
    tested = [(s["end"] - s["built"]).total_seconds() / 60 for s in through]
    print(f"{LINUX}, building the tests: {summary(built)}")
    print(f"{LINUX}, running them: {summary(tested)}")
    for run, s in linux:
        if not s["out"] or not s["binaries"]:
            continue
        last, started = s["binaries"][-1]
        nth = sum(1 for b, _ in s["binaries"] if b == last) - 1
        rest = sorted((t["end"] - hits[nth]).total_seconds() / 60 for t in through
                      for hits in [[w for b, w in t["binaries"] if b == last]] if len(hits) > nth)
        step = (s["end"] - s["start"]).total_seconds() / 60
        still = (s["end"] - s["moved"]).total_seconds() / 60
        if rest:
            print(f"ran out {run}: in {last} (#{nth}), unmoved {still:.1f} min; the rest took "
                  f"{pct(rest, .5):.1f} to {max(rest):.1f} in {len(rest)} runs: a step of "
                  f"{step + pct(rest, .5):.1f} to {step + max(rest):.1f}")


def limit_of(name):
    """The tests' step's limit the measured runs had (ci.yml before M420)."""
    return 120 if name.startswith("x86_64-pc-windows-msvc") else 41


if __name__ == "__main__":
    main()
