# Reproduced held-mount entry failure: fixture-only CLONE_FS preparation

Policy renewed before edits: FULL canonical g121-main AGENTS, R01–R16, evidence,
CLAUDE, managed plan and current handoff. Revision
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`; rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Role implementer. Own only sock_addr.rs held-namespace branch and focused evidence.
Root owns actual guests, BPF, mutations/restoration, audits/profiling/cost and merges.

## Actual reproduced basis

Root reports real r492 at exact `22286c3cf385742ef31af5097a48bbf022664994`:
connected and unconnected KVM cases both ran the exact binary/empty baseline;
hooked replies arrived with responder endpoint/getpeername presentation and
correctly recorded Compatibility Mismatch. Both children were reaped, workers
joined, and link detached. RemoveCgroup failed errno22; cgroup_removed=false.
This is a native compatibility RED, not successful mediation or cleanup.

Root r493 then instrumented the actual mount operations at that same source:
UnshareNewMount returned0, then BOTH SetnsHeldMount attempts returned-1/errno22.
Original source was restored in finally with SHA256
`0184eab9bfa8ecc629a6170ba1418fe7623ff948e4ec1805879546c74d095939` and empty diff.
Root records are retained in Fedora `~/root-managed-sockaddr-setns-step-v1`.
These are coordinator-supplied real observations; this implementer neither ran nor
instrumented a guest. The earlier source suspicion stayed unchanged until these
actual operation facts authorized this fix. Raw coordinator packaging/review is
root-owned; the new source has no actual after-result yet.

## Primary precondition and minimal change

- [setns(2), mount namespaces](https://man7.org/linux/man-pages/man2/setns.2.html):
  entering a mount namespace requires filesystem-related attributes not be shared
  with another process/thread through CLONE_FS. Capability requirements remain;
  this change does not bypass them.
- [Linux v6.18 fs/namespace.c, mntns_install](https://github.com/torvalds/linux/blob/v6.18/fs/namespace.c#L5894-L5934):
  after capability/namespace checks, a filesystem structure with users other than
  one returns EINVAL. This protects root and working-directory updates.
- [Linux v6.18 kernel/nsproxy.c, prepare_nsset](https://github.com/torvalds/linux/blob/v6.18/kernel/nsproxy.c#L302-L325):
  the single CLONE_NEWNS setns case uses the current task's fs structure; the
  temporary copy is for a different, multi-namespace case. Our held-FD call is
  the single-mount case, so a new thread alone does not satisfy that requirement.

Retrieved primary document byte identities are recorded in
[setns-primary-identities.json](setns-primary-identities.json). The kernel source
is pinned v6.18, not an unversioned latest tree; the manpage is the exact current
retrieval identified there. The experimental guest kernel identity remains the
coordinator's separately recorded6.18.48 build, not a production pin change.

Signed source `a788c0e9ea8ff76d22738d767bfd1e9131c61ddd` changes only the held
namespace `Some(ns)` branch of fixture-private in_mount: the dedicated new thread
calls `unshare(CLONE_FS)`, returns the actual io::Error on failure, then calls the
existing setns. The existing common rc error path handles setns failure as before.
The caller/PID1 keeps its original filesystem attributes and mount namespace;
only the new worker thread changes its own attributes before entering the held
fixture namespace. The `None` new-namespace branch is byte-identical.

Creating a new process/manager or requesting extra namespaces merely to obtain a
private filesystem structure would broaden this fixture and was rejected. A
retry/fallback without the precondition would preserve the actual failure or
weaken ownership. No new abstraction, polling, cache, timeout/cap, grant, installed
artifact, global setting, host administration or product adapter is introduced.
Mount ownership, finite work deadline/outer VM bound, lifecycle/cleanup reporting,
actual datagram observations and strict original-IP-and-port predicates remain
unchanged. No address-presentation repair or passing contract is added.

Before sock_addr.rs SHA256:
`0184eab9bfa8ecc629a6170ba1418fe7623ff948e4ec1805879546c74d095939`.
After source SHA256:
`52b4c270623abac18894c98cfd1bca880fd091804dac56488706f12c9ec4a5f5`.
The sole source diff is7 added/1 removed lines in that branch; initial/followup
source and all earlier evidence remain preserved.

## Actual implementer light results

Private Fedora `8fadc1ec213332e16cb0d915a3fb913f`, actual r138, unchanged admitted
4CPU/1GiB/2GiB, pinned rustc1.98.1, NEW
`~/managed-network-sock-addr-proof-v7` source/target:

```sh
python3 docs/research/measurements/managed-network-sock-addr/light.py \
  "$HOME/managed-network-sock-addr-proof-v7"
```

23 GNU pure units0;23 musl pure units0; both source-linked clippy panic/unsafe lint
subsets0. Policy/diff/owned rustfmt checks0. No unit calls in_mount/unshare/setns,
Cgroup::new, sockets, child clients, namespace setup or BPF. These are compile and
existing pure parser/emitter/predicate results, not a live fix or cleanup proof.
No test mirrors the inserted syscall. Full source-matching guest/host build,
seven-target matrix, all actual behavior and resource/fault proof remain root gates.

[setns-light/transfer.json](setns-light/transfer.json) identifies the retained raw
logs/commands/manifests and test-binary identities. Archive4543 bytes SHA256
`29417ac8896b9301597f214618f33503ae1fcad7480ed85b101633444809902e`,19 regular entries,
18 verified member hashes, no executable bytes. The checked copied-source hash
matches the exact new sock_addr.rs hash above. Copies in fresh directories do not
retain old sync mtimes; private targets avoid stale instrumented artifacts. Original
sources/proof directories/binaries/archive remain on the runner; nothing deleted.
`setns-light/pack.py` retains the exact packaging source.

## R01–R16 delta and owed root gates

[Original all-rule record](README.md#rule-by-rule-evidence-and-owed-work) remains;
this scoped delta adds:

|Rule|Evidence / limit|
|---|---|
|R01|Fallible syscall checked before held setns; errors propagate, no panic/index/allocation or unchecked arithmetic added. Scoped GNU/musl panic/unsafe lint0; full allocator/fault behavior unverified.|
|R02|Three primary contracts/source paths plus actual r492/r493 basis; no broader mechanism choice.|
|R03|CLONE_FS is Linux ABI flag/precondition; no new picked numeric cap/timeout.|
|R04|Existing held/new branch and typed failure/cleanup facts retained; precondition failure cannot enter callback or become success.|
|R05|Underlying failing syscall precondition addressed in the existing one-owner fixture; no manager/cache/probe.|
|R06|One local branch, one fallible syscall, no new layer.|
|R07|Existing SAFETY block and precise setns/mntns_install rationale retained.|
|R08|No measured performance result;4–5x target not achieved/claimed.|
|R09|Previously held source suspicion now authorized only after exact real SetnsHeldMount errno22.|
|R10|Source hashes/commands/logs/primary identities retained; actual new runtime/removal/restoration owed.|
|R11|Only light GNU/musl checks; full seven targets/real host combinations owed.|
|R12|Existing private Linux fixture seam/libc syscall; no product/platform adapter, hostroot or crypto change.|
|R13|Only filesystem-attribute precondition changed; no presentation/acceptance change.|
|R14|All profiling/fault/resource/cost work is root-owned and unrun in this lane.|
|R15|No guests/BPF/socket/namespace/cgroup execution, mutation, audit/profile, heavy work, child agents or merge.|
|R16|Mac36924568KiB free before edits; lockf formatting/no local compilation. Private source/target only; root clone/cache and all old evidence untouched.|

Root must build actual source-matching guest/host and rerun both connected and
unconnected cases with the same supplied kernel/config/resources/deadlines from
[README replay](README.md#light-results-and-replay). Observe actual held setns,
leaf removal/unmount and reported cgroup_removed, preserving full binary/empty
arrival facts, strict tuple mismatches and both owned reap/join/link facts. Root
owns meaningful precondition-removal/restoration and any fault/matrix/cost gates.
Overall compatibility may correctly remain cargo101 even if cleanup succeeds;
cleanup success NEVER changes responder-port/getpeername mismatch into a pass.
No new route, address presentation, native compatibility or product adoption claim.
