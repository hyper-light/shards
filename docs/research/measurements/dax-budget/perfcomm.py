#!/usr/bin/env python3
"""Samples of a perf.data file by thread name and CPU mode, without perf: the file's
format as tools/perf/Documentation/perf.data-file-format.txt gives it (PERFILE2), the
records as include/uapi/linux/perf_event.h lays them out.

    perfcomm.py PERF.DATA
"""
import collections, struct, sys

path = sys.argv[1]
data = open(path, "rb").read()
assert data[:8] == b"PERFILE2", "not a perf.data v2 file"
size, attr_size = struct.unpack_from("<QQ", data, 8)
attrs_off, attrs_size, data_off, data_size = struct.unpack_from("<QQQQ", data, 24)
# The first attr's sample_type and whether sample_id_all is set.
attr = data[attrs_off:attrs_off + attr_size]
a_type, a_size, config, period, sample_type, read_format, flags = struct.unpack_from("<IIQQQQQ", attr, 0)
SAMPLE = {"IP": 1 << 0, "TID": 1 << 1, "TIME": 1 << 2, "ADDR": 1 << 3, "READ": 1 << 4, "CALLCHAIN": 1 << 5,
          "ID": 1 << 6, "CPU": 1 << 7, "PERIOD": 1 << 8, "STREAM_ID": 1 << 9, "IDENTIFIER": 1 << 16}
has = lambda k: sample_type & SAMPLE[k]
comms = {}
by = collections.Counter()
modes = collections.Counter()
MODE = {0: "unknown", 1: "kernel", 2: "user", 3: "hypervisor", 4: "guest kernel", 5: "guest user"}
off, end = data_off, data_off + data_size
total = 0
while off + 8 <= end:
    rtype, misc, rsize = struct.unpack_from("<IHH", data, off)
    if rsize == 0:
        break
    body = off + 8
    if rtype == 3:  # PERF_RECORD_COMM
        pid, tid = struct.unpack_from("<II", data, body)
        name = data[body + 8: off + rsize].split(b"\0", 1)[0].decode(errors="replace")
        comms[tid] = name
    elif rtype == 9:  # PERF_RECORD_SAMPLE
        p = body
        if has("IDENTIFIER"):
            p += 8
        if has("IP"):
            p += 8
        pid = tid = None
        if has("TID"):
            pid, tid = struct.unpack_from("<II", data, p)
            p += 8
        total += 1
        by[tid] += 1
        modes[MODE.get(misc & 7, str(misc & 7))] += 1
    off += rsize
print(f"{path}: {total} samples; sample_type {sample_type:#x}")
print("  by mode:", ", ".join(f"{m} {n * 100 / total:.1f}%" for m, n in modes.most_common()))
named = collections.Counter()
for tid, n in by.items():
    named[comms.get(tid, f"tid {tid}")] += n
for name, n in named.most_common(15):
    print(f"  {n * 100 / total:5.1f}%  {name}")
