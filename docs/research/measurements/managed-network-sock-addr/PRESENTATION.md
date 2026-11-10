# Finite fixed-original-peer comparison — TEST ONLY

New branch `p/managed-network-presentation` starts at exact combined
`95e3b55e420199cd29a5649ad690d554dbf73942`; old sock-addr branch/checkpoints remain.
Signed source `e1bf8e03445748472448b173a93ec386af958543` implements the coordinator's
accepted interface. Policy renewed FULL before edits: canonical g121-main AGENTS,
R01–R16/evidence/CLAUDE/full plan/current handoff, revision
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`;
role implementer. Root owns all real guests/BPF, mutation/restoration, audit,
profiling/cost/matrix, evidence review and architecture/adoption/merge decisions.

## Live basis and exact interface

Root actual r492 at exact `22286c3cf385742ef31af5097a48bbf022664994` observed the
binary/empty baseline, hooked replies exposing responder endpoint/getpeername,
and strict Compatibility Mismatch. Root later reports destination-only native
cases still fail the tuple requirement while cleanup now succeeds; independent
CLONE_FS removal/restoration reproduced cleanupfalse/errno22 then cleanuptrue,
with strict compatibility still101. Output MEMBER COUNTS0/1/2 are a different
experiment and never presentation-profile successes. No new profile has run in
this implementer lane.

The controller now REQUIRES the explicit profile token:

```
sock-addr-udp4 {connected|unconnected} DEADLINE_MS {destination-only|fixed-original-peer}
```

Missing/unknown profile is typed InvalidInput, tested directly. No default,
legacy implicit selection, failed-hook fallback or product policy is added.
The fresh exec child invocation and its original endpoint are unchanged.
Host caller REQUIRES `SHARDS_SOCK_ADDR_PRESENTATION` with the same two spellings;
case/identity name includes mode and profile to distinguish retained observations.
Parent managed_network.rs has ONLY the approved adjacent usage-string correction;
its dispatch/parser/native engine is unchanged. Root r498's host `as_chunks::<2>()`
correction is preserved.

|Profile|Mode|Concrete hook plan|
|---|---|---|
|destination-only|connected|Connect4|
|destination-only|unconnected|Sendmsg4|
|fixed-original-peer|connected|Connect4, Recvmsg4, Getpeername4|
|fixed-original-peer|unconnected|Sendmsg4, Recvmsg4|

The existing emitter generates original→responder destination bytes. The SAME
emitter called with responder→original generates the presentation bytes, matching
AF_INET, SOCK_DGRAM, UDP and exact address AND port before writing address/port.
There is no map, cache, dynamic flow history, source-port relaxation, modified
backend, actor-side address repair or new relay/loader framework. This synthetic
one-tuple reverse relation is supplied by actual bound endpoint objects; it is
NOT general original-destination recovery or real socket-keyed reverse state.

## Primary contracts and alternative

All kernel source below is pinned Linux v6.18; retrieved byte identities are in
[presentation-primary-identities.json](presentation-primary-identities.json).
The separately supplied experimental6.18.48 kernel/config association and actual
support/verifier results remain coordinator evidence, not inferred from sources.

- [bpf.h UAPI](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/bpf.h):
  CONNECT4=10, UDP4_SENDMSG=14, UDP4_RECVMSG=19, INET4_GETPEERNAME=29.
  These are typed Hook variants passed to the existing private PROG_LOAD /
  LINK_CREATE path, not configurable numeric authority or selected limits.
- [filter.c access validation](https://github.com/torvalds/linux/blob/v6.18/net/core/filter.c#L8651-L8764):
  these attach types permit the existing four-byte user_family/type/protocol reads
  and four-byte user_ip4/user_port reads/writes. Family/type/protocol are read-only;
  only address and port are written. Network byte order is retained.
- Pinned [connect4](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/connect4_prog.c),
  [recvmsg4](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/recvmsg4_prog.c),
  [getpeername4](https://github.com/torvalds/linux/blob/v6.18/tools/testing/selftests/bpf/progs/getpeername4_prog.c)
  selftests exercise the distinct context writes. Their unrelated helpers,
  priorities, source binds, fixed ports and resource settings are not copied.
- [udp.c1995–2013](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/udp.c#L1995-L2013):
  receive-peer sockaddr presentation is distinct from later original-destination
  ancillary construction; no cmsg is synthesized from our configured original.
  Kernel filtering occurs before this presentation and needs separate real proof.
- [af_inet.c750–781](https://github.com/torvalds/linux/blob/v6.18/net/ipv4/af_inet.c#L750-L781):
  getpeername uses the stored remote tuple then its presentation hook; unconnected
  ENOTCONN precedes that hook. No unused Getpeername4 is installed for unconnected.
- Established [Cilium v1.18.0 bpf_sock.c504–578](https://github.com/cilium/cilium/blob/v1.18.0/bpf/bpf_sock.c#L504-L578)
  uses socket-cookie-plus-endpoint keyed reverse state in recvmsg/getpeername.
  That real-system behavior is NOT supplied by our fixed known tuple. No Cilium
  map/default/capacity architecture is adopted or approximated by a new cache.

## Ownership, errors and observations

One controller-owned list is fallibly reserved to the exact selected plan length
BEFORE loads. Every successfully attached named, unpinned link is inserted before
reporting can fail. HookSetup transitions NotStarted→Partial→Ready only as actual
attach operations complete; no intercepted child starts before Ready. Partial
failure retains acquired links through the same child reap / both worker joins.
The existing mount cleanup and outer VM/work-deadline ownership remain unchanged.

Private HookFailure carries Hook + Emit/LoadProgram/AttachLink/DetachLink stage +
actual io::Error cause (kind/errno). Each acquired link's detach is attempted and
reported even after another detach/report fails. The existing outer io::Result
presentation bridge remains; the first cause returns nonzero after recording the
named facts. Report-write/alloc/OS-fault behavior is not certified by pure tests.
No failure becomes a direct-routing fallback or successful native compatibility.

Target records selected profile. Each Program record names Hook/from/to and actual
bytecode; LinkAttached / LinkDetached name actual operations. HookError preserves
exact stage/kind/errno. Cleanup includes HookSetup, required/acquired counts and
actual aggregate outcomes. `link_detached` describes acquired links; it alone does
NOT establish that the required plan attached. Host success additionally requires
Ready, exact named plan records, both actual worker completions and strict native
observations. Missing/partial/error/extra facts cannot pass by a profile label.

Client getpeername, per-datagram length/flags/copied bytes/metadata, Binary/Empty
state and original-IP-and-port checks are unchanged. A mismatch still accumulates
through both exchanges where I/O permits and finally FAILS. Worker algorithm is
unchanged. These are responders, not a full forwarding relay or default-deny layer.

## Exact source identities and light evidence

|File|SHA256 at source checkpoint|
|---|---|
|intercept/sock_addr.rs|7c83dc31708476f93c727190e7dd7b7b6d0ccf412c4b6ae584051a4b1a7974c9|
|intercept/sock_addr/bpf.rs|9aad0283c896f1a192f204a99537fd7804dffe30d25a1227560e8fc201ffde01|
|host tests/managed_network.rs|52e791204edef56f9e657f31cf32e3364218cf04340c7c508fa24f5c52f77e5c|
|parent managed_network.rs|803b8cdc80557a54db859f052b94e08ddec7b0de1aeea458080fc0909247f5c2|

Byte-identical blocks versus95e3: client observations/strict checks SHA
`edc1da5be6f40d26970dbef033287cecefd0c8797a5e879f2a04d25656934fa9`;
fresh child ownership/invocation SHA
`9d574b1cb0597593ea0f60517a7a5bba3ad5bd82289b27a4459aedc982832ee8`;
mount cleanup SHA `b40b9a3e1bf7542441972a0bfb34877429584873b6bff8ece83e32e17d817213`;
existing emitter SHA `efd3dd500adc412bf0d48beec2037b7eceac58ed7ded23a0d048a2f0b71982c8`.

Actual private Fedora `8fadc1ec213332e16cb0d915a3fb913f`, admitted4CPU/1GiB/2GiB,
pinned rustc1.98.1, NEW presentation-proof-v1/v2 and presentation-host-v1 targets:

- r140 before adding the explicit typed-profile-error assertion:25 GNU and25 musl
  pure tests0; GNU/musl scoped panic/unsafe lint subsets0.
- r141 final source:26 GNU and26 musl pure tests0; both scoped lint subsets0.
  Extracted actual host caller source compiles0; exactly one record-parser test0
  plus exactly one profile-parser test0 (separate exact filters).
- Policy check, diff check and owned rustfmt check0. No local compiler; Mac
  36879772KiB before worktree creation. No own active jobs remain.

Replay light commands (NEW directories):

```sh
python3 docs/research/measurements/managed-network-sock-addr/light.py "$HOME/managed-network-presentation-proof-NEW"
python3 docs/research/measurements/managed-network-sock-addr/light-host.py "$HOME/managed-network-presentation-host-NEW"
```

Pure cases cover all four finite plans, missing/unknown profile InvalidInput,
reverse endpoint/port encodings, hook IDs and typed error records. They execute no
fixture client/socket/namespace/cgroup/BPF/load/link/detach path. Host extraction
uses explicitly inert typed common stubs, not actual common VM implementation.
Full crate/real host+guest build, seven targets and runtime remain root obligations.

[presentation-light/transfer.json](presentation-light/transfer.json): archive8384B,
SHA256 `b76e0409f00df9036a30980abe893b3921c2d1f78c642d297e199cfbf8bdbbff`,46 regular
entries/45 checked hashes, no executable bytes. Source manifests match actual
final sock_addr/bpf/host bytes. Original copied sources/binaries/private targets
remain retained remotely; new source-copy mtimes avoid sync/artifact staleness.

## Coordinator replay and meaningful mutations — ALL OWED

With actual source-matching normal CLI/helpers/guest and fresh records per case:

```sh
SHARDS_SOCK_ADDR_MODE=connected SHARDS_SOCK_ADDR_PRESENTATION=fixed-original-peer \
SHARDS_KERNEL=/absolute/experimental/vmlinux SHARDS_TEST_KERNEL=/absolute/experimental/vmlinux \
SHARDS_NETWORK_EFFECTIVE_CONFIG=/absolute/experimental/config \
SHARDS_NETWORK_CONFIG_PROVENANCE='exact source/build/config association evidence' \
SHARDS_NETWORK_RECORDS=/absolute/fresh/records \
SHARDS_NETWORK_BOOT_MS="$CALLER_BOOT_MS" SHARDS_NETWORK_CASE_MS="$CALLER_CASE_MS" \
SHARDS_NETWORK_MEMORY_MIB="$CALLER_MEMORY_MIB" SHARDS_NETWORK_CPUS="$CALLER_CPUS" \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  direct_guest_socket_address_udp4_requires_original_native_tuple -- --ignored --exact --nocapture
```

Run all FOUR profile×mode combinations. SHARDS_KERNEL is the direct fixture input;
SHARDS_TEST_KERNEL independently selects common test-kernel helpers if used.
No production pin/config is changed. The earlier exact22286c3 direct argv was
`sock-addr-udp4 connected DEADLINE_MS` (or unconnected); current original-mode replay
MUST explicitly append `destination-only`, and host replay sets that profile.
Its native mismatch remains strict101 if observed, not a new passing contract.
Raw records precede host assertions; diagnostic-driver0 is never compatibility0.

Root's independent removals must establish actual effects, not just missing records:

|Remove ONLY|Relevant fixed profile case|Actual witness required, not yet observed|
|---|---|---|
|Getpeername4|connected|Actual peer_addr exposes responder endpoint; both receive observations retained; strict mismatch|
|Recvmsg4|connected|getpeername remains original, actual receive peer exposes responder; strict mismatch|
|Recvmsg4|unconnected|Actual receive peer exposes responder, ENOTCONN remains; strict mismatch|
|Connect4|connected|Second client actually reaches backend: BACKEND bytes, extra actual backend Binary/Empty receipts, responder lacks script completion|
|Sendmsg4|unconnected|Same direct-backend byte/receipt effect for fresh send_to client|

Restore exact source bytes and observe restored profile outcome after each. No
Getpeername4-necessity claim for unconnected, where it is deliberately absent.
Root also owns actual partial load/attach/detach/report failures, complete child/
worker/link/cgroup resource return, relevant syscall/filter/cmsg semantics,
contention, fault/matrix/cost and architecture choice. No result is predicted as
passing from documentation or pure encoding tests.

## R01–R16 accounting

|Rule|This scoped piece / remaining gap|
|---|---|
|R01|Checked/fallible exact-plan reservation; no vector growth beyond plan; typed syscall errors and full detach loop. Scoped panic/unsafe lint0; real allocation/OS/report-fault proof owed.|
|R02|Pinned primary contracts, Cilium alternative, root actual tuple red; no product decision from this finite input.|
|R03|Hook IDs/word widths are UAPI; list length derives from selected real plan; caller deadlines and OS ports unchanged. No new cap/default.|
|R04|Explicit profile/Hook/HookStage/HookSetup; named per-hook actual facts; incomplete/unknown/error stays nonzero.|
|R05|Same emitter/loader/worker/client seams; finite owned link resources only, no generic manager/maps/cache/polling.|
|R06|Two explicit profiles/four real hook variants, one setup and teardown path; no new engine.|
|R07|Comments preserve lifetime/unsafe/ABI rationale; linked primary sources and exact diffs.|
|R08|No performance result;4–5x not measured or achieved.|
|R09|Root actual native tuple mismatch is live basis. New profile runtime is unrun, not claimed fixed.|
|R10|Signed source, durable light logs/commands/manifests and exact owed four-case/removal/restoration commands.|
|R11|Light GNU/musl only; full seven-target and actual hosts/VMs owed. Windows ARM stays paused.|
|R12|Existing Insn/ABI seam reused; no installed binary/hostroot/crypto/production-platform changes.|
|R13|One finite fixed synthetic relation; broader state/forwarding/grants deliberately not invented.|
|R14|All profiling/benchmark/resource comparisons root-owned and unrun here.|
|R15|Implementer source/pure/light only; no actual fixtures/namespaces/BPF/guests/mutations/audits/heavy runs, child agents or merge.|
|R16|Private new worktree/targets only; Mac floor maintained/lockf formatting, no local build/cache/evidence deletion/rootcache writes.|

Default deny, arbitrary native socket/filter/cmsg semantics, inherited/transferred
FD authority, budgets, multi-destination/IPv6/other routes, fragmentation/PMTU/QUIC,
full forwarding relay, fail-closed lifecycle, real cost and complete P4 selection
remain OPEN. Passing this finite case, if later observed, cannot close those gaps.
