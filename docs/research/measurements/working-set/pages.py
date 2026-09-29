"""Summarizes the page records of track.patch (PM M30): for each warm VM, the 16 KiB
guest pages first touched before its request, while serving it (request to answer), and
after; by region and by kind of first touch; and how alike different VMs' sets are.

    python3 pages.py DIR
"""
import collections, glob, sys

runs = []
for path in sorted(glob.glob(sys.argv[1] + "/*.pages")):
    lines = open(path).read().split("\n")
    head = lines[0].split()
    t = dict(zip(head[0::2], map(int, head[1::2])))
    regions, pages = [], {}
    for l in lines[1:]:
        f = l.split()
        if not f:
            continue
        if f[0] == "region":
            regions.append((int(f[1], 16), int(f[2], 16)))
        else:
            pages[int(f[0], 16)] = (int(f[1]), f[2], int(f[3]))
    if t["request_us"] == 0:
        continue
    runs.append((path, t, regions, pages))


def region(regions, ipa):
    return "ram" if ipa < regions[0][0] + regions[0][1] else "pmem"


sets = []
for path, t, regions, pages in runs:
    req, ans = t["request_us"], t["answered_us"]
    phase = collections.Counter()
    kinds = collections.Counter()
    during = set()
    writes = 0
    for ipa, (first, kind, wr) in pages.items():
        p = "before" if first < req else "during" if first <= ans else "after"
        phase[p] += 1
        if p == "during":
            during.add(ipa)
            kinds[(region(regions, ipa), kind)] += 1
        if req <= wr <= ans:
            writes += 1
    sets.append(during)
    print(path.split("/")[-1], dict(phase), "request", ans - req, "us;",
          "during by region/kind", dict(sorted(kinds.items())), "writes during", writes)
if len(sets) > 1:
    common = set.intersection(*sets)
    union = set.union(*sets)
    print("request-path sets: common", len(common), "union", len(union),
          "sizes", [len(s) for s in sets])
    for i in range(1, len(sets)):
        a, b = sets[0], sets[i]
        print(f"  run 0 vs {i}: jaccard {len(a & b) / len(a | b):.2f}, "
              f"{len(b - a)} of {len(b)} not in run 0")
