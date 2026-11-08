#!/usr/bin/env python3
"""Where a guest's boot spends its time, from the kernel's own log, across many boots.

    boot-log.py --bin DIR --guest FILE --kernel KERNEL [--runs N] [--cmdline ARGS]

Boots the test guest (crates/testguest) in its `kmsg` mode, which prints the kernel's log
records once the boot is over, so the log costs the boot no console output (`quiet`).
Each record carries the kernel's clock in microseconds. It reports when each boot ran
init ("Run /init as init process"), splits the boots into fast and slow at the midpoint
between the fastest and slowest, and names the record at which the slow boots' median
falls behind the fast boots' by more than 1.5 ms: the step that holds the difference.
--cmdline adds kernel arguments, such as cryptomgr.notests=1, to compare.
"""

import argparse
import os
import statistics
import subprocess
from collections import defaultdict


def boot(bin_dir, kernel, guest, extra):
    r = subprocess.run(
        [
            os.path.join(bin_dir, "shards"), "vm", "run", "--kernel", kernel, "--init",
            guest, "--cpus", "1", "--memory", "256", "--cmdline",
            f"console=ttyS0 quiet panic=-1 {extra} shards_test=kmsg".strip(),
        ],
        capture_output=True, text=True, errors="replace", timeout=120,
    )
    records = []
    for line in r.stdout.splitlines():
        head, _, text = line.partition(";")
        fields = head.split(",")
        if len(fields) >= 3 and fields[2].isdigit():
            records.append((int(fields[2]), text))
    return records


def median_times(boots):
    seen = defaultdict(list)
    for records in boots:
        first = {}
        for us, text in records:
            first.setdefault(text[:60], us)
        for text, us in first.items():
            seen[text].append(us)
    return {t: statistics.median(v) for t, v in seen.items() if len(v) >= 0.9 * len(boots)}


def main():
    ap = argparse.ArgumentParser()
    for flag in ("--bin", "--guest", "--kernel"):
        ap.add_argument(flag, required=True)
    ap.add_argument("--runs", type=int, default=60)
    ap.add_argument("--cmdline", default="")
    args = ap.parse_args()
    boots = [boot(args.bin, args.kernel, args.guest, args.cmdline) for _ in range(args.runs)]
    init = []
    for records in boots:
        at = [us for us, text in records if text.startswith("Run /init")]
        if not at:
            raise SystemExit("a boot printed no 'Run /init' record")
        init.append(at[0])
    split = (min(init) + max(init)) / 2
    slow = [b for b, t in zip(boots, init) if t > split]
    fast = [b for b, t in zip(boots, init) if t <= split]
    v = sorted(init)
    print(f"n={len(v)} init at p50 {v[len(v) // 2]} us, min {v[0]}, max {v[-1]}")
    print(f"fast {len(fast)}, slow {len(slow)} (split at {split:.0f} us)")
    if not fast or not slow or max(init) - min(init) < 3000:
        return
    mf, ms = median_times(fast), median_times(slow)
    previous = 0.0
    for text in sorted(mf, key=mf.get):
        if text not in ms:
            continue
        behind = ms[text] - mf[text]
        if abs(behind - previous) > 1500:
            print(f"slow boots {behind:.0f} us behind from: {text!r} (fast {mf[text]:.0f} us)")
        previous = behind


if __name__ == "__main__":
    main()
