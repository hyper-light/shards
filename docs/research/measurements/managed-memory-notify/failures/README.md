# Finite FD-admission launcher for ROOT execution

Only this new `failures/` subtree is owned. The parent native fixture is frozen at
source SHA-256 `bafc3ff31ae9f5a4c484124911c890494860bc88c50fd2d751aed57c2f44aab7`.
Production, existing evidence and the preserved IOMMU proposal are read-only.
This prepares ROOT's actual error tests; it does not reproduce a defect, select a
buffer/limit, or establish cgroup/pressure/OOM behavior.

POLICY_READ implementer `01a1c5bda0bf766683384c0705c0a9bc`; coordinator
`01a148884a4770fdaf0111f3f792a248`; canonical main
`1d0588b28f839fd0fec049e4bcb00e261e28c3aa`; full AGENTS, R01–R16, evidence,
CLAUDE, plan and handoff read before edits. Rules SHA-256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Handoff SHA-256 `b5e6ba4b693270992874974d586a6a65b21d6517ab3964913d8e852f0bf62b98`.
Its older refs do not override the current explicit assignment. No child agents,
goal/plan dispatch, lane-completion claim or main merge is authorized.

## Exact interface proposed to ROOT

```text
python3 -B launcher.py
  --binary ABS --binary-sha256 HEX
  --child-helper ABS --child-helper-sha256 HEX
  --record-dir NEW_ABS
  --deadline-monotonic-ns U64 --writes POSITIVE --read-bytes HEADER_OR_LARGER
  --nofile-soft POSITIVE --outer-timeout-ns POSITIVE --max-log-bytes POSITIVE
  --expect-operation {create_new_file,inotify_init1,independent_open}
  --expect-errno POSITIVE --expect-exit NONZERO
```

All inputs are mandatory. The two executable identities refer to ROOT's frozen
native fixture binary and its separately compiled new `nofile-child.c` helper,
respectively; a source hash is not substituted for an executable hash. Paths
must be caller-owned private artifacts, immutable during the run. A preflight
path/hash check alone is not an atomic bind between bytes and a later path exec;
this is a controlled immutable-file fixture, not an adversarial exec mechanism.

The original native deadline is forwarded unchanged. The outer deadline is a
single checked `CLOCK_MONOTONIC` timestamp computed at launcher entry from the
caller timeout, covering preflight/setup/capture and cleanup. It must extend past
the native deadline, leaving only the caller's already supplied remaining window
for kill/reap/drain. Native execution stops at its original deadline; the outer
deadline does not extend native authority. Neither timestamp resets on a retry.
`max-log-bytes` bounds combined raw stdout/stderr retention. Reaching that bound
before both EOFs makes output incomplete and prevents a case pass, even if an
unread final byte might have been EOF. The caller admits parent capture/storage
cost separately from the constrained native child.

The native helper receives only `SOFT BINARY NEW_DIRECTORY ORIGINAL_DEADLINE_NS
WRITES READ_BYTES`. It changes **only its own soft `RLIMIT_NOFILE`**, refuses
increases, preserves/readbacks the existing hard limit, then uses absolute-path
exec. The parent performs no setrlimit/prlimit call and no global inotify change.

## Source contracts and conditional choice

[Exact source identities](evidence/source-identity.json) include retrieval time,
commit, URL, size and SHA-256. Pinned research is not the actual running toolchain
or kernel identity, which ROOT must retain separately.

| Primary source | Relevant contract / alternative |
| --- | --- |
| [Linux 6.18 resource limits](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/sys.c) | `do_prlimit` separates soft/hard attributes. A fresh helper can lower self soft while preserving hard; other-process/global setters are outside scope. |
| [Linux 6.18 descriptor allocation](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/file.c) | Allocation tests the candidate descriptor against the NOFILE bound and returns EMFILE. Extra inherited or observer FDs can change the failing operation. |
| [Linux 6.18 inotify](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/fs/notify/inotify/inotify_user.c) | Inotify has both descriptor acquisition and per-user instance accounting. An errno alone does not identify which admission constraint was decisive. |
| [util-linux prlimit 2.40.2](https://raw.githubusercontent.com/util-linux/util-linux/1cebcc34373ed34266f726fff71030a83b4df7da/sys-utils/prlimit.c) | Established set-limit-before-exec pattern. Its general PID/resource/PATH interface is broader than this one-child, one-soft-limit helper. No dependency on an installed prlimit is introduced. |
| [CPython 3.14.0 subprocess native setup](https://raw.githubusercontent.com/python/cpython/ebf955df7a89ed0c7968f79faec1de49f61ed7cb/Modules/_posixsubprocess.c) | `close_fds` closes descriptors above stdio except explicit keepers; fork/vfork callback restrictions matter. This launcher uses a freshly exec'd native helper, with no Python preexec callback. |
| [CPython 3.14.0 subprocess](https://raw.githubusercontent.com/python/cpython/ebf955df7a89ed0c7968f79faec1de49f61ed7cb/Lib/subprocess.py) | Timed wait uses short sleeps. The proposed fixture waits on readiness/exit notification instead, retaining one outer deadline. |
| [CPython 3.14.0 pidfd watcher](https://raw.githubusercontent.com/python/cpython/ebf955df7a89ed0c7968f79faec1de49f61ed7cb/Lib/asyncio/unix_events.py) | Established parent-only owned-child pidfd readiness followed by wait/reap/close. No event-loop framework, global signal handler, socket or watcher thread is adopted. |
| [Linux 6.18 pidfd acquisition](https://raw.githubusercontent.com/torvalds/linux/7d0a66e4bb9081d75c82ec4957c50034cb0ea449/kernel/pid.c) | pidfd is close-on-exec. Its proposed acquisition is only for the still-owned child; it is not an arbitrary PID-control API. |

The parent reuses the **existing adopted ROOT own-child wait seam** in
[`coordinator/driver.executed.py` at main1d0588](https://github.com/hyper-light/shards/blob/1d0588b28f839fd0fec049e4bcb00e261e28c3aa/docs/research/measurements/managed-memory-notify/coordinator/driver.executed.py):
hold the Popen child unreaped, acquire its pidfd, observe exit readiness, then
reap/close. Its exact blob/hash is retained in the new identity record. This
launcher requires default SIGCHLD and a sole reaper in its fresh parent process.
It uses one original-deadline poll over its two capture pipes and own-child pidfd,
without timed-wait sleeps, a new signal handler, namespace, socket or watcher
thread. The initially proposed extra stdin gate was dropped; stdin is `/dev/null`.
This is source for ROOT review, **not native-executed evidence**.

The C helper closes its own fd>=3 using Linux close_range, checks all stdio FDs,
and labels/closes its transient `/proc/self/fd` observer **before** lowering soft.
It rejects an unexpected inherited descriptor or unavailable close_range instead
of adding an unrelated fallback. Dynamic-loader descriptors and failures remain
actual ROOT observations; documentation cannot promise the native program will
reach a particular error site at a guessed soft limit.

## Required separation in the result

The raw native stdout/stderr and actual return code are evidence, not a wrapper
success label. ROOT's expected-operation/errno/exit assertion is independent.
The intended record distinguishes at least:

- helper/loader/exec setup failure;
- actual native terminal failure matching the explicit expected operation/errno;
- wrong native cause, unexpected native success, timeout or incomplete output;
- actual child exit/reap, parent FD return, retained sentinel/created paths and
  original native/outer deadline identities.

An expected-failure case can succeed while `native_passed` remains false and the
native return remains nonzero. No arbitrary nonzero status is accepted as an
inotify EMFILE result. Establishing FD admission as the cause also needs ROOT's
source-matching control with sufficient child slots and actual inherited-FD
evidence; inotify's independent per-user limit is not silently excluded. The
private frozen program's exact terminal bytes are matched as an artifact-specific
presentation contract; this does not introduce a production wire protocol.

`stdout.raw` and `stderr.raw` retain the actual native/helper streams independently.
`result.json` is explicitly the **pre-result-writer-close observation record**,
with `case_assertions_match_before_result_close`, not a final case pass. The final
launcher JSON on stdout adds the closed result-writer FD census and
`expected_failure_case`; its actual exit is zero only when all independent checks
hold. Root must retain that final launcher stdout/stderr/exit too. The native exit
is always retained separately. No arbitrary nonzero exit, loader failure or helper
failure can satisfy the frozen native terminal assertion.

Failures/timeouts request termination only for the still-owned child and attempt
reap within the **same** outer deadline. Missing exit readiness, failed acquisition
or kill, and incomplete cleanup remain typed uncertainty, never an expected case
pass. On failed pidfd acquisition, the sole unreaped Popen owner attempts kill and
one nonblocking reap; it does not add a state-probe loop or an unbounded wait.
Root's enclosing job owner must retain/resolve any such incomplete child outcome.
Blocking filesystem/exec/report kernel calls cannot be preempted by Python's
deadline checks; root supplies the independent finite outer process/job stop as
well. No universal hard kill-time or native fault/OOM-resistance claim is made.

Every record directory/file is create-new and retained. No path is deleted,
including partial setup, sentinels and replaced paths. Only the new native target
subdirectory and its known `watched` artifact are inspected after the case; this
is not a filesystem/repository audit. Parent-owned setup/observer/control FDs
must be labelled independently from target fd0/1/2 and from transient census FDs.

## Validation boundary

Implementer runs only argument/checked-arithmetic tests and format/syntax checks
in admitted new private remote scopes <=1 CPU/1 GiB/2 GiB. Neither this C helper
nor the frozen binary is compiled/executed by this lane. ROOT owns all native
FD limits/readiness/error/guard/trace/cost work, with exact binaries, source,
kernel/libc/Python versions, commands and raw results retained. GNU/musl and
amd64/arm64 outcomes remain unverified until actually run. macOS arm64 policy,
paused Windows ARM VM, root clones/targets/caches and 30 GiB local floor remain
binding. No local heavy build, full worktree, cache deletion or production change.

## Actual light checks and owed ROOT runs

[Pure check driver](evidence/pure-driver.py) copies only the three source files to
a NEW admitted private remote directory, pins one CPU affinity and a 1 GiB
address-space limit, and runs `python3 -B purechecks.py`. It never invokes the
launcher entrypoint or compiles C/native code. Its per-file 2 GiB size cap is not
an aggregate disk quota. No parent NOFILE is changed by the launcher or pure driver.

[First pure record](evidence/pure-attempt-01.json): eight argument/arithmetic tests
passed, removing upward timeout rounding failed for `0 != 1`, then the unchanged
source's eight tests passed. This was an intermediate argument-only source, not
the final parent lifecycle body. Python3.14.3 on Fedora GNU amd64, new private
`/home/adalundhe/managed-notify-failures-pure-vf7dwo85`; source hashes, exact argv,
outputs and admission retained.
[Final-source pure record](evidence/pure-attempt-02.json) covers nine argument and
checked-arithmetic tests, the relevant round-down guard failure, and restored
nine-test pass, in new private
`/home/adalundhe/managed-notify-failures-pure-4_9r5pwk`, with the same admitted
1 CPU/1 GiB/2 GiB scope. Launcher source SHA-256:
`b10e3895ebe5844e23e0c9f3178ea66d3cf535a204cf14b27a8e7c6273ba95b4`;
C helper source SHA-256:
`4ba28b107a0f9b325f021d2106f57721eabef6db1d173df0d0deb114ea28e1ea`.
The full Python body is parsed when imported, but the native entrypoint/capture/
classification/cleanup paths are not called. C source is hash-recorded only.
Python AST syntax and manual source formatting are light
checks; no C compilation/typecheck/native failure outcome is inferred.

ROOT must compile `nofile-child.c` with its actual GNU/musl toolchain, retain
compiler/source/binary hashes, and supply that binary's SHA-256 explicitly. No
existing helper/fixture source, target, binary, cache or evidence is overwritten.
The straightforward private build command is `cc -std=c11 -Wall -Wextra -Werror
nofile-child.c -o NEW_PRIVATE_HELPER`; all actual compilation is ROOT-only/unrun by
the implementer. Use caller paths/finite values in the interface above and retain
each exact invocation. The ELF loader is part of the observed path.

| Owed ROOT case | Required independent observation |
| --- | --- |
| Actual inotify-init EMFILE | Sufficient-budget control first; helper inherited-FD/soft/hard readback, exact native operation/errno/nonzero exit, retained empty created file, sentinel unchanged, held exit and parent FD/limit return. A copied/guessed soft value is not an actual result. |
| Actual independent-open EMFILE | Actual successful writes, notification instance/watch setup, exact reopen failure, independent final bytes, native removal/closes and returned parent resources. Notification completeness/pressure is not inferred. |
| Loader/helper/exec failure | Captured raw stderr and actual exit; missing native terminal must fail the expected-native assertion rather than pass because an exit was nonzero. |
| Sufficient child slots | Actual native success must make an expected-failure launcher case fail; actual native exit0 remains visible. No automatic case-status normalization. |
| Wrong binary/hash/input/path/deadline | No overwritten pre-existing path; exact failed phase and whether any native child/path was acquired. Hash checks require controlled immutable executables. |
| Timeout/capture/report/identity failure | Original timestamps retained; only owned child signalled, exact readiness/reap/FD return or uncertainty; partial raw records and artifacts preserved. No full fault claim from pure inputs. |
| Native guard removals/restoration | Remove actual child-limit/inheritance/expected-cause/cleanup guard independently; corresponding actual wrong effect/cause/resource must invalidate the case, followed by source-matching restored runs. |
| Portability/cost | GNU/musl amd64/arm64 as available; all profiling, allocation/kernel attribution and equivalent controls remain ROOT-owned/unrun. No benchmark/4–5× claim. |

## R01–R16 account

| Rule | Evidence / remaining gap |
| --- | --- |
| R01 | Production untouched; checked argument/deadline arithmetic and explicit fixture errors, pre-release capture allocation, owned close/reap paths. Python/stdio/library OOM and all live paths remain unverified; no universal panic/fault claim. |
| R02 | Exact pinned Linux/util-linux/CPython and existing adopted root driver identify contracts/alternatives. A narrowly scoped test launcher only; live failure evidence remains ROOT-owned. |
| R03 | Explicit caller limits/envelopes/deadlines/expected cause; UAPI header, integer/exit ABI and physical time units derived. Stdio three is fd0/1/2's ABI, not a learned production capacity. No production number selected. |
| R04 | Preflight/launch/capture/reap/expected-failure/not-established phases; actual native result, case assertion, timeout and cleanup uncertainty remain separate. Live transitions unrun. |
| R05 | Existing fresh-process limit/exec and own-child readiness seams; one child owner, no cache, polling interval, fallback manager or extra watcher. Actual perturbation/cleanup proof owed ROOT. |
| R06 | One parent and one narrow C exec helper; focused input/capture/identity/census/reap helpers. No repository refactor or generic process framework. Native path review/compilation owed ROOT. |
| R07 | Comments explain fresh exec, observer FD exclusion, sole-reaper identity, original deadline, conditional reap and provisional result recording. |
| R08 | No competing baseline, speed/resource result or achieved ratio; ROOT cost controls remain owed. |
| R09 | Actual exhaustion is still owed, not a reproduced production bug. No production correction from source prediction. |
| R10 | Exact primary identities and pure argv/results/mutant/restored evidence retained; all native red/green/guard/resource traces owed ROOT. |
| R11 | Linux-specific research artifact; GNU amd64 pure proof only. Native GNU/musl amd64/arm64 unrun, macOS arm64-only policy and paused WinARM unchanged. |
| R12 | Existing libc/stdlib seams, no installed deliverable, host-root/global-resource/security/crypto change. SHA-256 uses existing hashlib for artifact identity. |
| R13 | Explicit one-child inputs, one native helper, bounded raw capture and create-new retained artifacts; no cache, framework or automatic policy. |
| R14 | No measurement/profile/campaign execution by implementer; capture/hash/setup overhead and kernel/parent costs unmeasured and owed ROOT. |
| R15 | Sole writer, no children/plan/goal dispatch, no main merge. Named signed source checkpoint is review progress, never lane release/completion. ROOT owns every native run, audit and campaign. |
| R16 | No local build/cargo/large copy/new worktree or cache deletion; admitted remote private pure files preserved; root targets/frozen artifacts/paused VM untouched, Mac30 GiB floor retained. |
