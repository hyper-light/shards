#!/usr/bin/env python3
"""smaps.sh's dumps of a VM process, without a window and with one (PM M307): each's total
Rss, and its twelve mappings of the most Rss, each's Rss, AnonHugePages, size and name.

    smaps.py NONE WITH"""
import re, sys

def read(path):
    maps, cur = [], None
    for line in open(path):
        m = re.match(r"^([0-9a-f]+)-([0-9a-f]+) (\S+) \S+ \S+ \S+\s*(.*)$", line)
        if m:
            cur = {"size": int(m[2], 16) - int(m[1], 16), "perms": m[3], "name": m[4].strip(), "Rss": 0, "AnonHugePages": 0}
            maps.append(cur)
        elif cur is not None:
            k = line.split(":")[0]
            if k in ("Rss", "AnonHugePages"):
                cur[k] = int(line.split()[1])
    return maps

for path in sys.argv[1:]:
    maps = read(path)
    print(path, "total Rss", sum(m["Rss"] for m in maps), "KiB")
    for m in sorted(maps, key=lambda m: -m["Rss"])[:12]:
        print(f"  {m['Rss']:8} KiB rss {m['AnonHugePages']:8} KiB huge  {m['size'] >> 20:9} MiB  {m['perms']} {m['name']}")
