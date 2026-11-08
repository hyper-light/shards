#!/usr/bin/env python3
"""What shards' learned answer waits (crates/registry/src/patience.rs, D222) would have done
with the answers probe.py recorded (PM M486).

    replay.py PROBE.jsonl

Each endpoint's answers, in order, through RFC 6298's estimator as patience.rs keeps it: a
wait of SRTT + 4·RTTVAR, at least 1 s, 1 s before any answer, doubled for each timeout in
a row. An answer later than the wait would have been cut and asked again: a healthy
answer so cut is asked again with the wait doubled, as often as it outlasts the wait,
and answered as it was, each wait lost; a request that never answered (the probe's own timeout) loses the
wait, where a fixed one loses all the probe's timeout. Prints, by endpoint and in all: the
answers, the healthy answers cut (and the share), the time a probe of fixed 30 s waits
lost, and the time the learned waits would have.
"""
import json
import sys
from collections import defaultdict

FIRST = 1.0


class Pace:
    def __init__(self):
        self.srtt = None
        self.rttvar = None

    def add(self, r):
        if self.srtt is None:
            self.srtt, self.rttvar = r, r / 2
        else:
            self.rttvar = 0.75 * self.rttvar + 0.25 * abs(self.srtt - r)
            self.srtt = 0.875 * self.srtt + 0.125 * r

    def timeout(self):
        if self.srtt is None:
            return FIRST
        return max(FIRST, self.srtt + 4 * self.rttvar)


def main():
    by = defaultdict(list)
    for line in open(sys.argv[1]):
        r = json.loads(line)
        by[(r["host"], r["path"].split("?")[0])].append(r)
    totals = defaultdict(float)
    print("endpoint: answers, healthy cuts (share), stuck, fixed-wait loss s, learned loss s, wait p50/max s")
    for (host, path), rs in sorted(by.items()):
        rs.sort(key=lambda r: r["t"])
        pace = Pace()
        timeouts = 0
        cut = stuck = 0
        fixed = learned = 0.0
        waits = []
        for r in rs:
            wait = pace.timeout() * (2 ** timeouts)
            waits.append(wait)
            took = r["took"]
            never = r["status"] is None and r["error"] == "timeout"
            if never:
                stuck += 1
                fixed += took
                # Cut at the wait, where the fixed wait ran out: then asked again.
                learned += wait
                timeouts += 1
                continue
            if r["status"] is None:
                continue
            # Cut at each wait it outlasts, asked again with the wait doubled, and answered
            # as it was: each wait lost.
            while took > wait:
                cut += 1
                learned += wait
                wait *= 2
            pace.add(took)
            timeouts = 0
        n = sum(1 for r in rs if r["status"] is not None)
        waits.sort()
        print(f"{host}{path}: {n}, {cut} ({cut / max(n, 1):.4%}), {stuck}, {fixed:.1f}, {learned:.1f}, "
              f"{waits[len(waits) // 2]:.2f}/{waits[-1]:.2f}")
        for k, v in (("n", n), ("cut", cut), ("stuck", stuck), ("fixed", fixed), ("learned", learned)):
            totals[k] += v
    print(f"all: {int(totals['n'])} answers, {int(totals['cut'])} healthy cuts "
          f"({totals['cut'] / max(totals['n'], 1):.4%}), {int(totals['stuck'])} stuck; "
          f"fixed 30 s waits lost {totals['fixed']:.1f} s, learned waits {totals['learned']:.1f} s")


if __name__ == "__main__":
    main()
