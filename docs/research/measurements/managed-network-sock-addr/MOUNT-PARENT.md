# Reproduced missing scratch parent: fixture-only preparation

Policy receipt renewed before edits: canonical g121-main AGENTS/rules R01–R16,
evidence, CLAUDE, full managed plan and current handoff read. Revision
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`;
role implementer. Owned only `intercept/sock_addr.rs` minimal mount-parent
preparation and this fixture's measurement evidence. Root owns all actual guests,
BPF loads, profiling/audits/chaos, architecture choices and merges.

## Actual reproduced basis and precise scope

The coordinator reports real r466 at exact f4f29cc3 had BOTH guest cases fail
Cgroup/NotFound/errno2 before Target or any clients. Diagnostic-driver0 did not
mean compatibility success. Root then performed operation-level real r467 at
exact `f4f29cc3184c5540013db5851e6f5073fcaa5453`:

- OpenMountNamespace: Ok.
- CreateMountDirectory: ok=false, errno2, tmp_exists=false.
- Cgroup: NotFound/2, before client setup.

Root-retained Fedora records `~/root-managed-sockaddr-cgroup-steps-v1` contain
original/instrumentation hashes, patch, actual guest stdout and exact-byte finally
restoration. These are coordinator-supplied observations; this lane did not run
or instrument any guest. The new revision has no actual runtime result yet.

Signed source checkpoint `22286c3cf385742ef31af5097a48bbf022664994` adds exactly
one rationale comment and `fs::create_dir_all("/tmp")?` BEFORE the existing exclusive
`fs::create_dir(MOUNT)` call. `/tmp` is the already-selected fixture scratch parent
of the mount/report paths, not a new production path or resource limit. Failure
still returns through the existing Cgroup typed stage. Existing mount-path
AlreadyExists refusal, private leaf ownership and removal/unmount sequence remain.
The scratch parent may preexist and is not deleted by fixture cleanup; the exclusive
owned mount path remains separately created and removed. No permission/global
setting change or relaxed conflict check was introduced.

The [official create_dir contract](https://doc.rust-lang.org/std/fs/fn.create_dir.html)
rejects missing parents and existing target paths. The
[create_dir_all contract](https://doc.rust-lang.org/std/fs/fn.create_dir_all.html)
prepares missing parents and may retain created parent components on error. These
current official documentation pages identify std1.99.1; actual code compilation
below pins1.98.1. Version-specific1.98.1 web pages were inaccessible; no claim of
retrieving them is made. Creating the full MOUNT path recursively would accept an
existing mount target and was rejected as a weakening; only its known parent is
prepared. No broader mechanism decision or production adaptation is made.

Before sock_addr.rs SHA256:
`6cc260b2e330347670473ed0003b9856e1db30258f368253eb537a0222056af1`.
After source checkpoint SHA256:
`0184eab9bfa8ecc629a6170ba1418fe7623ff948e4ec1805879546c74d095939`.
The diff is exactly two added lines. CLONE_FS/setns/unshare behavior is unchanged;
its suspected cleanup issue remains UNREPRODUCED and out of scope. Existing
socket, hook, worker, original-IP-and-port predicates and observation code are
byte-identical. Original f4f29cc3 and prior source/evidence checkpoints remain.

## Actual light checks and identities

Private Fedora `8fadc1ec213332e16cb0d915a3fb913f`, r136, admitted4CPU/1GiB/2GiB,
new `~/managed-network-sock-addr-proof-v6` source and target, pinned rustc1.98.1:

```sh
python3 docs/research/measurements/managed-network-sock-addr/light.py \
  "$HOME/managed-network-sock-addr-proof-v6"
```

Actual23 GNU pure units passed;23 musl pure units passed; GNU and musl source-linked
clippy with explicit panic/unsafe lint subset passed. These tests do not execute
Cgroup::new or mkdir/mount/socket/BPF/child paths. They verify source compilation
and the existing pure parser/encoding/predicate cases, not successful preparation
or compatibility. Policy check/diff check/owned rustfmt check0. No new tests mirror
the one directory call. Full actual caller/guest build and seven-target checks
remain root gates; old revision results are not inherited passes.

Raw logs/commands/source-hash manifest/test-binary identities:
[mount-parent-light/](mount-parent-light/transfer.json). Archive3882 bytes,
SHA256 `bf640c34530879e8debf33ce01e07d70cc8ea30b1ced00857aa2df00a2663970`,
18 regular entries,17 member hashes verified. No executable bytes transferred.
Fresh source-copy mtimes and private target avoid old instrumented-artifact reuse.
Original proof source/targets/binaries and archive remain retained remotely.

## R01–R16 delta accounting and owed coordinator runs

The [original all-rule record](README.md#rule-by-rule-evidence-and-owed-work)
remains applicable; this two-line delta adds:

|Rule|Evidence / limit|
|---|---|
|R01|Fallible std operation propagates error; no assertions/indexing/unchecked arithmetic/new capacity. Scoped panic/unsafe lint subset0; std allocations/fault behavior unproven.|
|R02|Exact live root operation facts plus official API contract; no mechanism selection.|
|R03|Existing fixture scratch parent only; no cap/timeout/learning/authority change.|
|R04|Existing Cgroup failure and mount lifecycle retained; no error-as-success path.|
|R05|One preparation at the actual failing path; no polling/cache/manager/abstraction.|
|R06|One operation, unchanged lifecycle complexity.|
|R07|Comment explains direct-initramfs missing-parent assumption and target exclusivity.|
|R08|No performance claim;4–5x not measured or achieved.|
|R09|Root actual r467 identified exact ENOENT before fix. CLONE_FS remains unverified and untouched.|
|R10|Exact source identities, retained compiler logs and root red record location; actual after-run owed.|
|R11|Light GNU/musl only; seven targets/real VM combinations unverified for new source.|
|R12|Shared standard fs API; no installed artifact/hostroot/crypto changes.|
|R13|Only reproduced parent omission addressed, exclusive target still required.|
|R14|All profiling/fault/cost work root-owned and unrun.|
|R15|Pure/light implementer checks only; no guests/loads/namespaces/sockets/cgroups, child agents, mutations, audits/heavy work or merge.|
|R16|Mac36989588KiB free before edits; lockf formatting, no local compile. Private Linux target; root clone/cache untouched; no source/evidence deletion.|

Coordinator owed: build actual full source-matching guest and host caller; run both
explicit connected/unconnected ignored cases with the same supplied experimental
kernel/config/deadlines/resources in [README replay](README.md#light-results-and-replay).
Record actual progress beyond directory creation, every subsequent setup refusal,
full Binary/Empty native metadata and strict original endpoint verdict, cgroup
placement/ownership/reap/join/detach/remove facts. Any later failure remains a
failure. Relevant fault/mutation/restoration, cost/matrix and architecture decision
remain separate root gates. No CLONE_FS or other runtime remedy is authorized by
this mkdir result alone. No product adoption or main integration claim.
