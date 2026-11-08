#!/usr/bin/env python3
"""PM M489 (D227): which pages a run restored from a template touches and writes, from the
working set the daemon keeps beside the template's generation (crates/vmm/src/snapshot
`encode_working_set`: each page's guest-physical address, its low bit set where the guest
wrote it, recorded by the first runs restored from it), each page classed by what the
template's memory file holds there before the restore:

    brk     at least half its words the JIT's fill: AARCH64_BREAK_FAULT (0xd4202000,
            arch/arm64/include/asm/debug-monitors.h) on arm64, int3 (0xcc) on x86_64:
            the BPF pack's unused chunks
    zero    all zeros (free memory)
    other   anything else

    written.py GENERATION...

GENERATION is a template's generation directory (templates/KEY/g-...), holding `memory`
and `working-set`. Prints, per generation, the pages touched and written, by class, in
pages of the working set's page size, and in KiB.
"""
import os, struct, sys

DRAM_BASE = {"aarch64": 0x8000_0000, "x86_64": 0}  # crates/vmm/src/arch/*: guest RAM's start
MAGIC = b"SHRDWSET"


def decode(path):
    b = open(path, "rb").read()
    if b[:8] != MAGIC:
        raise SystemExit(f"{path}: not a shards working set")
    (version,) = struct.unpack_from("<I", b, 8)
    (n,) = struct.unpack_from("<I", b, 12)
    arch = b[16:16 + n].decode()
    at = 16 + n
    (page,) = struct.unpack_from("<Q", b, at)
    (count,) = struct.unpack_from("<I", b, at + 8)
    words = struct.unpack_from(f"<{count}Q", b, at + 12)
    return version, arch, page, [(w & ~1, bool(w & 1)) for w in words]


def kind(data, arch):
    if not data.strip(b"\0"):
        return "zero"
    if arch == "aarch64":
        words = struct.unpack(f"<{len(data) // 4}I", data)
        fill = sum(1 for w in words if w == 0xD4202000) / len(words)
    else:
        fill = data.count(0xCC) / len(data)
    return "brk" if fill >= 0.5 else "other"


for gen in sys.argv[1:]:
    version, arch, page, pages = decode(os.path.join(gen, "working-set"))
    base = DRAM_BASE[arch]
    counts = {}
    with open(os.path.join(gen, "memory"), "rb") as mem:
        for gpa, written in pages:
            mem.seek(gpa - base)
            k = kind(mem.read(page), arch)
            key = ("written" if written else "read", k)
            counts[key] = counts.get(key, 0) + 1
    total_w = sum(v for (w, _), v in counts.items() if w == "written")
    print(f"{gen}: {arch}, {page // 1024} KiB pages, {len(pages)} touched, {total_w} written "
          f"({total_w * page // 1024} KiB)")
    for (w, k), v in sorted(counts.items()):
        print(f"   {w:<8} {k:<6} {v:>6} pages {v * page // 1024:>8} KiB")
