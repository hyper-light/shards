# Managed worker, peer broker and cap admission: combined gate

Coordinator-reviewed implementation batch; P0–P13/G01–G24 remain incomplete.
The accepted code/test source is0219ab2eb2466ee3e9b08064434dc9efd70053d5.
Later record-only changes must keep its production/test bytes exact.

POLICY_READ: coordinator; current policy maincbd39284937d024640c56db6f8d8ef2bd9b0ee24;
R01–R16 SHA256df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3.
Root implemented Programs/native error handling, audited/composed scoped agent
pieces, performed the combined gates and owns final main merge.

## Behavior and research

- [Worker admission](../managed-program-admission/README.md): checked IDs/deadlines,
  fallible retained state before native RUN/FD release, typed native setup causes.
- [Peer-effect broker](../managed-broker-admission/README.md): one real dispatcher
  for HTTP and sandbox callers, actual held grants/roots, reserved packet/result
  state before effects, checked IDs and typed local-acceptance/uncertainty metadata.
- [Helper ownership](../managed-helper-ownership/README.md): atomic pidfd creation,
  pre-instruction parent-death arming/check, signal/reap/cleanup uncertainty and
  pre-birth tracking/poll storage.
- [Domain caps](../managed-resource-admission/README.md): held controller objects,
  declared-cap/page-normalized readback and unused-group rollback before release.
  D59 caps/pressure/victim policy remains unchanged; these are not guarantees.
- [Borrowed API](../managed-borrowed-combined/README.md): reviewed finite/immediate
  deadline contract, fallible state and typed parser/process results.

Each linked record includes primary sources, established alternatives, literal
derivations, state/wait/cache review and before/after/remaining evidence. No new
host process manager, installed binary, shell wrapper, cache or timer probe was
introduced in this composition. The production mechanism is shared Linux guest
code across host backends; the API keeps existing platform seams.

At original f1a3e562796b4635359a4e19df3c8c2416339782, r777 passed native server
tests but normal carried-musl init build failed101: musl statfs.f_type is unsigned
while the libc magic constant is signed. r776 also failed the seven-target gate.
185aa85385fc69065241cb088f02b40753a82592 compares both by losslessly widening to
i128. This preserves both libc types without duplicate implementations, an
unchecked conversion or the compiler's suggested panic. Fresh gates below test
the correction; failed originals are retained.

## Actual combined acceptance

| Run / source0219 | Result |
| --- | --- |
| r778, ARM64 Macb24 | Full scripts/lint: format, all seven supported targets and GPU feature checks0; native managed remote15/15, no ignored cases. |
| r779, Fedora8f | Native full server36/36, normal carried helpers/static init/server/test build0, matching static testguest0. |
| r779 real KVM | Exactly one supplied-case test passed,0failed0ignored, no timeout/signal. Caller elapsed13.256s is an operational observation, not a benchmark. |
| r780, native broker mutation | Replace only checked peer ID addition with wrapping: test101 and actual recipient ID0. Exact source restored; full server36/36, strict all-target Clippy0. |

The KVM case has two runnable tenants and a files-only declaration. It verifies
four authenticated member/channel subscriptions, actual program success/error/
timeout, ineffective cross-session cancellation, own cancellation, abandonment
and subsequent success. An independently gated real recipient has an empty inbox
after the denied operation; receiver-side filtering remains a second guard, so
this is not a sender-only grant-mutation proof. Two main execs still make exact
stdout/stderr progress. A guest process snapshot observes two ready actors,
servers and sandboxes and zero program names. Owner stop and every observer join
succeed, host observer FDs8→8. A point snapshot is not complete kernel-charge
return or proof against escaped populations.

[linux-selected.json](linux-selected.json) is a decoded selection of original
receipts. Capture at the observation cut is947/0/488/0 payload bytes across the
four channels. No terminal is observed at that cut; empty stderr is not EOF or
future emptiness. Testguest reports are controlled test reports, not newly minted
supervisor execution receipts or required durable audit.

The actual normal test binary, carried shards and static testguest are frozen
under the remote after-0219/frozen directory. Selected hashes and compiler
association are in the original records and linux-selected.json. Retain those
files; normal target regeneration must not overwrite the frozen copies.

## Reproduce and original evidence

[drivers](drivers) contains the exact submitted drivers, including the failed
f1a3 attempts and the final mutation. Use a new output directory if repeating.
The normal build clears helper/init/VM/testguest overrides; no prebuilt stub
substitutes for carried helpers. The campaign's capture, case, call and cleanup
budgets are explicit test inputs, not production policy or benchmark thresholds.

Linux root scope: runner8fadc1ec213332e16cb0d915a3fb913f,
~/managed-capture-check with owned target, admitted4CPU/8GiB/16GiB for the combined
build/VM,2CPU/4GiB/8GiB for mutation. Mac root scope:
b24c5c951260dcdfd0972f2aeaf3cf51, ~/managed-combined-seven-target-check and
~/.cache/managed-combined-seven-target-b8fe, admitted4CPU/8GiB/8GiB and lockf.
Actual toolchain/kernel/storage facts and empty Docker configuration are retained.

[transfer-verification.json](transfer-verification.json) records archives:
Linux53353B SHA5335caabd11f8c6a38495c483909c4c76ce0df98782caf4cf5b1f4cd18438f55,
107 original members; Mac11169B SHAb6bc9251874f498e6d11397ebba7c0d26749e76ae86e176ad1a5195154356b5f,
39 originals. Every safe unique regular member's size/hash was verified locally.
Archives retain original logs without normalization; no executable/runtime cache
was included. Per-member lists are in linux-members.json and mac-members.json.
Source-specific signatures and scope receipts remain with their commits.

## Coordinator review and remaining work

The linked admission record accounts for every R01–R16 rule. Combined review adds
actual guest/caller and seven-target compatibility plus the independent broker
guard-removal effect/restoration. Source review preserves one authority/state
owner and original grants, isolates native platform differences, and avoids
new production panic calls or guessed limits. Transitive/outer OOM safety is not
certified by these tests. The broker agent's prototype-before-baseline chronology
violation remains explicitly disclosed; it is not retrospectively marked compliant.
The final checked path was re-evaluated against actual baseline data and the root
mutation/combined effects, rather than accepted from that prototype.

An old build.rs code-mode test still asserts removed main-stderr rewriting; its
actual failure is retained. A scoped follow-up is migrating its full original
assertions to the same canonical observer, preserving successful effects and all
error/cleanup checks. This known migration gap is not a passed workspace suite.

Hard hierarchical/parent/kernel entitlements, all-route buffered mediation,
native effect executors and real harness adapters/fallback closure, credentials,
required durable pre-effect audit, rich artifacts/display, generations/revocation/
restore/move and full adversarial/resource/VM sweeps remain required. MR-OOM-MAC-1,
Windows zero-withheld/final handle attribution and the paused ARM64 Windows VM
coverage remain unverified. No full product, code-complete, leak-free,
allocation-free, durability or4–5x performance claim is made.

Automatic approval review rejected the later helper malformed-control and
worker-result allocation-fault campaigns for possible cybersecurity risk. Those
campaigns are held; they were not retried under different wording or another
actor. Their missing tests remain explicit, while ordinary independent validation
and the other authorized implementation lanes continue.
