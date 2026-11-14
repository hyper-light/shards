# Platform findings 05–11: work owed to ROOT

Identity and binding policy receipt: [platform12.md](platform12.md). Branch
`p/fix-platform-v1`, base `9efde72524dd626f3ff84e6b9891852d7c810a9b`.
The fix brief requires Windows findings to run on jupiter and prohibits local Mac
builds/tests and implementer real-VM/benchmark campaigns. No production Windows code
was changed without its required red/green checks.

## Runner evidence and status

Explicit kiln `run(on: jupiter, sync: false)` probes r1 and r2 both ended without an
exit code: `no runner suits jupiter`, specifically `jupiter: offline`. Neither command
ran. A later runner inventory still reported jupiter offline. No Windows test is
reported passed, failed, skipped or refuted. ROOT's inbox also rejected policy/progress
sends with typed backpressure; the complete receipt is retained on disk.

Findings 05–10 remain **owed**, not fixed or refuted. These are the exact existing
module check commands to run in a prepared checkout on **jupiter** once available:

| Findings | Existing module command | Red regression still required before a fix |
|---|---|---|
| 05, 06 | `cargo test -p shards-vmm hv::whp::tests` | Check the emulator registry after a partial unmap/replacement, then a readonly memory callback. The current dirty-tracking test is not the emulator-registry regression. A real-partition red and guest string-instruction route belong to ROOT. |
| 07 | `cargo test -p shards-ipc windows::tests` | Check borrowed stdio inheritance flags before/after confined spawn; pause bootstrap creation and inspect a concurrent ordinary helper's inherited handles on the exact pinned Rust. |
| 08 | `cargo test -p shards-ipc slots::named::tests` | Use an unpaired-surrogate home with all instances busy, plus notification failure injection; prove bounded waiters and unchanged-path behavior. |
| 09 | `cargo test -p shards-vmm platform::windows_files::tests` | Precreate `state.~1`, read via `STATE`, and exercise collision handling without allowing an unbounded test hang. |
| 10 | `cargo test -p shards-vmm platform::windows_thread::tests` | Multiple raised actors share one affinity CPU beside a normal host actor; the existing one-spinner test does not establish aggregate fairness. |

These commands are **baseline/module checks**, not claims that the missing regression
already exists. The original report retains the minimal scenarios and test sketches.
ROOT must add/run each red before changing its guard, or return the lane once jupiter
is available. In particular, a zero-test filter or hardware SKIP is not verification.
After an implementation, run the same red and touched module plus
`cargo clippy -p shards-vmm --all-targets -- -D warnings` or
`cargo clippy -p shards-ipc --all-targets -- -D warnings` on jupiter as applicable.

## 2026-11-14: findings 05–10 taken over and run in the Windows 11 container

With jupiter offline, the takeover lane (branch `p/tk-platwin-v1`) wrote each red above and
ran it, the fixes and their guard mutations, in the Windows 11 x86_64 container on adacachy
(Rust 1.98.1, NTFS): 05–09 fixed, 10 fixed within a process with its cross-process owner
owed. Results, commands and what stays owed (the real-WHP guest routes of 05/06, 09's
restore cost, 10's host-wide owner and measurement): [PM M577](../../platform-measurements.md),
D140, D191, D195, D196. The table above stays as the record of what was held.

## platform-11: online-value design owed, with cold starts

No fixed value has been replaced by another fixed value. The outstanding design must
cover these separate costs and authority boundaries; it is not an implemented or
performance-validated decision:

1. **macOS thread policy.** Keep threads at timeshare in a typed `Unmeasured` state.
   Record actual thread CPU cost of completed device batches and actual timer/wake
   delays in the running build. A batch's known remaining work and an explicit guest
   timer deadline supply the computation/deadline inputs; observed cost and clock
   conversion must fit the Mach API's documented integer/interval bounds. Where there
   is no measured cost or justified deadline, remain at timeshare with a typed reason,
   rather than inventing a cold 0.5/1 ms pair. Promotion must have an aggregate authority
   limit before any learning is allowed to raise priority. Needed: research/measure how
   to admit device work without an explicit timer deadline while preserving goal10's
   throughput. Until that is established, no automatic promotion is claimed designed.
2. **Windows governor.** Measure positive GetThreadTimes increments and actual wake
   overshoot on the running host; derive the next observation from the earliest credit
   exhaustion under the admitted aggregate CPU budget and that observed resolution.
   Cold or unavailable observations mean normal priority with typed `Unmeasured` or
   `ClockUnavailable`, not a baked 100 ms period. Per-thread half-CPU credits alone
   cannot establish host fairness (finding10). A host/affinity-wide admission budget
   must include other VM processes and daemons; integrating that owner without adding
   a parallel scheduler remains an architectural decision requiring ROOT's R02 data.
3. **Pipe quotas.** The caller must supply capacity from the flow's actual admitted wire
   window/known first transfer and available run budget, with a fallible typed admission.
   Cold flow setup can use only its known framing requirement; no assumed steady-state
   throughput quota. Query the kernel's actual capacity (GetNamedPipeInfo) after creation
   and learn observed completed transfer sizes/backpressure for subsequent connections,
   bounded by admitted memory. A zero-size API argument merely selecting an upstream
   default is not by itself the required derivation. Needed: enumerate each real caller's
   wire window and bootstrap requirement, then validate this method's cold behavior and
   resource accounting. No arbitrary floor, retry count, multiplier or sample count is
   proposed as a substitute.

Typed transitions are `Unmeasured -> Observed` only on successful completed measurements,
`Observed -> Unmeasured` when assumptions/resources change, and explicit refusal for
missing authority or unavailable measurements. Observations never enlarge network,
handle, filesystem or CPU authority. Uncertain/overflowed estimates cannot promote a
thread or allocate a larger quota. No persistence cache is proposed.

ROOT owes primary-source/API compatibility research, real cold and contended host
measurements, neighbor fairness, throughput, CPU/allocations/faults and cross-platform
validation for this design. Existing M8/M10/M294/M317/M340 support investigating those
methods, not baking their measured numbers into the next build. This explicitly leaves
platform-11 open under the fix brief's "owed with its design" rule.

## Other obligations

No P0 edits: platform01/02/04 remain with Claude's Linux confinement lane and 03 with
Claude's Windows staging lane. No additional review was started. The policy check,
Linux fixture red/green and all Windows/VM/performance limitations remain distinct;
successful platform12 tests do not cover any of the held findings above.
