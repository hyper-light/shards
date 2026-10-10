# The 2 MiB stretches of guest memory each arm's VMM held, from the firecracker bench's
# --layout-ab output (README.md):  python3 layout.py boots.log
import collections
import re
import statistics
import sys

# Each CHUNKS line belongs to the ARM line before it. Per arm: stretches held per boot
# and the 4 KiB pages they hold, and which stretches every boot, or some, held.
chunks = re.compile(r"CHUNKS (\S+) ([0-9a-f]+)\+([0-9a-f]+) size ([0-9a-f]+): (\d+) stretches: (.*)$")
arm_line = re.compile(r"ARM (\S+)\s*$")
by_arm = collections.defaultdict(list)
arm = None
for line in open(sys.argv[1], errors="replace"):
    m = arm_line.search(line)
    if m:
        arm = m.group(1)
        continue
    m = chunks.search(line)
    if m and arm:
        held = {int(i): int(n) for i, n in (item.split(":") for item in m.group(6).split())}
        by_arm[arm].append(held)
        arm = None

for arm, boots in by_arm.items():
    counts = collections.Counter(len(h) for h in boots)
    pages = [sum(h.values()) for h in boots]
    print(f"{arm}: {len(boots)} boots; stretches {dict(sorted(counts.items()))}; "
          f"pages median {statistics.median(pages)} ({statistics.median(pages) * 4 / 1024:.1f} MiB), {min(pages)}-{max(pages)}")
    every = collections.Counter(i for h in boots for i in h)
    print(f"  always: {sorted(i for i, c in every.items() if c == len(boots))}")
    print(f"  sometimes: {dict(sorted((i, c) for i, c in every.items() if c < len(boots)))}")
    sparse = {}
    for i in sorted(every):
        p = [h.get(i, 0) for h in boots if i in h]
        if statistics.median(p) < 512:
            sparse[i] = statistics.median(p)
    if sparse:
        print(f"  not whole (stretch: median pages): {sparse}")
