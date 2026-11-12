#!/usr/bin/env python3
"""Step 6's mutant table: each guard's mutant (mutants.tsv), the test it must fail, and how
that test ended in a round's output (logs/mutants-REV.out), with the assertion that failed.

    python3 -I docs/research/measurements/s38-nested-grants/step6/mutant-table.py REV

Prints one tab-separated row a mutant: name, file, test, exit, where it panicked. Exits 1
unless every mutant was applied and its test failed (exit 101) at an assertion.
"""

from pathlib import Path
import re
import sys

STEP = Path(__file__).resolve().parent


def main():
    rev = sys.argv[1]
    defs = [
        line.split("\t")
        for line in (STEP / "mutants.tsv").read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]
    out = (STEP / "logs" / f"mutants-{rev}.out").read_text(encoding="utf-8")
    bad = 0
    print("mutant\tfile\ttest\texit\tpanicked at")
    for name, path, _expr, args in defs:
        test = args.split()[-1]
        rc = re.search(rf"^MUTANT {re.escape(name)}: its test's exit (\d+)$", out, re.M)
        where = None
        if rc:
            # Its summary: from its MUTANT line to the next mutant's command.
            seg = out[rc.end():].split("\n== mutant-", 1)[0]
            m = re.search(r"panicked at (\S+):", seg)
            where = m.group(1) if m else None
        applied = f"MUTANT {name} NOT APPLIED" not in out
        code = rc.group(1) if rc else "none"
        if not (applied and code == "101" and where):
            bad += 1
        print(f"{name}\t{path}\t{test}\t{code if applied else 'NOT APPLIED'}\t{where or 'no assertion'}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
