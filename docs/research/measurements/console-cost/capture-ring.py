#!/usr/bin/env python3
"""Captures what crates/vmm's printk reader reads of a real guest kernel's printk ring, from a
snapshot's memory file, as a unit-test fixture (D261): the VMCOREINFO page, the `prb`
pointer, the ring's struct, and its descriptor, info and text arrays, each kept as its
runs of non-zero bytes (the rest of an array is zeros, which the test's guest memory
holds anyway). An x86-64 guest of shards' layout: its RAM is the memory file's first
bytes, the file's offset its guest-physical address (vmm memory.rs `save`).

Format: b"SHRDRING", u32 runs, then each: u64 guest-physical address, u32 length, bytes;
little-endian.

Usage: capture-ring.py MEMORY_FILE OUT
"""

import re
import struct
import sys

PAGE = 4096
START_KERNEL_MAP = 0xFFFF_FFFF_8000_0000
PAGE_OFFSET_L4, PAGE_OFFSET_L5 = 0xFFFF_8880_0000_0000, 0xFF11_0000_0000_0000

mem = open(sys.argv[1], "rb").read()
info_at = next(
    (at for at in range(0, len(mem), PAGE) if mem[at:at + 10] == b"OSRELEASE="), None)
if info_at is None:
    sys.exit("no VMCOREINFO page")
text = mem[info_at:info_at + PAGE].split(b"\0")[0].decode()
info = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


def number(key):
    v = info[key]
    return int(v, 16) if v.startswith("0x") else int(v)


offset = lambda key: int(info[key])
symbol = lambda key: int(info[key], 16)
if "NUMBER(kimage_voffset)" in info:
    sys.exit("an arm64 guest: its RAM is not at the file's offset 0")
phys_base = number("NUMBER(phys_base)")
page_offset = PAGE_OFFSET_L5 if number("NUMBER(pgtable_l5_enabled)") else PAGE_OFFSET_L4


def pa(va):
    return va - START_KERNEL_MAP + phys_base if va >= START_KERNEL_MAP else va - page_offset


u64 = lambda va: struct.unpack_from("<Q", mem, pa(va))[0]
u32 = lambda va: struct.unpack_from("<I", mem, pa(va))[0]
prb_var = symbol("SYMBOL(prb)")
prb = u64(prb_var)
desc_ring = prb + offset("OFFSET(printk_ringbuffer.desc_ring)")
text_ring = prb + offset("OFFSET(printk_ringbuffer.text_data_ring)")
count = 1 << u32(desc_ring + offset("OFFSET(prb_desc_ring.count_bits)"))
size = 1 << u32(text_ring + offset("OFFSET(prb_data_ring.size_bits)"))
ranges = [
    (info_at, PAGE),
    (pa(prb_var), 8),
    (pa(prb), offset("SIZE(printk_ringbuffer)")),
    (pa(u64(desc_ring + offset("OFFSET(prb_desc_ring.descs)"))), count * offset("SIZE(prb_desc)")),
    (pa(u64(desc_ring + offset("OFFSET(prb_desc_ring.infos)"))), count * offset("SIZE(printk_info)")),
    (pa(u64(text_ring + offset("OFFSET(prb_data_ring.data)"))), size),
]
runs = []
for start, length in ranges:
    chunk = mem[start:start + length]
    for m in re.finditer(rb"[^\0](?:[^\0]|\0{1,15}(?=[^\0]))*", chunk):
        runs.append((start + m.start(), m.group()))
with open(sys.argv[2], "wb") as out:
    out.write(b"SHRDRING" + struct.pack("<I", len(runs)))
    for gpa, data in runs:
        out.write(struct.pack("<QI", gpa, len(data)) + data)
print(f"{info.get('OSRELEASE')}: {len(runs)} runs, {sum(len(d) for _, d in runs)} bytes, "
      f"ring of {count} records and {size} bytes of text, RAM to {max(g + len(d) for g, d in runs):#x}")
