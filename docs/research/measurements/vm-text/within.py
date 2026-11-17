"""What each run of the Firecracker comparison holds of its own, its snapshot taken out.

Every restore of a run restores one snapshot, whose guest's private copies are the same in
each of them and differ from run to run (shards' working set, copied ahead, is recorded once
a run). So each run's memory rows are one draw of its snapshot, and its Envelope's interval,
over restores alike, a point. Two differences within a run take the snapshot out, per VMM,
in 4 KiB pages:

- `own`: each restore's anonymous memory (`rss_anon`) less its guest's private copies
  (`written_pages`, the kvm table's, paired by sample): the VMM's own anonymous pages,
  their median and range.
- `fleet+`: the density's private pages a VM (`fleet_private`, its fleet of restores of the
  same snapshot held) less the restore's `rss_anon`, medians: what a fleet adds to a VM.

Usage, from the repository's root: python3 -I within.py OUT... (run.sh's output directories;
a run is a directory beside its .stderr, holding the bench's samples).
"""

import csv
import json
import os
import statistics
import sys

PAGES = 256  # 4 KiB pages in a MiB, the samples' unit
VMMS = (("shards", "shards"), ("firecracker", "fc"))


def runs(out):
    for name in sorted(os.listdir(out)):
        path = os.path.join(out, name)
        if all(
            os.path.exists(os.path.join(path, f))
            for f in ("firecracker_restore-0.json", "firecracker_density-0.json", "firecracker_restore-kvm.tsv")
        ):
            yield name, path


def main(outs):
    print("run      vmm          host                      revision  copies  rss_anon       own [min..max]  fleet_private  fleet+")
    for out in outs:
        for name, path in runs(out):
            restore = json.load(open(os.path.join(path, "firecracker_restore-0.json")))
            density = json.load(open(os.path.join(path, "firecracker_density-0.json")))["samples"]
            table = list(csv.DictReader(open(os.path.join(path, "firecracker_restore-kvm.tsv")), delimiter="\t"))
            for vmm, key in VMMS:
                copies = [float(r["written_pages"]) for r in table if r["vmm"] == vmm]
                anon = [v * PAGES for v in restore["samples"][f"{key}_rss_anon"]["values"]]
                own = [round(a - c) for a, c in zip(anon, copies)]
                private = statistics.median(v * PAGES for v in density[f"{key}_fleet_private"]["values"])
                print(
                    f"{name:8} {vmm:12} {restore['host'][:25]:25} {restore['revision']:9} "
                    f"{statistics.median(copies):6.0f}  {statistics.median(anon):8.1f}  "
                    f"{f'{statistics.median(own):.0f} [{min(own)}..{max(own)}]':>19}  "
                    f"{private:13.1f}  {private - statistics.median(anon):+6.1f}"
                )


if __name__ == "__main__":
    main(sys.argv[1:] or ["."])
