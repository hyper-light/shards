# The engine path through a real VM: vsock, its windows, and a shared ring's ceiling

ROOT coordinator 01a148884a4770fdaf0111f3f792a248, 2026-11-11. POLICY_READ main bff30151, rules
df2acf47. Measurement-only branch (p/vsock-path-v1), never merged: it carries knobs the product
must not (`SHARDS_VSOCK_CREDIT`, `SHARDS_VSOCK_SOCKBUF`, read by the VM process), the test guest's
`vsock_bench` and `dax_ring` modes, and the ignored tests `vsock_path_measurement` and
`dax_ring_measurement` in crates/shards/tests/vsock.rs. Answers docs/research/virtio-io-exits.md
open question M8 (vsock throughput and latency were never measured).

## Harness

- `vsock_path_measurement`: one test-guest VM per configuration (`VSOCK_BENCH_CONFIGS`,
  `label:VAR=VALUE,...`), every configuration and arm interleaved in a rotated order each round.
  Arms: 256 MiB host to guest (K), guest to host (S), both through an echo (E), each directly over
  vsock and through the engine relay's own pump in the guest (lower case: a Unix client in the
  guest, `relay.rs::pump` with `Slots::vsock_packet` between it and vsock, EnginePeerEnd as init
  runs it); then 64-byte round trips. n, p50, p90, p99, max; on Linux the VM's process CPU, split
  into the vsock worker thread and the vCPU threads. `VSOCK_GUEST_BUF` makes the guest set its
  socket's vsock buffer (SO_VM_SOCKETS_BUFFER_MAX_SIZE, then SIZE).
- `dax_ring_measurement`: an 8 MiB file in a `--share` directory with a DAX window
  (`--share-window 64`); the guest mounts it `dax=inode` and maps it, the test maps the same file;
  a 4 MiB SPSC ring with acquire/release indices in the first page; both sides spin.

## Results (p50 of 256 MiB unless said)

Runner 451397 "adacachy": Linux 7.2.9-cachyos, AMD Ryzen 9 9950X3D2, 32 CPUs, KVM, busy (load
in each row). Mac: M3 Ultra (32 cores), macOS 26.4, HVF.

r378 (KVM, load 76-90, 11 rounds), the product as it was (64 KiB window):
host->guest 106.5 ms direct / 109.8 relayed; guest->host 143.2 / 238.9; echo 226.2 / 253.2;
64 B round trip 53.7 us / 63.6 us (p99 105 / 129).

r380 (relay alone over socket pairs, measurement branch p/relay-throughput-v1): no relay at all
11.2 ms, pump 15.6, splice relay 12.0: the relay is within 1.4x of the floor any relay has.

Credit stall (r399, r406, r414 and a header trace): windows above 256 KiB stopped guest-to-host
streams at 262,144 bytes. Cause: Linux 8ee784fdf006 (in the pinned 6.18.48) caps a socket's TX
credit at its own buffer; our device credited only near its whole window. Fixed on
p/vsock-credit-v1 (docs/research/measurements/vsock-credit-v1).

r429 (KVM, load 27-58, 11 rounds), the fixed credit rule, host window = guest buffer:

| window | h->g relayed | g->h direct | g->h relayed | echo relayed | VM CPU g->h direct |
|---|---|---|---|---|---|
| 64 KiB | 123.6 | 194.8 | 244.4 | 268.6 | 250 ms |
| 256 KiB | 127.1 | 87.5 | 117.8 | 236.5 | 120 |
| 1 MiB | 90.5 | 55.5 | 102.0 | 182.5 | 90 |
| 4 MiB | 85.9 | 52.6 | 84.3 | 127.9 | 70 |
| 16 MiB | 92.2 | 57.7 | 75.2 | 117.5 | 70 |

r425 (KVM, load 45-117, 7 rounds) agrees within noise.

r421 (M3 Ultra, load 3-4, 7 rounds), host socket buffers with the 64 KiB window (macOS gives Unix
stream sockets 8 KiB, XNU `PIPSIZ`, net.local.stream.sendspace; a sender's in-flight data is
bounded by its send buffer, bsd/kern/uipc_usrreq.c at xnu f6217f89 lines 452-475, 594-660,
957-969):

| socket buffers | h->g direct | g->h direct | g->h relayed | echo relayed | 64 B RTT direct / relayed |
|---|---|---|---|---|---|
| 8 KiB (default) | 285.5 | 355.1 | 396.3 | 323.6 | 31.9 / 34.7 us |
| 64 KiB | 106.0 | 90.3 | 150.0 | 187.9 | 30.5 / 40.6 us |
| 256 KiB - 4 MiB | 104.8-105.9 | 89.1-90.4 | 148.1-150.9 | 187.5-189.3 | ~30 / ~41 us |

r430 (KVM, load 38-55, 11 rounds), the shared ring's ceiling, both sides spinning:

| direction | p50 | p90 | max | MiB/s at p50 |
|---|---|---|---|---|
| guest -> host | 3.8 ms | 6.4 | 24.9 | 68,224 |
| host -> guest | 25.1 ms | 47.4 | 55.1 | 10,214 |

r433 (M3 Ultra, load 2-3, 11 rounds), the fixed credit rule, host window = guest buffer = host
socket buffers unless said:

| config | h->g relayed | g->h direct | g->h relayed | echo relayed | 64 B RTT direct / relayed |
|---|---|---|---|---|---|
| 64 KiB, sockets 64 KiB | 112.6 | 90.6 | 149.4 | 189.7 | 30.3 / 41.1 us |
| 256 KiB | 111.9 | 37.7 | 88.8 | 137.5 | 30.4 / 39.6 us |
| 1 MiB | 85.3 | 20.7 | 86.4 | 79.3 | 29.8 / 41.4 us |
| 4 MiB | 85.5 | 14.3 | 85.9 | 78.5 | 29.8 / 40.6 us |
| 4 MiB, sockets 256 KiB | 85.7 | 14.3 | 87.0 | 78.9 | 29.8 / 40.5 us |

On HVF the 4 MiB window holds too (guest to host direct 18 GB/s); relayed traffic flattens at
about 86 ms from 1 MiB, bound by the guest relay; host to guest at about 85 ms, bound by the
guest's sub-4 KiB receive buffers. Host socket buffers of 256 KiB measure the same as 4 MiB.

r-jupiter (2026-11-12, WHP: Windows 11 Pro build 26200, x86_64, 32 CPUs; quiet, CPU 1-9 % in the 6 s
before and after each run as `Get-Counter` read it, since the harness's load field is empty on
Windows, which has no load average; 11 rounds; abab7e2bd's base config, a 64 KiB window and the
fixed credit rule; run by kiln agent jupiter-whp-runs, its log on jupiter at `~/g121-wv-logs/4b.log`):

| arm | n | p50 | p90 | p99 | max | MiB/s at p50 |
|---|---|---|---|---|---|---|
| host->guest direct | 11 | 362.5 ms | 369.0 | 371.8 | 371.8 | 706 |
| host->guest relayed | 11 | 366.0 ms | 375.3 | 378.4 | 378.4 | 699 |
| guest->host direct | 11 | 567.8 ms | 580.0 | 598.5 | 598.5 | 451 |
| guest->host relayed | 11 | 486.8 ms | 523.9 | 534.1 | 534.1 | 526 |
| echo direct | 11 | 505.4 ms | 546.7 | 549.2 | 549.2 | 507 |
| echo relayed | 11 | 612.3 ms | 635.3 | 637.9 | 637.9 | 418 |
| 64 B round trip direct | 22,000 | 153.7 us | 175.2 | 218.5 | 567.2 | |
| 64 B round trip relayed | 22,000 | 175.4 us | 229.0 | 283.3 | 844.0 | |

At the same window and credit rule, WHP's path takes 3.0 to 3.3 times KVM's (r429) and HVF's (r433)
host to guest relayed, 2.9 times KVM's and 6.3 times HVF's guest to host direct, and 4.3 to 5.1
times HVF's 64-byte round trip. Not explained yet: the first suspect is WHP's cost per exit (each
virtqueue notify an MMIO exit handled through `WHvRunVirtualProcessor`), which
docs/research/virtio-io-exits.md's M8 asks to count per transfer. The window sweep
(`VSOCK_BENCH_CONFIGS`) wasn't run on Windows, nor the shared ring's ceiling: `dax_ring_measurement`
maps its file with mmap(2), so it skips there. The VM's CPU split is Linux's alone.

Not measured yet: aarch64 Linux KVM; on Windows, the window sweep, the shared ring's ceiling, and why
WHP's path costs 3 to 6 times the others'.
