# Native exec failure status placement

This is a tested component caller fixture, not an installed native job executor.
The two GNU cases passed at the exact sources below. The production helper's
real owner/admission/ACK/audit caller remains native/parent-owned and unverified
by this fixture. No production path was edited.

Coordinator: `01a148884a4770fdaf0111f3f792a248`. Implementer:
`/root/native_program_executor`. Base/current policy revision:
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`; rules SHA-256:
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Owned paths are only this measurement directory, in
`managed-native-exec-status-placement-v1`. The resumed relay implementer remains
the sole production process-module writer. No child agent was started.

## Inputs and repeatable invocation

`INPUTS.json` pins the exact dependency selections. `frozen-process-v4/` retains
the owner's unchanged freeze and original tests. The harness production
`process.rs` is byte-identical SHA-256
`05ccf9552e4ad4521584cc6e5fe222eb5c3d4315d63151954e2f9412781c3eec`.
Its existing test leaf receives only one additional test-module registration.
The mapper, file, objects, lifecycle codec and test-directory implementation are
exact signed `46ab5f60b03d61bb7a53ac404a38d8c477d65727` selections. Literals uses
the owner's exact narrow counts delta, resulting SHA-256
`4ee5e5c298c345faf199e54acbcd33b4a2c9fbf3fb5289b6008f67482141e4ee`.
Only the four existing resolver constants are selected from `server.rs` for
compiling the unchanged file module; no new resolver or mapper is substituted.

The standalone harness aliases its crate as `shards_ipc` to compile that same
mapper source. It includes the original component tests for compilation but runs
only the exact two names under `status_placement::`. This is not the carried
server/init graph, and original ignored/filtered tests are not reported passed.

`PACKAGE-v1.tar.gz` is the exact pre-build source archive: 43,488 bytes, SHA-256
`bbc28e11b22c684415c55693decd56e9649b9fc74ee921c216185bf3d4374f28`.
`SOURCE-MANIFEST.json` was checked before build and after the whole batch; its
SHA-256 is `e8299de5a68805cc8ac94015cf398b7640ca8b97997ef43d10827ff998a6c9cb`.
In a fresh Linux scope, extract that archive and invoke:

```sh
python3 run.py
```

The actual normal Kiln admission used Linux amd64, two CPUs, 2 GiB requested
memory, 4 GiB requested disk, cargo `-j2` and a 600-second absolute batch including
cleanup. It used `sync:false`, no guard bypass, no system install and no local
build. The driver used the first two CPUs from its actual inherited affinity;
the effective set was `[0, 1]`. Those asks are not hard resource backing claims.
All child processes belong to the fixture's private process group; a driver
timeout kills that group and records cleanup uncertainty rather than a pass.

## Cases and actual results

The private fixture supplies a checked executable object whose generated content
is deliberately not an executable image, a held workdir, literal argv and empty
environment, separate stdio pipes and status pipes. No shell is invoked. Before
fork it captures the actual raw FD from
`Prepared::launch_status_before_install()`. A separate disposable pipe is placed
at FD 3 in the child as the wrong-endpoint witness.

| Case | Native failure and selected location | Actual observation |
| --- | --- | --- |
| Before map installation | The child closes only its own staged stdin source. The real mapper returns `Install { destination: 0 }, EBADF`; `status_location()` returns `Staged`. | Saved status FD 17 receives canonical `Failed { pid: 1742341, failure: Setup/EBADF(9) }`; the FD 3 witness receives no bytes. |
| After map installation / failed exec | The same real map installs/cleans descriptors and `execveat` rejects the private non-image with `ENOEXEC`; `status_location()` returns `Installed`. | Staged status 17 is actually `EBADF`; installed FD 3 receives canonical `Failed { pid: 1742343, failure: Exec/ENOEXEC(8) }`. The wrong endpoint receives no bytes. |

Both children deliberately overwrite ambient errno with `EINTR` after capturing
the typed failure. The emitted packet retains the original positive errno and
step through the unchanged lifecycle codec. Unknown/unmappable failures select
no endpoint and raw-exit as fixture failures. EOF is never converted to a typed
refusal, exec success, reaping or resource return.

The parent prints each actual record before assertions, decodes the exact
11-byte canonical packet, waits for the exact child, checks fixture exit 0,
observes a second wait returning `ECHILD(10)`, observes empty stdout/stderr and
wrong endpoint, and measures parent FD count `4 -> 4`. These are direct fixture
children; no arbitrary descendant population or kernel resource return is proven.

Actual runner: `451397fcc585f7f7cb9aaf00e42064bb`, `adacachy`, amd64 Linux
`7.2.9-2-cachyos`, Rust `1.99.1` commit
`58cf8f9c35b355199727627d07ae34a4e2846d67`, LLVM `23.1.1`.
This is not a pinned product Linux 6.18 runtime result.

All six stages exited 0: tool identities, offline locked test build, exact binary
list, the two native cases, and `cargo clippy --locked --offline --all-targets
--jobs 2 -- -D warnings`. Native result: **2 passed, 0 failed, 0 ignored,
21 filtered**. The total functional batch took 3.628 seconds; this is a deadline
receipt, not a performance benchmark. Local targeted rustfmt check and
`python3 scripts/check-development-policy` exited 0 (`LOCAL-CHECKS.json`).

The pre-actor frozen test binary was 9,878,504 bytes, SHA-256
`88d0480fc0ef217db57552475f910ca09b7c12ff6560a55da80513cd4e81258b`.
Original sources, targets, private temporary root and frozen binary remain at
`/home/adalundhe/native-exec-status-placement-05cc-v1` on that runner. No cleanup
of prior work/evidence/caches was performed by this implementer.

`raw-gnu-v1.tar.gz` is 4,122 bytes, SHA-256
`6523bce9cbd526b38baa23568507056f9feb4be10cf5e8de87c51724668272f8`.
Every one of its 18 payloads was independently checked against the retained
`ORIGINAL-FILES.json`; readable exact copies are under `raw-gnu-v1/`. It retains
full build, list, native, strict, identities, pre-actor and driver output. The
binary is retained on the runner, identified locally rather than copied into Git.

## Sources, state and integration limit

This is new caller validation, not a claimed reproduced production defect or
production correction. The first old-input freeze stopped on a hash mismatch
before copying the advancing process leaf; `INITIAL-FREEZE-STOP.json` retains
that setup failure. No child/build ran there. Immutable v4 is a new exact input,
not a retry of a failed runtime case.

The fixture exercises `prepared -> child map failure -> staged report -> exit`
and `prepared -> map installed -> exec failure -> installed report -> exit`.
Parent transitions require explicit packet decoding plus actual matching wait
and `ECHILD`. There is no cache, periodic probe, new watcher, ledger, manager or
authority constructor. Blocking waitpid is an event wait; EINTR repeats the same
interrupted native operation. No lock is held over actor I/O.

The actual held-object and status semantics are grounded in the inspected pinned
mapper/codec and these primary sources. Linux man-pages 6.19 documents the
`execveat` failure return and explicit argument/environment vectors:
https://man7.org/linux/man-pages/man2/execveat.2.html and
https://man7.org/linux/man-pages/man2/execve.2.html . Linux v6.18's clone UAPI is at
https://raw.githubusercontent.com/torvalds/linux/v6.18/include/uapi/linux/sched.h .
The established runc v1.3.3 launcher also explicitly synchronizes actual child
identity and release; its source was inspected as an alternative, not imported:
https://raw.githubusercontent.com/opencontainers/runc/v1.3.3/libcontainer/nsenter/nsexec.c .
No choice of an alternate launcher or broader architecture is made here.

`CALLER-HOOK-UNAPPLIED.md` identifies the tested capture/selection boundary and
the missing real helper splice. The current native helper per-job caller must
freeze its actual span before a truthful applied patch can be formed. This
fixture supplies a concrete compiling caller and exact bytes, not a synthetic
production producer. The full parent/current-owner/process permission/resource
admission/audit/clone3/pidfd/ACK/cgroup and rich outcome integration remains with
its actual owners.

## R01–R16 account and owed coordinator work

| Rule | Evidence or concrete scope limit |
| --- | --- |
| R01 | Production unchanged. Raw fixture child uses only prepared stack/held objects, checked codec/matches and native calls, then `_exit`; no allocation, unwind or inherited destructor. Strict Clippy passes. This does not audit unchanged production implicit panic/allocation paths. |
| R02 | Exact production mechanisms and primary/established sources above are inspected. The choice is a narrow caller fixture at the existing map/status seam. No broader launcher decision or external compatibility claim is made. |
| R03 | Production literals unchanged. FD 0–5 and packet length come from the exact existing map/codec. Errnos/flags/modes are native ABI facts. Two CPUs, `j2` and 600 seconds are the assigned test envelope, never runtime authority. |
| R04 | Explicit case/state and typed location/failure matches; canonical packet plus actual wait, never EOF classification. Unknown mappings raw-exit without a packet or invented errno. |
| R05 | Reuses one mapper/codec, no new runtime abstraction/cache/poller/state owner. Waits block on child completion and repeat only EINTR. |
| R06 | Two finite cases share one focused raw child and one parent verifier; no combinatorial campaign/framework. |
| R07 | Comments identify unsafe obligations, actual placement and distinction from owner authority. |
| R08 | No comparative measurements or 4–5x claim. Equivalent baselines/distributions remain ROOT-owned and unrun here. |
| R09 | This adds new caller coverage; it fixes no asserted legacy defect. Actual live EBADF and ENOEXEC observations are retained. No red/green production result is inferred. |
| R10 | Exact package/source/binary pins, pre-actor identity, all raw records and verifier retained. No guard-removal mutation was authorized/run in this implementation-only fixture. |
| R11 | GNU amd64 component check only. Musl/arm64, all seven build targets and real KVM/HVF/WHP integration are unverified here; macOS support remains arm64 only and WinARM stays paused. |
| R12 | Exact shared Linux guest mapper/codec and held-object implementations reused. No platform adapter, installed binary or host-root operation is added. |
| R13 | Small docs-only compiling caller, two cases, exact source freeze and one bounded driver; no production manager or synthetic grant. |
| R14 | No profiling/benchmarking performed. Parent FD return is observed; memory/kernel costs, descendant cleanup, fault/throughput/latency distributions and comparative ratios are unverified and ROOT-owned. |
| R15 | Implementer light scope only, no children, no merge. Native/parent/resource/audit own the real caller and ownership integration. ROOT review/signature/adoption and broader gates remain owed. |
| R16 | Fresh private remote scope, normal 2/2/4 admission, all original/frozen artifacts retained. Earlier local below-floor hold was respected; no local builds/actors or self-cleanup occurred. |

Remaining coordinator gates include real helper caller integration, held actual
owner-issued job scope before birth, saved-errno wire mapping of every supported
failure, process permission/current-owner/audit ordering, population termination
and reaping, cancellation/revocation/restore/move, meaningful guard removals,
supported libc/architecture/VM/seven-target coverage and parent/kernel cost data.
