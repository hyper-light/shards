"""How long cargo's name for a checkout of each commit of REV's history is.

Cargo names a git dependency's checkout directory by its commit's short id: 7 hex digits,
lengthened while they name more than one object of its database (cargo 5f94df478,
src/sources/git/utils.rs `to_short_id`). Counted here against every object of this
clone, more than a database of REV's history alone holds, so no length is understated.

    python3 -I short-ids.py [REV]   (run inside a clone)
"""
import bisect
import collections
import subprocess
import sys

rev = sys.argv[1] if len(sys.argv) > 1 else "HEAD"
git = lambda *a: subprocess.run(["git", *a], capture_output=True, text=True, check=True).stdout
objects = sorted(set(git("cat-file", "--batch-all-objects", "--batch-check=%(objectname)").split()))
commits = git("rev-list", rev).split()


def common(a, b):
    n = 0
    while n < len(a) and a[n] == b[n]:
        n += 1
    return n


lengths = collections.Counter()
for c in commits:
    i = bisect.bisect_left(objects, c)
    shared = max([common(objects[j], c) for j in (i - 1, i + 1) if 0 <= j < len(objects)] or [0])
    lengths[max(7, shared + 1)] += 1
print(f"objects {len(objects)}, commits of {rev} {len(commits)}, short ids by length {dict(sorted(lengths.items()))}")
