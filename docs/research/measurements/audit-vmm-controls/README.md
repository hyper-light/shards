# Guest-controlled diagnostics and expired HVF handles

Run from the repository root:

```sh
python3 docs/research/measurements/audit-vmm-controls/run.py --n 30
python3 docs/research/measurements/audit-vmm-controls/run.py --n 30 --hardware
```

This standalone workspace executes actual public `Control`, `Serial`, `hv::Vm`,
`Gic`, and `Kicker` APIs. It changes no product source. The saved results include
revision, relevant source hashes, dirty state, compiler/host/OS, executable hash,
separate allocation counts, raw timing samples and nearest-rank quantiles.
Copy the harness into an isolated checkout of the recorded revision to reproduce
that source; compare relevant hashes if concurrent edits were recorded.

Marker probes write 512–262,144 guest marker values through `MmioDevice::write`.
Info logging is disabled, and its timestamp clock is initialized before measurement.
Each sample is a fresh process. A warmup is discarded for each size, followed by
30 timing samples in reproducibly randomized order. Counting is disabled during
timings, leaving the allocator flag check. One separate census measures allocations,
reallocations, cumulative requests, peak and retained requested heap. Another census
measures the public marker getter's clone. Input/output and content checking are
outside intervals. These are host API timings, excluding actual VM exits, not an
attack-rate or Firecracker benchmark. Capacity excludes allocator metadata, native
allocations, mappings, kernel and HVF memory; it is not RSS/PSS. Reallocation counts
do not establish copying by malloc. The single-threaded count phases free none of
the allocations created before those phases.

The serial probe fills a real pipe nonblocking, restores blocking mode, and calls
`Serial::write` on a separate thread. It checks completion during a 200 ms window,
then drains the pipe and joins the thread. It starts no VM. This proves the device
call blocks behind host output; the audit separately traces its effect on vCPU
teardown. Five repetitions are correctness observations, not latency measurements.

`--hardware` requires macOS arm64 and a host with HVF. It signs the local executable
with the repository's hypervisor entitlement. Five repetitions of each probe create
and destroy sequential VMs, leaving no CPU or VM behind. The GIC probe uses the actual
old public handle to inject into a new VM; SDK distributor APIs are used only to
configure and observe SPI33. The kicker probe destroys its original vCPU and VM,
creates a replacement, first proves BRK traps, then calls the old kicker and checks
the replacement instead returns Canceled. Its one instruction is in an isolated
16 KiB RAM page whose mapping outlives both the CPU and VM. No kernel, image, network,
existing VM or workload is involved.
