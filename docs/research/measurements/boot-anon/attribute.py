"""The boot's anonymous memory, shards' against Firecracker's, taken apart (Envelope,
`firecracker/rss_anon`; PM M617).

For each pair of boots (the comparison's samples, shards' and Firecracker's of the same run
index), the gap in `rss_anon`, shards less Firecracker, as three parts that add up to it:

- own: each VMM's anonymous memory outside its guest's memory (its `rss_anon` less its guest
  memory's `Rss`), shards' less Firecracker's;
- rounding: in each huge page's span of guest memory (2 MiB under 4 KiB pages) that one VMM
  holds in huge pages and the other does not, the pages the other's guest left untouched
  there: what THP's advice holds of pages its guest did not touch, taking the guest to touch
  under one VMM what it touched under the other;
- touched: the rest, span by span: pages one VMM's guest touched and the other's did not.

A span is held in huge pages when it lies in a mapping that huge pages hold whole (its
`AnonHugePages` its length). A mapping huge pages hold in part is not placed: its huge pages
count as touched, and both VMMs' such bytes are said apart (`unplaced`). The parts count the
pages pagemap says each span holds; the gap and `own` take smaps' `Rss`, read a moment
before: what the two readings saw apart is said too (`apart`), so gap = own + rounding +
touched + apart, pair by pair (the medians of the parts need not add up to the gap's).

From the bench's `firecracker-guest.tsv` (benches/firecracker.rs `write_guest`), each boot's
row in the order taken. Prints each run's medians over its pairs, in MiB, and the huge
pages' spans each VMM's guest held pages in.

Usage, from the repository's root: python3 -I attribute.py DIR... (directories holding a
`firecracker-guest.tsv`, searched below each).
"""

import csv
import os
import statistics
import sys

MIB = 1 << 20


def tables(roots):
    for root in roots:
        for here, _, files in sorted(os.walk(root)):
            if "firecracker-guest.tsv" in files:
                yield here, os.path.join(here, "firecracker-guest.tsv")


def huge_spans(row, spans):
    """Which huge pages' spans of guest memory lie in a mapping huge pages hold whole, and
    the huge bytes of mappings they hold in part."""
    huge = int(row["huge"])
    whole, unplaced = [False] * spans, 0
    for mapping in row["spans"].split(","):
        at, rest = mapping.split("+")
        length, held = rest.split(":")
        at, length, held = int(at, 16), int(length, 16), int(held)
        if held == length:
            for span in range(at // huge, (at + length) // huge):
                whole[span] = True
        else:
            unplaced += held
    return whole, unplaced


def apart(ours, theirs):
    """One pair's gap and its parts, in bytes: gap, own, rounding, touched, apart and
    unplaced."""
    page, huge = int(ours["page"]), int(ours["huge"])
    per = huge // page
    held = [[int(n) for n in row["held"].split(",")] for row in (ours, theirs)]
    spans = max(len(h) for h in held)
    held = [h + [0] * (spans - len(h)) for h in held]
    (mine, my_unplaced), (yours, your_unplaced) = (huge_spans(row, spans) for row in (ours, theirs))
    rounding = touched = 0
    for span in range(spans):
        a, b = held[0][span], held[1][span]
        if mine[span] and not yours[span]:
            rounding, touched = rounding + per - b, touched + a - per
        elif yours[span] and not mine[span]:
            rounding, touched = rounding - (per - a), touched + per - b
        else:
            touched += a - b
    anon = [int(row["rss_anon"]) for row in (ours, theirs)]
    guest = [int(row["guest_rss"]) for row in (ours, theirs)]
    gap, own = anon[0] - anon[1], (anon[0] - guest[0]) - (anon[1] - guest[1])
    rounding, touched = rounding * page, touched * page
    return gap, own, rounding, touched, gap - own - rounding - touched, my_unplaced + your_unplaced


def main(roots):
    print(
        f"{'run':36} {'pairs':>5}  {'gap':>6}  {'own':>6}  {'rounding':>8}  {'touched':>7}  {'apart':>6}"
        f"  {'unplaced':>8}  {'shards spans':>12}  {'fc spans':>8}"
    )
    for here, path in tables(roots):
        with open(path) as f:
            rows = [r for r in csv.DictReader(f, delimiter="\t") if r["guest_rss"] != "-"]
        ours = [r for r in rows if r["vmm"] == "shards"]
        theirs = [r for r in rows if r["vmm"] == "firecracker"]
        parts = [apart(a, b) for a, b in zip(ours, theirs)]
        if not parts:
            print(f"{here}: no pairs whose guest memory was read")
            continue
        med = [statistics.median(p[i] for p in parts) / MIB for i in range(6)]
        spans = [
            statistics.median(sum(1 for n in r["held"].split(",") if int(n) > 0) for r in side)
            for side in (ours, theirs)
        ]
        print(
            f"{here[-36:]:36} {len(parts):5}  {med[0]:+6.3f}  {med[1]:+6.3f}  "
            f"{med[2]:+8.3f}  {med[3]:+7.3f}  {med[4]:+6.3f}  {med[5]:8.3f}  {spans[0]:12.0f}  {spans[1]:8.0f}"
        )


if __name__ == "__main__":
    main(sys.argv[1:] or ["."])
