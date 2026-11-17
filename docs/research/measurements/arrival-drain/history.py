#!/usr/bin/env python3
"""The firecracker job's Arrival step in main's CI, from GitHub's records (D209 Part 11).

    history.py PAGES_DIR OUT.json   # pages: history.graphql's answers, fc-page*.json

Writes OUT.json, each firecracker check run of the pages (its commit, run, conclusion, times
and steps), and prints each run's job time and Arrival step, the Arrival steps that passed,
and the runs cut at the job's limit with the step each was in."""
import glob, json, os, sys
from datetime import datetime


def at(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ") if s else None


def secs(a, b):
    return (at(b) - at(a)).total_seconds() if a and b else None


runs = {}
for page in sorted(glob.glob(os.path.join(sys.argv[1], "fc-page*.json"))):
    for commit in json.load(open(page))["data"]["repository"]["object"]["history"]["nodes"]:
        for suite in commit["checkSuites"]["nodes"]:
            for job in suite["checkRuns"]["nodes"]:
                runs[job["databaseId"]] = {
                    "commit": commit["oid"],
                    "committed": commit["committedDate"],
                    "run": (suite.get("workflowRun") or {}).get("databaseId"),
                    "job": job["databaseId"],
                    "conclusion": job["conclusion"],
                    "started": job["startedAt"],
                    "completed": job["completedAt"],
                    "steps": job["steps"]["nodes"],
                }
runs = sorted(runs.values(), key=lambda r: r["committed"])
with open(sys.argv[2], "w") as out:
    out.write("[\n" + ",\n".join(json.dumps(r, separators=(",", ":")) for r in runs) + "\n]\n")

passed, cut = [], []
print("commit     run          job  conclusion  job_s  arrival  arrival_s  in_progress")
for r in runs:
    steps = {s["name"]: s for s in r["steps"]}
    arrival = steps.get("Arrival") or {}
    took = secs(arrival.get("startedAt"), arrival.get("completedAt"))
    running = [s["name"] for s in r["steps"] if s["startedAt"] and not s["completedAt"]]
    job_s = secs(r["started"], r["completed"])
    print(r["commit"][:9], r["run"], r["job"], r["conclusion"], job_s, arrival.get("conclusion"), took, running)
    if arrival.get("conclusion") == "SUCCESS":
        passed.append(took)
    # A job's limit is 30 minutes (1800 s): a job cancelled past it was cut there.
    if r["conclusion"] == "CANCELLED" and job_s is not None and job_s >= 1800:
        cut.append((r["commit"][:9], r["run"], job_s, running))
passed.sort()
n = len(passed)
rank = lambda p: passed[max(1, -(-p * n // 100)) - 1]
print(f"\nArrival passed: n {n}, p50 {rank(50)} s, p90 {rank(90)} s, p99 {rank(99)} s, max {passed[-1]} s: {passed}")
print(f"Cut at the job's limit: {len(cut)}")
for c in cut:
    print("  ", *c)
