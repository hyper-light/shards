#!/usr/bin/env python3
"""Each VMM's mappings of its own binary at boot and at restore, as the Firecracker comparison
says them on stderr (benches/firecracker.rs `describe_mappings`: once a VMM and a phase, each
mapping's resident bytes), and their file-backed sum: what of a VM's `rss_file` its binary is.

    maps.py STDERR...

One line a run, VMM and phase: the file-backed sum (each mapping's resident bytes less its
private copies), then each of the binary's mappings as resident/mapped bytes, `anon` where
its pages are private copies (its written data)."""
import os
import re
import sys

HEAD = re.compile(r"(\S+) (boot|restore) \(pid \d+\) mappings by resident bytes:")
ROW = re.compile(
    r"rss +(\d+) anon +(\d+) pmd +\d+ huge +\d+ size +(\d+) at \+0x[0-9a-f]+ of 2 MiB  (\S+)"
)

for path in sys.argv[1:]:
    with open(path, errors="replace") as f:
        lines = f.read().splitlines()
    for i, line in enumerate(lines):
        head = HEAD.search(line)
        if not head:
            continue
        program, phase = head.group(1), head.group(2)
        own = []
        for row in lines[i + 1 :]:
            mapping = ROW.search(row)
            if not mapping:
                break
            if mapping.group(4) == program:
                own.append(tuple(int(mapping.group(n)) for n in (1, 2, 3)))
        file_backed = sum(rss - anon for rss, anon, _ in own)
        parts = " + ".join(
            f"{rss}/{size}{' anon' if anon else ''}" for rss, anon, size in own
        )
        print(
            f"{os.path.basename(path)} {os.path.basename(program)} {phase}: "
            f"file-backed {file_backed} = {parts}"
        )
