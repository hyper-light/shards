#!/usr/bin/env python3
"""A timeshare thread of the host, at its default QoS as any program's: it sleeps 1 ms
over and over for SECONDS and prints, each time it woke more than 5 ms late, the seconds
since its start and how late, in ms. What the host's other work waits while real-time
threads hold its CPUs.

    probe.py SECONDS
"""
import sys, time

seconds = float(sys.argv[1])
start = time.monotonic()
while (before := time.monotonic()) - start < seconds:
    time.sleep(0.001)
    late = (time.monotonic() - before - 0.001) * 1000
    if late > 5:
        print(f"{before - start:.3f} {late:.1f}", flush=True)
