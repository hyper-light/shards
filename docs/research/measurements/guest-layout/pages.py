# The 4 KiB pages each VMM's guest touched, stretch by stretch, from boots with THP off
# (README.md):  python3 pages.py pages.log
import collections
import re
import statistics
import sys

# Boots whose stretches are not all whole: THP was off, so each count is the 4 KiB pages
# the guest touched there. Per arm (the ARM line before each boot's CHUNKS line, else
# the VMM): each stretch's pages, median and range over boots.
pattern = re.compile(r"CHUNKS (\S+) ([0-9a-f]+)\+([0-9a-f]+) size ([0-9a-f]+): (\d+) stretches: (.*)$")
arm_line = re.compile(r"ARM (\S+)\s*$")
boots = collections.defaultdict(list)
arm = None
for line in open(sys.argv[1], errors="replace"):
    a = arm_line.search(line)
    if a:
        arm = a.group(1)
        continue
    m = pattern.search(line)
    if not m:
        continue
    held = {int(i): int(n) for i, n in (item.split(":") for item in m.group(6).split())}
    name, arm = arm or m.group(1).rsplit("/", 1)[-1], None
    if all(n == 512 for n in held.values()):
        continue
    boots[name].append(held)

for vmm, samples in boots.items():
    totals = [sum(h.values()) for h in samples]
    print(f"{vmm}: {len(samples)} boots without THP; 4 KiB pages touched: median {statistics.median(totals)}, "
          f"{min(totals)} to {max(totals)} ({statistics.median(totals) * 4 / 1024:.1f} MiB)")
    stretches = sorted({i for h in samples for i in h})
    for i in stretches:
        pages = [h.get(i, 0) for h in samples]
        print(f"  stretch {i:3} ({i * 2:3}-{i * 2 + 2:3} MiB): pages median {statistics.median(pages):6} "
              f"min {min(pages):4} max {max(pages):4} in {sum(1 for p in pages if p)}/{len(pages)} boots")
