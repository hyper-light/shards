"""Turns one page record of track.patch into a prefetch list (PM M30): each page first
touched by the time the VM answered its request, as `IPA w` if the guest had written it
by then, else `IPA r`, in the order the guest first touched them.

    python3 list.py PAGES > LIST
"""
import sys

lines = open(sys.argv[1]).read().split("\n")
head = lines[0].split()
t = dict(zip(head[0::2], map(int, head[1::2])))
pages = []
for l in lines[1:]:
    f = l.split()
    if not f or f[0] == "region":
        continue
    first, kind, written = int(f[1]), f[2], int(f[3])
    if first <= t["answered_us"]:
        pages.append((first, f[0], "w" if 0 < written <= t["answered_us"] else "r"))
for _, ipa, kind in sorted(pages):
    print(ipa, kind)
