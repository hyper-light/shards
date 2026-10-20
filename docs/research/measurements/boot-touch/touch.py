#!/usr/bin/env python3
"""Which of a booted guest's memory the host holds, 2 MiB stretch by stretch (Linux): what
the Firecracker comparison's boot (crates/shards/benches/firecracker.rs) leaves resident,
with and without huge-page advice (SHARDS_THP_ADVICE), to see where huge pages hold more
than the guest touched.

    touch.py SHARDS_VM KERNEL INIT MEMORY_MIB ROUNDS ADVICE

ADVICE is `default` or `never`. Each round boots `SHARDS_VM run --kernel KERNEL --init INIT
--cpus 1 --memory MEMORY_MIB` (INIT the test guest, packed as the comparison packs it) with
the comparison's command line, waits for the
guest's ready line and 200 ms more (as the comparison reads its memory), then reads the
VM process's /proc/PID/smaps and /proc/PID/pagemap for its guest memory: each 4 KiB
page present or not (pagemap's bit 63, which needs no privilege), and the huge pages
(AnonHugePages). Prints, per round, the pages present, the 2 MiB stretches with any, and
each stretch's count; then every stretch's count in every round, for comparing."""
import os, re, subprocess, sys, time

vm, kernel, init, mib, rounds, advice = sys.argv[1:7]
mib, rounds = int(mib), int(rounds)
size = mib << 20
PAGE, STRETCH = 4096, 2 << 20
env = dict(os.environ)
if advice == "never":
    env["SHARDS_THP_ADVICE"] = "never"
else:
    env.pop("SHARDS_THP_ADVICE", None)
cmdline = "console=ttyS0 quiet panic=-1 shards_test=idle"


def guest_vmas(pid):
    """Guest memory: `size` bytes of anonymous mappings from a start the reservation aligns
    to a huge page (platform::reserve_huge), which its first 2 MiB's own advice splits
    from the rest; guest address 0 at that start (x86_64's RAM below its hole)."""
    vmas = []
    for line in open(f"/proc/{pid}/maps"):
        parts = line.split()
        lo, hi = (int(x, 16) for x in parts[0].split("-"))
        vmas.append((lo, hi, len(parts) < 6))
    found = []
    for i, (lo, _, anon) in enumerate(vmas):
        if not anon or lo % STRETCH:
            continue
        end, k = lo, i
        while k < len(vmas) and vmas[k][2] and vmas[k][0] == end and end < lo + size:
            end = vmas[k][1]
            k += 1
        if end >= lo + size:
            found.append(lo)
    if len(found) != 1:
        raise SystemExit(f"{len(found)} places guest memory of {size} bytes could be in /proc/{pid}/maps")
    return found[0], found[0] + size


def huge_in(pid, lo, hi):
    total, cur = 0, None
    for line in open(f"/proc/{pid}/smaps"):
        m = re.match(r"^([0-9a-f]+)-([0-9a-f]+) ", line)
        if m:
            a, b = int(m.group(1), 16), int(m.group(2), 16)
            cur = lo <= a and b <= hi
            continue
        if cur and line.startswith("AnonHugePages:"):
            total += int(line.split()[1])
    return total << 10


every = []
for r in range(rounds):
    p = subprocess.Popen([vm, "run", "--kernel", kernel, "--init", init, "--cpus", "1",
                          "--memory", str(mib), "--cmdline", cmdline],
                         env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        for line in p.stdout:
            if b"SHARDS-TEST READY" in line:
                break
        time.sleep(0.2)
        lo, hi = guest_vmas(p.pid)
        pages = (hi - lo) // PAGE
        with open(f"/proc/{p.pid}/pagemap", "rb") as pm:
            pm.seek(lo // PAGE * 8)
            raw = pm.read(pages * 8)
        present = [int.from_bytes(raw[i * 8:i * 8 + 8], "little") >> 63 for i in range(pages)]
        huge = huge_in(p.pid, lo, hi)
    finally:
        p.kill()
        p.wait()
    per = [sum(present[s:s + STRETCH // PAGE]) for s in range(0, pages, STRETCH // PAGE)]
    every.append(per)
    touched = [c for c in per if c]
    print(f"round {r}: {sum(present)} pages present ({sum(present) * 4 / 1024:.1f} MiB), "
          f"{len(touched)} stretches with any, {huge >> 20} MiB in huge pages; "
          f"stretches under half full: {sum(1 for c in touched if c < 256)}")
print("stretch " + " ".join(f"r{r}" for r in range(rounds)))
for s in range(len(every[0])):
    counts = [e[s] if s < len(e) else 0 for e in every]
    if any(counts):
        print(f"{s:7d} " + " ".join(f"{c:3d}" for c in counts))
