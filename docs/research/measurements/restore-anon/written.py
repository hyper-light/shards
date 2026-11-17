#!/usr/bin/env python3
"""The pages of a restored guest's memory file that shards-vm holds private copies of as its
guest first beats, as two runs of the Firecracker comparison read them (benches/
firecracker.rs `pages_to`: `pages.tsv` beside each run's samples), and those one run's guest
wrote that the other's did not: what one host's restore writes that another's does not.

    written.py PAGES_A PAGES_B [VMLINUX]

Each page by its offset in the memory file. With VMLINUX, the guest's kernel, each page of
its image is also named by the symbols on it (`nm`), where the memory file holds the guest's
RAM from guest-physical 0 and the kernel lies at its link address less __START_KERNEL_map
(x86_64, no KASLR): a per-CPU, slab or page-table page stays unnamed."""
import subprocess
import sys

# arch/x86/include/asm/page_64_types.h: the kernel image's virtual base, physical 0.
START_KERNEL_MAP = 0xFFFFFFFF80000000


def written(path):
    """Each shards-vm restore's privately copied pages of its memory file, by offset, and
    the page size of the host that read them."""
    pages, size = set(), 0
    with open(path) as f:
        for line in f:
            vmm, _, file, _, page, runs = line.rstrip("\n").split("\t")
            if not vmm.endswith("shards-vm restore") or not file.endswith("/memory") or not runs:
                continue
            size = int(page)
            for run in runs.split(","):
                first, count = run[1:].split("+")
                if run[0] == "A":
                    pages.update(int(first, 16) + i * size for i in range(int(count)))
    return pages, size


(a, size), (b, _) = written(sys.argv[1]), written(sys.argv[2])
symbols = []
if len(sys.argv) > 3:
    for line in subprocess.run(
        ["nm", "-n", "-S", "--defined-only", sys.argv[3]], check=True, capture_output=True, text=True
    ).stdout.splitlines():
        fields = line.split()
        if len(fields) == 4 and int(fields[0], 16) >= START_KERNEL_MAP:
            at = int(fields[0], 16) - START_KERNEL_MAP
            symbols.append((at, at + int(fields[1], 16), fields[3]))


def named(page):
    return ",".join(s for start, end, s in symbols if start < page + size and end > page)


print(f"{sys.argv[1]}: {len(a)} pages written; {sys.argv[2]}: {len(b)}")
for title, pages in (("only in A", a - b), ("only in B", b - a)):
    print(f"{title}: {len(pages)}")
    for page in sorted(pages):
        print(f"  {page:#x} {named(page)}")
