#!/usr/bin/env python3
"""A symbol ordering file for LLD (`--symbol-ordering-file`) that lays out first the
functions of BINARY that its boot touched, from PAGES, the Firecracker comparison's
`pages.tsv` (benches/firecracker.rs `pages_to`) of a run with the host's fault-around off,
where the pages present of a binary's text are the pages its boot touched.

    order.py BINARY PAGES > order.txt

Each function whose bytes overlap a touched page of BINARY's executable segment, by its
symbol in BINARY's symbol table (`nm`), in BINARY's own order, so that what the boot runs
keeps its order and lies together. A page holds functions the boot did not run as well; a
second round, ordered by a run of the first's binary, keeps fewer."""
import os
import subprocess
import sys

binary, pages = sys.argv[1], sys.argv[2]
name = os.path.basename(binary)

# The executable segment: its file offset, virtual address and size in the file.
text = None
for line in subprocess.run(
    ["readelf", "-lW", binary], check=True, capture_output=True, text=True
).stdout.splitlines():
    fields = line.split()
    if fields[:1] == ["LOAD"] and "E" in fields[6:-1]:
        text = (int(fields[1], 16), int(fields[2], 16), int(fields[4], 16))
if text is None:
    sys.exit(f"{binary}: no executable segment")
offset, vaddr, size = text

# Each page of BINARY present in a boot's reading, by its file offset, in the page size of
# the host that read it.
touched, page = set(), None
with open(pages) as f:
    for line in f:
        vmm, _, file, _, size_of_page, runs = line.rstrip("\n").split("\t")
        if not vmm.endswith(" boot") or os.path.basename(file) != name or not runs:
            continue
        page = int(size_of_page)
        for run in runs.split(","):
            first, count = run[1:].split("+")
            touched.update(int(first, 16) + i * page for i in range(int(count)))
if page is None:
    sys.exit(f"{pages}: no boot reading of {name}")

nm = subprocess.run(
    ["nm", "-n", "-S", "--defined-only", "--no-demangle", binary],
    check=True,
    capture_output=True,
    text=True,
).stdout
for line in nm.splitlines():
    fields = line.split()
    if len(fields) != 4 or fields[2] not in ("t", "T", "w", "W"):
        continue
    address, length, symbol = int(fields[0], 16), int(fields[1], 16), fields[3]
    if length == 0 or not vaddr <= address < vaddr + size:
        continue
    start = address - vaddr + offset
    first, last = start // page * page, (start + length - 1) // page * page
    if any(p in touched for p in range(first, last + 1, page)):
        print(symbol)
