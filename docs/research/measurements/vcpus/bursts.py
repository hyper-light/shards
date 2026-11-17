#!/usr/bin/env python3
"""Checks crates/vmm/src/vm/limits/bursts.txt against the CI job logs it was measured from.

Each `gap GAP_US SPAN_US RUN JOB FIRST LAST DUMPS` line is one burst of refused VM entries:
the VMCS dumps kvm_intel made between its FIRST and LAST dmesg seconds ("kvm_intel: VMCS ...,
last attempted VM-entry on CPU N", printed by the job's failure step). For each, this reads the
job's log, takes the distinct dumps in that window, and prints the burst as measured: its
largest gap between consecutive dumps, its span from the first to the last, and the dumps in
it, then whether the file says the same (D261's gate, ROOT's ruling of 2026-11-17: a burst's
width is its largest consecutive gap, never its span).

Usage: python3 -I bursts.py BURSTS_TXT RUN:JOB=LOG...
  where LOG is the job's log as GitHub serves it (gh api .../actions/jobs/ID/logs).
"""

import re
import sys

DUMP = re.compile(r"\[\s*(\d+)\.(\d{6})\] kvm_intel: VMCS ([0-9a-f]+), last attempted VM-entry on CPU (\d+)")


def micros(seconds):
    whole, frac = seconds.split(".")
    return int(whole) * 1_000_000 + int(frac)


def dumps_in(path):
    """The distinct VMCS dumps a job's log holds, in order of their dmesg moments (us)."""
    seen = set()
    with open(path, errors="replace") as log:
        for line in log:
            m = DUMP.search(line)
            if m:
                seen.add((int(m.group(1)) * 1_000_000 + int(m.group(2)), m.group(3), m.group(4)))
    return sorted(seen)


def main(argv):
    if len(argv) < 2:
        sys.exit(__doc__)
    logs = dict(arg.split("=", 1) for arg in argv[1:])
    dumps = {job: dumps_in(path) for job, path in logs.items()}
    same = True
    with open(argv[0]) as bursts:
        for line in bursts:
            words = line.split()
            if not words or words[0] != "gap":
                continue
            gap, span, run, job, first, last, count = words[1:8]
            key = f"{run}:{job}"
            if key not in dumps:
                print(f"{key}: no log given")
                same = False
                continue
            moments = [at for at, _, _ in dumps[key] if micros(first) <= at <= micros(last)]
            gaps = [after - before for before, after in zip(moments, moments[1:])]
            measured = (max(gaps, default=0), moments[-1] - moments[0] if moments else 0, len(moments))
            said = (int(gap), int(span), int(count))
            print(f"gap {measured[0]} {measured[1]} {run} {job} {first} {last} {measured[2]}"
                  f"{'' if measured == said else '  # the file says ' + ' '.join(map(str, said))}")
            same &= measured == said
    sys.exit(0 if same else 1)


if __name__ == "__main__":
    main(sys.argv[1:])
