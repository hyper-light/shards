# Engine relay throughput: current pump against the relay it replaced

ROOT measurement (R08/R14), audit finding I2 (unit 1 #2). Branch p/relay-throughput-v1 is
measurement-only and is never merged. The harness is an ignored test appended to
`crates/init/src/relay.rs` (copy: relay-with-measurement.rs). It moves 256 MiB from a
writer thread through the relay to a reader thread over identical Unix socket pairs.
- The current `pump` uses one thread and one page per direction (`Slots::minimum_mapped`)
  with `EndPolicy::EnginePeerEnd`.
- The baseline is the exact two-thread `io::copy` relay from 842460aa engine.rs.
- Rounds alternate order; each round records wall time and process CPU time (getrusage).
- This is the relay's own byte path only: no vsock, no VM, no host engine.

## r329: runner 451397, Linux 7.2.9-2-cachyos, AMD Ryzen 9 9950X3D2 (32 CPUs), test-release, main bff3015

Load average 73 before and 75.6 after: the host was busy, equally for both sides.

| relay | n | wall p50 | p90 | p99/max | CPU p50 | MiB/s at p50 |
|---|---|---|---|---|---|---|
| pump (main) | 21 | 85.3 ms | 95.6 | 101.8 | 145.3 ms | 3002 |
| copy threads (replaced) | 21 | 50.9 ms | 58.9 | 61.7 | 99.0 ms | 5025 |

The current pump is about 1.68x slower (wall time p50) and uses about 1.47x more CPU per byte
on this path. This is a measured regression of the relay itself. Whether it limits the real
path depends on vsock, which is unmeasured. A buffer-size sweep (r332) decides whether
a data-derived slot size recovers it. Readiness-driven turns, which skip the idle direction's
speculative recv, are the second candidate.

## r332: per-direction slot sweep (same harness; `Slots::reserve(n, n)`; 15 rounds, order rotated)

Load average 34–57 during the run.

| slot | wall p50 | p90 | p99/max | CPU p50 | MiB/s at p50 |
|---|---|---|---|---|---|
| copy threads (replaced) | 42.5 ms | 60.7 | 64.6 | 94.0 ms | 6023 |
| 4 KiB (main) | 81.2 | 94.8 | 100.7 | 140.8 | 3154 |
| 8 KiB | 53.5 | 65.9 | 72.4 | 96.2 | 4787 |
| 16 KiB | 34.3 | 42.8 | 52.4 | 69.5 | 7470 |
| 32 KiB | 26.6 | 31.0 | 34.7 | 54.1 | 9623 |
| 64 KiB | 20.2 | 28.3 | 42.7 | 39.5 | 12667 |
| 128 KiB | 19.0 | 26.7 | 30.5 | 38.8 | 13444 |
| 256 KiB | 22.0 | 27.5 | 27.9 | 42.8 | 11631 |
| 1 MiB | 20.0 | 31.2 | 34.9 | 39.9 | 12830 |

The regression is the one-page slot, not the single thread. Throughput rises with the slot
up to 64 KiB and is flat beyond it, within noise. The derivation is Linux 6.18
`include/linux/virtio_vsock.h:140` `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE (1024 * 64)`: the
transport splits every send into packets of at most that size
(`virtio_transport_send_pkt_info`), so one 64 KiB turn moves one whole vsock packet on
the relay's real backend leg. The fix (branch p/relay-slot-v1) adds `Slots::vsock_packet()`,
one page-rounded packet per direction, for the engine relay only. The test guest's
"minimum-mapped" mode is unchanged. Per-connection cost rises from 2 pages to 2 × 64 KiB
of mapped slot (counted, not yet profiled in a guest).

## r337: the fix against main's pump and the replaced relay (21 rounds, order rotated)

Same harness and host, 256 MiB per round. The third arm is `Slots::vsock_packet()`, the fix
on p/relay-slot-v1. Load average about 123 throughout: the host was heavily loaded, equally
for all three arms. Harness copy: `relay-with-measurement.rs` (this run's file).

| relay | n | wall p50 | p90 | p99/max | CPU p50 | MiB/s at p50 |
|---|---|---|---|---|---|---|
| pump, 4 KiB (main) | 21 | 77.3 ms | 83.7 | 89.5 | 133.3 ms | 3311 |
| pump, one vsock packet (fix) | 21 | 18.7 ms | 29.1 | 35.8 | 41.1 ms | 13722 |
| copy threads (replaced) | 21 | 42.1 ms | 52.9 | 55.1 | 88.6 ms | 6079 |

The fix is 4.1x faster than main's pump at p50 and 2.25x faster than the relay it replaced,
using less than half that relay's CPU. Its p99 (35.8 ms) is below the replaced relay's p50.

Not measured here: the relay inside a guest with a real vsock leg. On that path the VMM's
vsock device (64 KiB credit per connection, conn.rs `BUF_ALLOC`) and the guest's
receive buffers of under 4 KiB (6.18 `VIRTIO_VSOCK_DEFAULT_RX_BUF_SIZE`,
`SKB_WITH_OVERHEAD(1024 * 4)`) may bound throughput before the relay does. That
measurement is owed (docs/research/virtio-io-exits.md, open question M8).
