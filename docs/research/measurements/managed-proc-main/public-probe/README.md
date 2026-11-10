# Standalone public process-identity caller

TEST-ONLY, Linux-only, `publish = false`. Base source is signed
`5df05d7d7392426b15119a67405dece4dae4e749`; no product file or dependency is
changed. This separate Cargo workspace consumes the real public `shards-api`
through a relative path and reuses the existing `../allocation.rs` by path.
The private lock contains only exact registry versions/checksums already present
in the product lock; `records/lock-setup.json` records metadata-only resolution.

Both ordinary binary modes observe their own TGID/starttime and build the
synthetic unused spec before fault/name changes. Expected start is always
`actual.checked_add(1)`: no successful self-adoption is requested. If attach
unexpectedly returns a guest, the probe immediately forgets it and fails without
running its destructor/stop path. There is no VM, child process, signal, network,
socket, cgroup or administrator operation. Spec values are the existing API unit
fixture's unused values; refusal precedes their use to start/connect a guest.

* `oom`: the existing thread-local Rust global allocator rejects allocations
  after zero successes only around real `Shards.attach`. It requires exact
  `Refusal::Process` PID/expected-start and `CapacityUnavailable`, then checks
  descriptors return to the prior count. No formatting/assertion occurs in the
  refusal interval. Native allocator failure and every outer caller are unproven.
* `raw-comm`: changes only its own leader's `/proc/self/comm` to the existing legal
  bytes `own \xff) ) \xfe`; observes those bytes in the real non-UTF8 stat record;
  calls real attach; then restores and byte-verifies the saved name before
  inspecting/reporting the result. It requires the exact scoped
  `DifferentStart { observed: original }`. An owned guard attempts restoration on
  all returned errors/unwind; a failed restore/verification is failure, never a
  passing cleanup claim.

This is an actual public consumer rather than a new test seam or mutation
framework. The independent pre-fault fixture parser selects field22 after the
last `)`; the product reader/parser is not included or substituted. Primary
semantics: [proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html),
[proc_pid_comm(5)](https://man7.org/linux/man-pages/man5/proc_pid_comm.5.html), and
[Linux v6.18 raw comm producer](https://github.com/torvalds/linux/blob/v6.18/fs/proc/array.c).
Existing private-unit, historical live-red, typed-producer and compatibility
records remain in [../TYPED.md](../TYPED.md).

From the checkout root on native Linux:

```sh
bash docs/research/measurements/managed-proc-main/public-probe/run-light.sh v1
```

One admitted private 4CPU/1GiB/2GiB light run builds the release binary, records
actual Cargo `fresh: false` and its hash, executes each mode once, then strict
probe clippy. It uses only `~/managed-proc-public-target` and
`~/managed-proc-public-evidence/<phase>`, refusing an existing evidence phase.
Raw source/toolchain/commands/exits/logs are retained; binaries stay remote.
No native result is claimed until its retained output is reviewed.

## Native light result

Fedora GNU amd64 r236, exact source
`c42b6129d2aaa1dc66f1c6ab9f9138afe9a137fb`: release build, `oom`, `raw-comm`,
and strict probe clippy all exit0. One admitted 4CPU/1GiB/2GiB request sufficed.
Actual compiler was pinned Rust1.98.1 (`48a229ceaefd4985c50990b14116b6d856af0985`),
Linux6.19.10-300.fc44 amd64. Cargo records both API and probe as `fresh: false`.
Binary SHA256 is
`c821940799e593f029dddfae097432e0057e2bde7a94bdd3d9b68b9c6d73c392`;
immutable copy remains `~/managed-proc-public-evidence/v1/probe.bin`.

| Mode | Actual public result and cleanup |
| --- | --- |
| `oom` | Own PID4162915, original start10566276, expected10566277; exact scoped `CapacityUnavailable`; FDs9→9. |
| `raw-comm` | Own PID4162916, original start10566276, expected10566277; exact scoped `DifferentStart` observed10566276; real invalidUTF8 stat bytes and restored original name verified before final check/report. |

Raw commands/exits/build/toolchain/source hashes and mode output are under
[records/native-v1](records/native-v1). The original 3645-byte raw archive has
SHA256 `8d55f26b8ab4f677cd2f5b2dc17319e298179585e9e234b846f66f466b258453`;
the extracted19 files and13 actual-source hashes were checked against the
manifest and source Git blobs. [Job](records/native-v1-job.json),
[transfer](records/native-v1-transfer.json),
[verification](records/native-v1-verification.json) and
[manifest](records/native-v1-manifest.json) retain provenance. No binary is
committed. The product API/Cargo source tree is byte-identical to5df.

Private synced checkout (inactive after r236):
`/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/managed-proc-public-probe-771d41ab38`.
Coordinator replays/mutations must use their own checkout/target rather than
overwrite these source/artifacts. Direct frozen binary commands are
`~/managed-proc-public-evidence/v1/probe.bin oom` and
`~/managed-proc-public-evidence/v1/probe.bin raw-comm`; the coordinator should
record their own replay results separately.

Policy receipt: implementer, full canonical g121-main AGENTS/R01–R16/evidence,
CLAUDE/managed plan/current handoff; revision
`f5cec365d2e435afae5e6f613c92d9d3759ef60f`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Only this directory is owned. R01/R04/R06/R07/R13: small explicit mode and
restoration states, typed checks, no product panic claim. R02/R03/R05/R12:
existing real consumer/allocator/OS seam, documented fixture bytes, no new
architecture, cache, polling, manager, runtime bound or product dependency.
R09/R10: prior reds preserved; this batch supplies public-caller results, not
new production changes. R08/R14: no cost/ratio/allocation-free claims; all
comparisons remain coordinator-owned. R11: Linux experiment only, other target
and whole-library guarantees remain owed. R15: no children/main merge, only
source/light checks and signed checkpoints. R16: private paths, no cleanup or
local build; old/root artifacts remain untouched.

Owed to the coordinator: independent real public-caller guard removals for
formatting/UTF8/infallible growth, restored revision, combined ordinary corpus,
supported matrix, actual read interruption, resource/cost and final adoption
review. Historical timing-only regression and whole-owned macOS Mutex OOM red
remain unchanged. This checkpoint cannot establish whole-library panic freedom,
complete error propagation, performance, leak freedom or main readiness.
