# Shared production extraction in progress

First signed core: `6ab0a06f3594b172f2c667a3dc6f5458562fc7bd`. The
shared-source/parser/native follow-up now has the eight actual TCP passes in
[NATIVE.md](NATIVE.md). Production `engine.rs` wiring now has a
[compiled caller checkpoint](engine-consumer/README.md): normal static-musl
guest build and strict init Clippy pass. Unix/VSOCK runtime compatibility
remains unverified on that candidate.
Existing first-checkpoint passes do not cover later changes. Current policy
cbd392/df2acf applies; renewed readback accepted
by ROOT at6120. First actual code checkpoint sent at6127, extraction interface
at6152. No children or goal/planning wrappers.

## Actual callers and source decision

`Standby::start` → `engine::start/serve/relay` is the production Unix↔VSOCK
consumer. TCP's fixture Worker is the second actual native-stream consumer.
`crates/init/src/relay.rs` is now the only new pump implementation; testguest
imports that same file. It avoids importing the host net packet stack, its
sequence/retransmit state and retained ring cache. The existing Deadline body
is moved there with visibility changes; its original absolute time, per-attempt
pending cancellation readiness and expiry-before-cancel semantics remain.

Current candidate API: `pump([BorrowedFd;2], Slots, [ByteLimit;2], Control,
EndPolicy, &mut Report) -> Result<Termination,Fault>`. Fixed storage and retained
native prefixes have one owner. `Control::Until` is the TCP fixture's original
Deadline with a real owned cancel FD; `Control::ProcessOwned` explicitly names
the existing engine's process/peer lifetime. The latter supplies no invented
absolute deadline or unproduced cancellation FD and does not claim per-flow
owner cancellation beyond the current engine contract.

TCP uses independent halves and finite caller byte limits. The current engine
copy ends its Unix side when host copying ends; the candidate EndPolicy names
that difference and distinguishes actual source EOF from peer-driven stop.
**Actual old Unix/VSOCK half-close observations are still required** to preserve
or explicitly evolve that behavior. The compiled wiring candidate is not an
assertion of measured equivalence or approved route coverage. HELLO/header/token
bytes, token verification and grants are unchanged. No authentication material
is logged.

The engine reservation candidate uses one minimum private mapping unit per
direction, from Linux `sysconf(_SC_PAGESIZE)`, allocated fallibly before dial/
HELLO. This is a physical minimum for those mappings, not a throughput-derived
capacity, copied std default, learned authority, aggregate hard-memory admission
or performance claim. It adds mapping/unmapping/fault costs that ROOT must
profile. TCP's caller-sized heap slots remain fixed and do not grow. A missing
physical-size fact or failed reservation refuses that owned attempt; no guessed
fallback size, cache or host setting is introduced. Hard parent/kernel/tenant
budget attribution remains required work.

## Primary source identities

| Primary source | Exact identity / inspected behavior |
| --- | --- |
| [Linux Unix sockets](https://github.com/gregkh/linux/blob/v6.18.48/net/unix/af_unix.c) | Git blob52b17331e9d5124c52f108c2fc25c89ea2aa47d4. `unix_shutdown`, `unix_poll`, native stream send/read. Read/write shutdowns propagate separate peer states. |
| [Linux VSOCK](https://github.com/gregkh/linux/blob/v6.18.48/net/vmw_vsock/af_vsock.c) | Git blobd007ce89d19b8e77c263332e574d18e365c38902. `vsock_shutdown`, `vsock_poll_shutdown`, stream send/receive. Readiness is not an EOF or byte receipt. |
| [Rust copy dispatch](https://github.com/rust-lang/rust/blob/1.98.1/library/std/src/io/copy.rs) | Git blob43c6f71357e61c442ebb803d7bc5091282da50e7. Linux first attempts kernel copy, with userspace fallback; no claim every old engine copy allocated the fallback buffer. |
| [Rust Linux kernel copy](https://github.com/rust-lang/rust/blob/1.98.1/library/std/src/sys/io/kernel_copy/linux.rs) | Git blob1c00d317f2a5239682cc85cc4793768451068e50. Actual file/socket properties select possible kernel paths. |
| [Rust default size](https://github.com/rust-lang/rust/blob/1.98.1/library/std/src/sys/io/mod.rs) | Git blob0158137174087a6db074496c3dad2639680843f0. The upstream fallback size is not adopted as a Shards production allowance. |
| [VSOCK API](https://man7.org/linux/man-pages/man7/vsock.7.html) | Native byte-stream send/recv contract; actual hypervisor support remains a runtime question. |
| [Mapping API](https://man7.org/linux/man-pages/man2/mmap.2.html) | Anonymous private page-based mapping contract. No new kernel configuration or product kernel pin. |

These are source observations/design inputs, not a reproduced runtime defect or
a passing compatibility test. No optimization or universal panic-free, leak-free,
allocation-free or 4–5× claim follows.

## Active/pending actual work

Private Fedora baseline clone/target/cargo-home:
`/home/adalundhe/relay-tcp-forward-cbd3-v1`. Actual preflight0: uid1000,
Linux6.19.10/Fedora44, existing /dev/kvm RW, 1,928,331,837,440 bytes disk free.
It retains exact signed6ab source with unchanged old engine relay. Run r39
completed the full actual engine host/helper build0 under admitted4CPU/8GiB/20GiB,
1800s build timeout/2100s outer. Its unchanged engine source is SHA256
`e2e4e6e6ae014ddb82aba96362801bd14bf4a69a2955ac44941bd481f6563eea`.
`record/engine-before-build-{input,result}.json` and `record/engine-before-build.log`
retain the invocation and outcome. This is a build result, not a native test pass.
Keep that source/target as the old-engine baseline until runtime artifacts and
observations have been retained.

The first light-v3 submission was not admitted (0idle CPU). A subsequent fresh
submission was admitted as r41, private1CPU/1GiB/2GiB. Policy0; GNU and musl
compilation failed on the new frontend/parser visibility boundary. Both library
and cfg(test) logs are retained in `~/relay-tcp-forward-cbd3-light-v3`; the parser
visibility and test-only `Instant` import are corrected locally. Further cfg(test)
diagnostics and the corrected source require a passing compilation before any
native outcome can be reported. No native socket case executed from r41.

Corrected-source submissions on Fedora and adacachy were temporarily
denied0idle CPU. They did not run; no headroom bypass occurred. Later fresh
admission ran light-v4/v5 and the eight-case native scope as NATIVE records.
`native_cases.py` launches one exact ignored source-linked Rust
test per process, holding the unreaped child's pidfd before its private release,
and retaining actual backend/client bytes, both half-close facts, exit readiness,
reaping, parent FD return and exact before/after binary hashes. Child FD return is
asserted in that same native test; no kernel-cost return claim is made.

First native cases execute the actual shared pump
on owned ordinary TCP sockets, with finite supplied byte/time envelopes, a
private release FD, held parent pidfd and independent backend/client bytes.
Production static musl is primary; GNU comparison cannot waive a differing guest
syscall/security outcome or justify an allowlist change.

Then run actual KVM interception/both half-closes, partial/backpressure,
reset/cancel/prefix and independent bypass/corruption mutations with exact
restoration, followed by the shared runtime's actual Unix/VSOCK compatibility.
The first eight native TCP cases passed; KVM and runtime compatibility remain
pending. All routing classes beyond
the bounded experiment and hard resource/restore requirements remain open.
ROOT owns audits, profiling, benchmarking, final integration/matrix and main.
