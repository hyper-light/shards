#!/usr/bin/env python3
"""measure.py's outputs as the PM entry's tables, one a host, as rule.py prints them beside
its verdicts: for each kind and arm, its rounds and the reads or writes timed, and of the
per-round values the median and range of p50 and p99, the median of p90 and the largest
max; seq's seconds; write's bytes written back per write; what the slice, the page cache,
the VM's mappings and KVM held.

    tables.py OUT.txt...
"""
import os
import runpy
import sys

if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    # rule.py's own reading and tables, by its path (python3 -I imports nothing beside
    # this script).
    rule = runpy.run_path(os.path.join(os.path.dirname(os.path.realpath(__file__)), "rule.py"))
    for path in sys.argv[1:]:
        host = rule["read"](path)
        print(f"== {path}")
        for line in host["head"]:
            print(line)
        rule["tables"](host)
