# vsock window: a device-wide lent window, host socket buffers, the engine relay's buffer

Implementer for ROOT coordinator 01a148884a4770fdaf0111f3f792a248, 2026-11-11. POLICY_READ: origin/main
bff3015182ad754274f1048ce393ac12cb6cd91f, docs/development-rules.md sha256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, role implementer ([POLICY_READ.json](POLICY_READ.json)).
Branch p/vsock-window-v1 on p/vsock-credit-v1 1e448c7e. Owned: crates/vmm/src/devices/virtio/vsock/**,
crates/init/src/engine.rs, crates/abi/src/** (the window constant), this directory. Light tests on runner
451397 (adacachy: Linux 7.2.9-2-cachyos, x86_64, rustc 1.98.1). The measurement section is ROOT's
(r421, r429, r433 and its tables).

## Implementation

**The window** (conn.rs). Every packet to the guest carries our window (`buf_alloc`):

- `WINDOW_LEAST` = 64 KiB: what every connection had before (Firecracker's CONN_TX_BUF_SIZE; the old
  `BUF_ALLOC`), one packet of the largest size a driver sends (VIRTIO_VSOCK_MAX_PKT_BUF_SIZE). Every
  connection always has it, so each still makes progress and still has the old per-connection bound.
- `WINDOW_MOST` = `shards_abi::VSOCK_WINDOW` = 4 MiB, the knee of guest-to-host throughput over windows:
  on KVM 194.8 / 87.5 / 55.5 / 52.6 / 57.7 ms for 256 MiB through 64 KiB / 256 KiB / 1 MiB / 4 MiB /
  16 MiB (r429); on HVF 37.7 / 20.7 / 14.3 ms through 256 KiB / 1 MiB / 4 MiB (r433). In crates/abi
  because the guest's engine relay asks its socket for the same size, and init cannot depend on the vmm crate.
- `WINDOW_LENT` = MOST − LEAST, lent by the device and shared by all its connections (ROOT's derivation:
  one stream's whole window at the knee, since one worker moves every connection's packets).

`header()` computes each packet's window:

    asked  = WINDOW_MOST if the guest has used half its window since it last heard (guest_needs_credit),
             else the window it has
    window = max(min(asked, guest's buf_alloc, LEAST + min(spare, LENT)), LEAST, exposure)
    granted_until = fwd_cnt + window

- **The window grows only on use.** A connection whose guest sends little is never lent anything, however
  much is spare. Its window grows once the guest has used half of what it has, which is the half-window
  rule that already decides when a credit update is owed.
- **No more than the guest's own buffer.** Linux sends no more unacknowledged than its own buffer
  (8ee784fdf006), so a larger window is never used, only held.
- **The right edge never moves left** (`max(.., exposure)`, RFC 1122 §4.2.2.16). A later window may be
  smaller, but `fwd_cnt + buf_alloc` never decreases. So `granted_until` is monotone by construction,
  and ROOT's RFC 1982 comparison is gone.
- `spare` is the muxer's `WINDOW_LENT − Σ others' lent`. On a failed conversion it is now 0 (fail
  closed); ROOT's start used `WINDOW_LENT` (fail open). The conversion cannot fail.

**The muxer's accounting** (ROOT's, kept). `lent` is maintained around every call that moves
`fwd_cnt` or `granted_until`:

- `on_guest_packet` (data, and SHUTDOWN's flush);
- `on_events`' `on_writable`;
- `next_rx` (the only place lent grows, bounded by `spare`);
- `remove`, `reset` and `restore`.

`expire`/`kill` move neither counter. A Windows lingering connection is out of the accounting from
`remove` on. It now also drops its unsent guest bytes (`linger()` clears its buffer), which it can never
send because its write side is shut. Before, a lingering connection could hold up to 4 MiB outside the
budget.

**The half-window rule after a buffer shrinks** (found by `Conn::check`; inherited from the base).
`on_guest_packet` now re-checks `guest_needs_credit()` after every packet, not only after data. Every
packet carries the guest's `buf_alloc`. A guest whose program lowers `SO_VM_SOCKETS_BUFFER_SIZE` once
half of its new, smaller window is already forwarded is capped at that buffer (8ee784fdf006). It sends
nothing more, so nothing later would make the update owed, and it waited for ever. Unit-level red below;
the real-VM symptom is not reproduced (owed).

**TxBuf** is a ring (`VecDeque`).

- What the host takes leaves the rest in place. ROOT's amortised compaction still moved the whole live
  buffer on every append while room was short. With a stalled consumer and a 4 MiB window that is up to
  4 MiB moved per 64 KiB packet; its comment's bound did not hold.
- Growth is capped at the connection's exposure (what the guest may still have to buffer), which
  restores the base's tightness. ROOT's start capped at the static WINDOW_MOST.
- After a flush, `fit()` gives back room past twice the exposure (to the exposure; never below
  WINDOW_LEAST, and only where the allocation succeeds). Without it, a buffer grown under a lent window
  kept its room after the window was lent elsewhere, so the device-wide bound did not hold. The factor 2
  is the hysteresis that makes each move cost fewer bytes than were forwarded since the room was made.

Bounds, held by `Conn::check` and `Muxer::check` in every test step:

- per connection: buffered ≤ exposure ≤ WINDOW_MOST, room ≤ max(WINDOW_LEAST, 2 × exposure);
- per device: Σ lent ≤ WINDOW_LENT.

So buffered bytes are at most N·LEAST + LENT and room at most 2·(N·LEAST + LENT). A connection that was
never lent keeps room ≤ LEAST, as before.

**Host socket buffers** (muxer.rs). `SOCKET_BUFFER` = 256 KiB each way (SO_SNDBUF, SO_RCVBUF). This is
ROOT's r433 derivation: with a 4 MiB window, 256 KiB measured the same as 4 MiB (guest→host 14.3/14.3 ms,
relayed 87.0/85.9, host→guest 85.7/85.5). It applies to:

- both ends of `serve()`'s pair (ours in `guest_connect`, theirs, the daemon's, in `serve`);
- `guest_connect`'s dialled socket;
- `take()`'s accepted socket.

A refusal or a cut size is logged at debug and the socket kept. **Not on Linux** (`SIZES_SOCKETS`): the
confined VM process's seccomp filter (crates/shards/src/confine.rs, not owned here) allows no
`setsockopt`. A refused syscall ends the process (SIGSYS handler, `shards_syscall_filter::name_refusals`),
so every Linux VM would have died at its first vsock connection, which unit tests (unconfined) cannot
see. Linux's own default, wmem_default 212,992, is about the size anyway. To size on Linux too, ROOT
would allow `setsockopt`/`getsockopt` with optname SO_SNDBUF/SO_RCVBUF in confine.rs and flip the
constant. These buffers are per socket, outside the shared budget: up to N × 256 KiB of kernel memory on
macOS where macOS gave 8 KiB.

**The engine relay** (init engine.rs). Right after dialling, `ask_window` sets
SO_VM_SOCKETS_BUFFER_MAX_SIZE (2), then SO_VM_SOCKETS_BUFFER_SIZE (0), as u64 values at level AF_VSOCK
(include/uapi/linux/vm_sockets.h), to `VSOCK_WINDOW`. The maximum goes first because
`vsock_update_buffer_size` clamps the size to it, and Linux tells the host at once
(`virtio_transport_notify_buffer_size`). On failure the relay says so on stderr once per relay process
(every connection's socket is the same kernel's) and keeps the connection. The change is local to
`relay()`, for the p/audit-fix-init-v1 merge. The guest then buffers up to 4 MiB of host data per engine
connection, its own memory.

## Validation (R09/R10)

| step | command / record | result |
|---|---|---|
| red, base | `bash red/run.sh . ~/.cache/vsock-window-v1-red` ([red/red.log](red/red.log)) | lone connection offered 65,536, not 4,194,304 |
| red, ROOT's start | same run, [root-start.patch](root-start.patch) on the base | 7/7 red: RESPONSE lends 4 MiB (`(2, 4194304, 4128768)`); window past a 256 KiB guest buffer; right edge moved left (1,146,880 for 4,227,072); shrunk buffer owed no update (`None`); second connection's sharing; busy device: "port 50076 waits for credit" |
| green | [green.log](green.log) | 40/40 vsock unit tests; shards-init 146 passed (1 ignored measurement); relay test read both options back as 4 MiB on a real AF_VSOCK socket |
| busy device | 4000 seeded steps | lent at most 4,128,768 of 4,128,768; 293 windows grown, 180 shrunk with the edge kept; 44 guests sent past their windows, all reset; every other host read its guest's bytes in order |
| host sockets | | Linux asked 262,144: [212992, 212992] became [524288, 524288] (doubled; wmem_max 4 MiB here) |
| mutants | `python3 -I mutants/run.py . ~/.cache/vsock-window-v1-mutants` ([mutants/mutants.log](mutants/mutants.log)) | 14/14 killed, then restored 40/40 and 1/1 |
| gates | [gates.log](gates.log) | fmt; development policy; clippy -D warnings: shards-vmm (all targets, x86_64 gnu), shards-abi, shards-init (all targets; x86_64 gnu, x86_64 musl, aarch64 musl, zig 0.16.0) |

`red/overlay.py` copies the committed test text onto the older trees. It adds a check that always holds,
and on the base the two window constants; their production code is unchanged.

The mutants, each removing one guard:

1. room clamp in `header()`
2. spare subtraction in `next_rx`
3. right-edge `max(exposure)`
4. guest-buffer cap
5. growth only on use
6. update after a buffer shrinks
7. `send_host`'s exposure check
8. lent recount in `on_events`
9. TxBuf growth cap
10. `fit()`
11. ring order across the wrap
12. setsockopt
13. no sizing on Linux
14. the relay's maximum first

In the first mutant run, M13 survived: the test asserted on `SIZES_SOCKETS` itself. It now pins the
platform (`cfg!(target_os = "linux")`), and the second run killed all 14.

## Owed to ROOT

- macOS (HVF) and Windows (WHP) clippy and unit tests: this runner has no std for those targets. The
  macOS socket sizing, Windows `set_buffer`/`buffer_size`, and the Windows-only test
  `a_lingering_connection_holds_none_of_what_its_host_did_not_take` are not compiled here.
- aarch64 gnu clippy; the 7-target lint at combined validation.
- Real-VM E2E on KVM, HVF and WHP: the relay's buffer seen by the host; the shrunk-buffer stall
  reproduced live (R09: unit-level red only); the measurement section.
- The D-entry for `VSOCK_WINDOW`, `WINDOW_LEAST/LENT`, `SOCKET_BUFFER` and fit's factor
  (docs/design/architecture.md, not owned here).
- The Linux socket-sizing decision (confine.rs).

## R01–R16 (implementer's part)

- **R01**: production adds no unwrap, indexing or unchecked arithmetic: `checked_add` on guest
  addresses, saturating counts, `get_mut` slices, a write count clamped before `drain`. Allocation goes
  through `try_reserve_exact`, and a failed `fit` keeps the old buffer. Checks are `#[cfg(test)]`.
  Clippy is clean.
- **R02/R03**:
  - Sources: Linux 7.2-rc4 tree (8ee784fdf006, 3ef3d52a1a98, af_vsock.c), RFC 1122 §4.2.2.16,
    virtio 1.3 §5.10.6.3, XNU uipc_usrreq.c via ROOT.
  - Each literal's derivation is above.
  - Gaps: WINDOW_MOST and SOCKET_BUFFER rest on ROOT's r421/r429/r433, single hosts per hypervisor.
    WHP and aarch64 KVM are unmeasured. The D-entry is owed.
- **R04**: no new state machine. The window is a function of demand, room, the guest's buffer and
  exposure.
- **R05**: no cache. One counter, owned by the muxer.
- **R06/R13**: `header()` is about ten lines. TxBuf is three methods.
- **R07**: the comments carry sources and invariants.
- **R08/R14**: no performance claim. Measurement is ROOT's.
- **R09/R10**: red, green, mutation and logs as above. One real-VM symptom is owed.
- **R11/R12**: Linux x86_64 only here; the others are owed. Sizing is skipped on Linux for confinement,
  with the alternative named.
- **R15**: light tests only; no E2E, profiling or merge.
- **R16**: the scratch trees under ~/.cache/vsock-window-v1-{red,mutants,evidence} on the runner were
  removed after use.

## Limits

- A grant cannot be taken back (virtio vsock has no revocation). A connection whose guest stops after a
  burst keeps the unused part of its last window, at most its guest buffer's worth, until it sends again
  or ends.
- Lending is first-come, with no fair-share policy.
- Guest kernels without 8ee784fdf006 use our whole window. The buffer cap limits them to their own
  buffer (256 KiB by default) unless their program raises it.
- 3ef3d52a1a98 (the unsigned credit) is in v6.19; whether stable 6.18.48 carries a backport was not
  checked. Since our right edge no longer moves left, it no longer matters.
