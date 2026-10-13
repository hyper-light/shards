# Boot exits

Where a cold boot's time goes on an x86_64 KVM runner, shards against Firecracker, for the
Firecracker comparison's `to_ready` (crates/shards/benches/firecracker.rs): the two tie
within about 1% (in the guest: −2 µs at the median; outside it, the spawn, the VMM's
setup, the kernel's start and the ready line's way out: +0.3 ms), and the envelope fails
where a run's interval lands above zero.

`measure.py` boots the comparison's guest (the pinned kernel, the test guest in `idle`
mode, the same command line, 1 vCPU, 128 MiB) with each VMM, interleaved:

1. under `perf kvm stat record` to the guest's ready line, then `perf kvm stat report` of
   VM exits, port I/O and MMIO by reason, count and time, boot by boot;
2. once each with the kernel's initcalls timed (`initcall_debug`), each initcall's time,
   shards less Firecracker, the largest differences first;
3. shards once with its log at debug, the VM process's setup in microseconds.

`.github/workflows/boot-exits.yml` runs it on two runners after the comparison's
`--runs 1` has built and fetched its guest; each uploads its directory as an artifact.
