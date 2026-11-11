# vsock credit: a guest that may send less than our window

ROOT coordinator 01a148884a4770fdaf0111f3f792a248, 2026-11-11. POLICY_READ main bff30151, rules
df2acf47. Found while measuring the engine path (measurement-only branch p/vsock-path-v1).

## Finding

Linux caps a vsock socket's TX credit at the lesser of its peer's advertised buffer and its own
(`virtio_transport_tx_buf_size`, net/vmw_vsock/virtio_transport_common.c, upstream 8ee784fdf006
"vsock/virtio: cap TX credit to local buffer size", 2026-01-21; present in the pinned 6.18.48,
checked against kernel.org stable v6.18.48). A guest socket therefore stops after sending its own
`SO_VM_SOCKETS_BUFFER_SIZE` (default 256 KiB) unacknowledged, whatever window we advertise.

Our device (conn.rs) sent a credit update only once the guest's view of our free space fell below
4 KiB (Firecracker's CONN_CREDIT_UPDATE_THRESHOLD), i.e. after forwarding about 60 KiB of its
64 KiB window. A guest socket whose buffer is below that never hears it and waits for ever. With a
larger window the same rule deadlocks every default guest socket at 256 KiB.

Evidence, runner 451397 (Linux 7.2.9, KVM, x86_64):
- r399/r406: with a measurement window of 1 MiB, guest-to-host streams stopped at 262,144 bytes;
  256 KiB windows streamed.
- r414: the guest held 0 bytes unsent (SIOCOUTQ) while blocked writing: it waited for credit.
- header trace: our REQUEST advertised buf_alloc 1048576; the guest answered with its own 262144
  and sent four 64 KiB packets, then nothing; we owed no update by the old rule.

Separately, a guest's CREDIT_REQUEST went unanswered unless the threshold also held, where virtio
1.3 csd01 §5.10.6.3 says "the reply to this query is a VIRTIO_VSOCK_OP_CREDIT_UPDATE packet".
Linux 6.18 guests do not send CREDIT_REQUEST; the deviation was latent.

## Fix

- `guest_window()`: the lesser of our window and the guest's advertised `buf_alloc` (from its
  every header).
- `guest_needs_credit()`: once forwarded-but-unannounced bytes reach half of that window: a
  receiver's window update at Fr = 1/2 of its buffer (RFC 1122 §4.2.3.3). The guest is credited
  before it runs out, has the other half to send while the update travels, and hears at most twice
  a window. No new constant; the 4 KiB threshold is removed.
- A guest's CREDIT_REQUEST sets its own pending answer, always sent.
- No snapshot format change (pending state is not saved). BUF_ALLOC is unchanged (64 KiB).

## Validation (R09/R10)

| step | run | result |
|---|---|---|
| red, unit | r416 | `a_guest_buffer_below_the_window_is_credited_before_it_is_used_up`: left None, right Some((6, 16384)); `a_credit_request_is_answered`: left None, right Some((6, 65536)) |
| red, real VM | rerun of r416 | `a_guest_socket_with_less_buffer_than_the_window_streams_whole` (guest socket buffer 16 KiB, 1 MiB stream): "stalled with 1032192 of 1048576 bytes left"; guest watchdog: unsent 0 |
| green | r423, final | 31/31 vsock unit tests; 5/5 vsock real-VM tests |
| mutant 1: window ignores the guest's buffer | r423 | unit red (None); real VM stalls at 16,384 bytes again |
| mutant 2: a credit request waits on the threshold | r423 | `a_credit_request_is_answered` red |
| restored | r423 | conn.rs a0a7a707 identical; 8/8 conn tests |
| lint | final | clippy -D warnings: shards-vmm all targets (gnu), shards test vsock, shards-testguest all targets on x86_64 gnu/musl and aarch64 musl; fmt; policy |

The existing test that pinned the 4 KiB threshold now pins the half-window rule (an update owed at
exactly 32,768 bytes forwarded, none a byte before).

Owed: the vsock real-VM tests on macOS (HVF) and Windows (WHP); the 7-target lint at combined
validation. Performance: not claimed here; window size is measured on p/vsock-path-v1.
