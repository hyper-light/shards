# Owned worker admission: coordinator evidence

Narrow admission correction; P1/P2/P7 and the full management plan remain open.

POLICY_READ: coordinator; main cbd39284937d024640c56db6f8d8ef2bd9b0ee24;
rules SHA256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Root implemented Programs/native readiness and owns final integration/evidence.
Peer dispatch, helper lifecycle and cgroup caps had separate scoped implementers.
Seven supported targets; macOS arm64 only. Paused WinARM remained untouched.

## Reproduced failures and correction

Actual RUN plus SCM_RIGHTS on the private helper socket allows worker creation.
At test-only b50a113622e4c595d62dfef5eee9340302a5cf3e, r743 set the internal
issued identity to u64::MAX: actual helper RUN carried ID0 and a descriptor,
worker_released=true, tracked=1, test101. This forced boundary is not a claim
that ordinary traffic performed 2^64 calls. The actor cannot select this counter.

6a0eccc4b reserves tracking, cancellation capacity, owned session and wire frame
fallibly before RUN, with checked ID/deadline and native socket setup.
PreparedProgram owns actual pre-release state. Issued IDs are consumed on native
send failure and never wrap/reuse. Existing RUN/STOP widths are stack-built.
After native release, insertion uses the already reserved tracking slot.

Two further live defects preceded 3cc9f6dbaf5f7226b509502a64b639c8d71c617e:
r762 at c1ec443d1 admitted a real O_PATH control despite failed nonblocking setup;
r763 at fefe3fbd3 observed one Rust heap request while formatting an actual EMFILE
refusal into a caller-owned buffer. The correction checks F_GETFL/F_SETFL,
preserves flags and retains typed operation/kind/raw OS code. Its Display does
not request an allocating OS error string. This is not whole-wire OOM safety.

## Primary sources and alternatives

Keep the existing helper/worker, transport and authoritative Programs owner.
A new manager, shell wrapper, cache or timeout guess does not close this seam.

- [Linux unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html) and
  [send(2)](https://man7.org/linux/man-pages/man2/send.2.html): native FD transfer,
  packet boundaries, nonblocking refusal and local acceptance.
- [Vec guarantees](https://doc.rust-lang.org/std/vec/struct.Vec.html#guarantees):
  capacity precedes mutation. Online docs say1.99.1; actual Rust1.98.1 identities
  are retained rather than relabelled as that documentation version.
- [fcntl(2)](https://man7.org/linux/man-pages/man2/fcntl.2.html): flag setup errors
  are not success.
- [cgroup-v2](https://docs.kernel.org/admin-guide/cgroup-v2.html): software
  reservations do not prove independent kernel charge or hard owner entitlement.
- [Existing broker research](../../managed-runtime-tool-broker.md) records the
  real caller, authority/evidence distinction and established alternatives.

Storage derives from actual payload/session bytes, owned live requests and the
existing u64/u32 wire fields. No benchmark number, guessed buffer or new runtime
timeout is introduced. Fixture buffers, IDs and runner/time budgets are inputs.

## Actual results and repeatable evidence

[drivers](drivers) contains exact submitted text. [transfers.json](transfers.json)
records archive/member hashes and verified safe regular-file transfers. Raw files
and failed attempts are preserved; extracted logs were not normalized. The
summary newline repair is separately recorded, not hidden in original bytes.

| Run/source | Actual result |
| --- | --- |
| r743 / b50 | Test101, actual exhausted counter released RUN ID0 plus FD. |
| r747 / e640e32f0 | Full server26/26, strict all-target Clippy0; each of four allocation refusals precedes RUN/FD. |
| r750 / 3b1967838b140466ff3367637541492a77be0459 | Checked-add removal:101 and actual reused-ID RUN/FD; exact restoration, fresh full26/26 pass. |
| r762 / c1ec443d1 | Actual unusable native control incorrectly admitted;101. |
| r763 / fefe3fbd3 | Actual EMFILE Display heap requests1;101. |
| r767 / 3cc9f6dba | Full server28/28 and strict all-target Clippy0; unusable control refused, Display requests0. |
| r773 / 3cc9f6dba | Ignoring only F_SETFL failure reproduces admission/101; restoring only allocating OS-error Display reproduces requests1/101. Exact source restored; full28/28 and strict Clippy0. |
| r746 / e640e32f0 | Seven-target lint101 on unused Windows test adapter; actual failure retained. |
| r749 / 3b196 and r766 / 3cc9 | Full format/seven-target/GPU0 and native ARM64 Mac managed remote15/15. |
| r753 / 3b196 | Native Windows API29/29, no ignored tests; failed initial PowerShell wrapper retained. |
| r760 / 3b196 | Linux API52 pass, six existing ignored; separately selected allocation-refusal case1/1. |
| r772 / 270bc46708fc700f3ca77f7de3b18ab4677c3926 | Combined broker/Programs full format/seven-target/GPU0, native Mac remote15/15; helper/cgroup added later. |
| r777 / f1a3e562796b4635359a4e19df3c8c2416339782 | Combined native server pass; actual carried musl init compile101 on signed/unsigned f_type comparison, before any VM. |
| r776 / same f1a3 | Seven-target gate101; not a passing gate. |
| 185aa85385fc69065241cb088f02b40753a82592 | Correct comparison by losslessly widening both libc fields; fresh combined gates required. |

The finite allocator sweep observes four sites: tracking, cancellation storage,
owned session and frame. Each refused attempt leaves no RUN/FD, tracked=0,
next=0. It does not prove every parser, HTTP/session, JSON or result allocation.
r773 mutations ran independently with exact original/mutant/restored hashes/diffs;
production mutations were never pushed. Every allocation-guard mutation is still
owed. The subsequent worker-result allocation-fault campaign was rejected by
automatic approval review; it is stopped, not silently retried or claimed passed.

## VM and scope limits

r751 built normal carried helpers on3b196 and ran the legacy code-mode VM test.
It failed101 at its old main-stderr rewriting assertion after the process-readiness
check passed. Raw native/kvm-legacy-red remains. This is not a passing VM gate or
proof that original main fails identically. Do not restore removed rewriting.

The new [broker-effects campaign](../managed-broker-effects/RESULTS.md) passed an
actual KVM case on original3b production: four private channels, actual worker
modes, gated recipient inbox, neighbor/main progress and visible cleanup. Root's
later broker/helper/cgroup combined source needs its own VM result.

[Helper ownership](../managed-helper-ownership/README.md) retains actual worker
completion after helper loss, the narrow guard correction, real guard-removal
effect and fresh restored cases. [Broker admission](../managed-broker-admission/README.md)
retains the actual peer-operation seam, typed results and its explicitly disclosed
draft-before-baseline chronology breach. [Borrowed composition](../managed-borrowed-combined/README.md)
retains API decisions and actual Mac results. Those results are revision-specific.
Native Windows zero-withheld semantics, final handle attribution, current cold Mac
allocation fault and the full supported VM matrix remain separate open gates.

## R01–R16 review

| Rule | Proof and remaining limit |
| --- | --- |
| R01 | Checked arithmetic/IDs, fallible pre-release storage, typed native errors, stack control and tested nonallocating Display; allocator cases and strict Clippy. Outer/transitive allocation and parser safety remain open. |
| R02 | Real caller, primary alternatives and actual before symptoms drove changes; no source theory labelled a reproduction. |
| R03 | Actual lengths/live counts/protocol widths derive storage; no selected runtime allowance. |
| R04 | PreparedProgram owns resources; typed admission/refusal/native failure and consumed issued IDs. Full lifecycle/generation is separate. |
| R05 | Existing single Programs/helper seam; no cache, extra thread, timer probe or duplicate manager. Existing readiness and protocol deadlines retained. |
| R06/R07 | Focused helpers; comments cover unsafe ownership/capacity/local acceptance. No private payload/token logging. |
| R08 | No4–5x or other performance claim; equivalent comparisons remain owed. |
| R09 | Boundary RUN/FD, setup and heap-request reds preceded production corrections. Forced internal exhaustion is disclosed. |
| R10 | Exact drivers, red/green/mutations/restoration, raw originals and hashes; broader fault/VM/charge proof still owed. |
| R11 | Exact seven-target compilation and native Linux/Windows/Mac facts above; full real-VM matrix unverified, Intel Mac unsupported. |
| R12 | Existing shared guest/platform seams, no installed binary, host-root/security/crypto weakening. |
| R13 | Complete pre-release state for this narrow seam; full management requirements remain required. |
| R14 | Finite allocator correctness instrumentation only; no allocation-free/leak-free/competitive claim. Parent/kernel cost, copies/faults and distributions remain root-owned. |
| R15 | Root implementation/audit/integration/heavy gates; scoped signed agent checkpoints, no agent main merges. Policy check is not evidence review. |
| R16 | Named isolated remote sources/targets and fresh headroom; preserved raw evidence, paused VM untouched, renewed user-authorized inactive build-cache cleanup. |

Still required: hard parent/kernel budgets, native executable authority, mandatory
route mediation, required durable pre-effect audit, real harness adapters and
credentials, generation/revocation/restore, adversarial coverage and comparable
performance. This record closes none of those by implication.

Raw files identified by raw-storage.json have a .gz suffix in the expanded view. gzip -dc recovers exact original bytes, including Windows CRLF/UTF encodings, compiler spaces and libtest final blank lines. Original archives and member manifests are unchanged; no whitespace exception or normalization was used.
