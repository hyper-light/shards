# Repeated orphan-child failures beside a live harness

Coordinator /root, current R01–R16 policy c921a0b1 / SHA256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
Source fixture f73d1188, final tested source b8febd1c; owned testguest::managed_reap
and managed_lifecycle host test. No production lifecycle change is proposed.
Positive, independent mutation and restored KVM results are recorded below.

The supplied AGENT repeatedly execs a controlled intermediate, which execs a leaf
and exits without waiting. The actor holds the leaf's pidfd while that leaf is
known alive, waits for/reaps its own intermediate, then releases the leaf. The
leaf reports its resulting namespace parent and kills ONLY itself with SIGKILL.
The actor waits for pidfd POLLHUP, not mere process-exit POLLIN, and compares its
own descriptor count before/after the whole round. It publishes a synced report
by rename. A separate supplied HARNESS must answer a new explicitly granted TCP
challenge after every round has finished; fresh main-workload exec must succeed.
The fixture report is controlled test evidence, not a product trusted telemetry
channel or an authorization mechanism for arbitrary actor reports.

Twenty rounds is a test workload input. One absolute caller-provided120000ms
experiment budget covers the whole guest sequence; it is the host's existing
120s test guard, not a new production timeout. The host waits for the guest's
atomic started marker, then budgets that reap wait and a separate120s report-
delivery phase. This is two existing observation phases, not a runtime grace. Blocking socket/pidfd readiness
and remaining-deadline waits replace repeated /proc state probes. Socket path
lives only in this tenant's private /tmp. No host process, peer, private manager
object or unrelated user's data is targeted. Failed I/O publishes an explicit
failure in the report where possible. VM disposal remains the test's final owner.

## Primary references and alternatives

- [Linux6.18 pidfs.c pidfd_poll](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/pidfs.c):
  exit reports readable state, removal of the task additionally reports HUP.
  [pidfd_open manual](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)
  documents the same exit-versus-reap distinction and PID lifetime conditions.
  The held live leaf removes the PID-open race for the actual tested process.
- [Tini0.19.0](https://raw.githubusercontent.com/krallin/tini/v0.19.0/src/tini.c)
  and [dumb-init1.2.5](https://raw.githubusercontent.com/Yelp/dumb-init/v1.2.5/dumb-init.c)
  reap adopted children with waitpid. Their established obligation is reap-all,
  not merely notice the principal child's exit. Shards already owns its native
  namespace init; the fixture tests it rather than adding another supervisor.
- Signal-zero or /proc-not-found alone could confuse exit/reap/reused PID.
  Sleeping then sampling zombie count would add a timing assumption. The
  kernel's held-pidfd reap event is the decisive observation instead.

Expected independent mutation: restrict only the native helper's wait4 target to
its actor, leaving adopted children unreaped; require the actual HUP/report guard
to fail. Check the existing syscall filter and actual failure before accepting
this mutation; compilation failure or a killed helper is a different result.

R01/R04/R06/R07: fallible I/O/spawn, owned handles, checked counts/deadline and
explicit handshake stages; test failures are Result/errno, no production panic
or state change. R02/R05/R12/R13: existing native reaper and kernel-event seams;
no polling cache, parallel manager, dependency or installed helper. R03: constants
are ABI sizes, kernel event/signal values, minimum handshake byte, or experiment
inputs, with no runtime tuning number. R08/R14: resource/latency benchmarking,
parent/kernel charges and competing-system ratios are unmeasured. R09/R10/R15:
the actual positive/mutation/restored runs are retained below; coordinator owns
the campaign and reviewed merges. R11: one Linux
guest implementation for both architectures, host fixture uses common APIs;
actual supported-host matrix remains owed. R16: private test homes/targets only,
existing evidence/user data preserved, Mac remains above30GiB, runner guard obeyed.
Deeper trees, threads, lifetime ownership, resource exhaustion and repeated whole
actor/reaper crashes remain separate required cases.

## Actual positive, failure and independent guard proof

Fedora x86_64 KVM runner8fadc, host uid1000, root's private managed-capture-check
and managed-capture-target,4CPU/8GiB/20GiB admitted. Native testguest strict clippy
passed at f73d1188; r97 actual KVM passed1/1 in16.02s including first14.33s guest
build. All20 orphan SIGKILL rounds reported adoption by namespacePID1 and actual
reaped HUP, actor descriptors5→5. After the rounds the main exec and fresh
healthy-HARNESS challenge succeeded. This is not a global host FD/leak claim.

R99 at4facb4ce first passed1/1 in14.86s, then its actor-only reap mutation failed
0/1 in124.08s. That failure was only “no publication of result”: the host guard
covered the guest120s wait but not subsequent synced report delivery. It is NOT
accepted as the required mutation proof. The source was restored by the trap;
restored/clippy stages did not execute because the strict evidence grep failed.
The complete failed log remains in reaping-runs.

At b8febd1c the host separately budgets reap and delivery after the started marker.
R108 completed the following exact-source sequence and exited0 overall:

- Normal1/1 passed2.14s,20 actual orphan deaths/reaps and fresh main/healthy progress.
- Independent patch changes ONLY wait4's target from all children(-1) to ACTOR.
  It compiles/runs under the unchanged native helper filter. Actual failure:
  `unreaped pid 4 state Z`, `failure TimedOut errno None`;0/1 failed121.52s,
  cargo101 at the report's failure assertion. This is a real unreaped orphan,
  not a missed diagnostic or killed helper.
- Source restored and checked byte-clean. The same test passed1/1 in1.62s,
  all20 reaped events/descriptor returns plus main/healthy progress.
- Restored native Linux testguest strict clippy passed0. No seven-target verdict
  follows from this result. The first matrix pass at b8 found two host-fixture
  lints (fallible helper stderr and needless borrowing), corrected in9caa5a030;
  the complete matrix rerun is pending.

Raw logs/statuses/exact mutation patch are retained in [reaping-runs](reaping-runs),
with SHA256 manifest. Source/compiler build lines demonstrate actual recompilation
at mutation/restoration; these root jobs use git checkout/restore in a private
clone, not source sync preserving stale mtimes. The observed times include
infrastructure/build differences and are not latency benchmarks.

Use run-orphan-reaping.sh in an isolated admitted checkout with fresh absolute
prefixes and a private DOCKER_CONFIG. Run green, actor-only, green; accept the
mutation only when its realZ state and the report failure occur. Preserve all
failed setup/observer attempts and rerun after source restoration.
